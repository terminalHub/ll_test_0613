//! Limitless 对冲系统入口
//!
//! 运行：cargo run -- <配置文件路径>

use std::collections::HashMap;

use tokio::sync::broadcast;
use tracing::{error, info, warn};

use limitless_0613::config::AppConfig;
use limitless_0613::discovery::market_discovery::MarketDiscovery;
use limitless_0613::domain::event::{PriceEvent, StatEvent};
use limitless_0613::domain::market::MarketInfo;
use limitless_0613::domain::order::{AccountId, OrderKind, OrderResult};
use limitless_0613::executor::OrderExecutor;
use limitless_0613::infrastructure::limitless::LimitlessApi;
use limitless_0613::monitor::ws_client::WsClient;
use limitless_0613::stats::collector::StatsCollector;
use limitless_0613::strategy::directional::DefaultDirectionalStrategy;
use limitless_0613::strategy::Strategy;
use limitless_0613::utils::onchain::BaseChainClient;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // ── 加载配置 ──
    let config_path = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "config/limitless.toml".to_string());
    let cfg = AppConfig::load_from_file(&config_path)?;
    init_logging(&cfg);

    info!("Limitless 对冲系统启动");

    // ── 创建组件 ──
    let (api_a, api_b) = create_apis(&cfg).await?;

    // 强制双号检查
    if api_b.is_none() {
        anyhow::bail!("必须配置双号（account_a + account_b）才能运行策略");
    }

    let executor = create_executor(api_a, api_b, &cfg);
    let wallet_state = startup_wallet_check(&cfg, &executor).await?;
    let mut discovery = MarketDiscovery::new(cfg.hedge.clone());
    let mut ws = WsClient::new(None);
    let (stat_tx, stat_rx) = broadcast::channel::<StatEvent>(1024);
    let mut strategy = create_strategy(&cfg, &wallet_state).await?;

    // 连接 SDK WebSocket
    let sdk_ws = executor.sdk_client().new_websocket_client(None);
    ws.connect(sdk_ws).await?;

    let mut price_rx = ws.subscribe();

    info!("系统初始化完成，进入主循环");

    // ── 主循环 ──
    let result = main_loop(
        &cfg,
        &executor,
        &wallet_state,
        &mut discovery,
        &mut ws,
        &mut strategy,
        &mut price_rx,
        &stat_tx,
    )
    .await;

    // ── 统计 ──
    let mut stats = StatsCollector::new(stat_rx);
    stats.poll();
    let summary = stats.summary();
    info!(
        total_trades = summary.total_trades,
        win_rate = format!("{:.1}%", summary.win_rate * 100.0),
        total_pnl = format!("{:.2} USDC", summary.total_pnl),
        order_failures = summary.order_failures,
        total_redeemed = format!("{:.2} USDC", summary.total_redeemed),
        "统计摘要"
    );

    result
}

#[derive(Debug, Clone)]
struct WalletRuntimeState {
    wallet_a: String,
    wallet_b: String,
    usdc_a: f64,
    usdc_b: f64,
    initial_total_usdc: f64,
    big_wallet: AccountId,
    small_wallet: AccountId,
}

fn init_logging(cfg: &AppConfig) {
    let env_filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new(&cfg.logging.level));
    tracing_subscriber::fmt()
        .with_env_filter(env_filter)
        .with_target(false)
        .init();
}

async fn create_apis(cfg: &AppConfig) -> anyhow::Result<(LimitlessApi, Option<LimitlessApi>)> {
    let api_a = LimitlessApi::new(cfg.account_a.clone())?;
    let wallet = api_a.verify_auth().await?;
    info!(address = %wallet, "A 号登录成功");

    let api_b = if !cfg.account_b.private_key.is_empty() || !cfg.account_b.hmac_token_id.is_empty()
    {
        let api = LimitlessApi::new(cfg.account_b.clone())?;
        match api.verify_auth().await {
            Ok(addr) => {
                info!(address = %addr, "B 号登录成功");
                Some(api)
            }
            Err(e) => {
                warn!(error = %e, "B 号登录失败");
                None
            }
        }
    } else {
        None
    };

    Ok((api_a, api_b))
}

fn create_executor(
    api_a: LimitlessApi,
    api_b: Option<LimitlessApi>,
    cfg: &AppConfig,
) -> OrderExecutor {
    match api_b {
        Some(b) => OrderExecutor::new(api_a, b, &cfg.rpc_url),
        None => OrderExecutor::from_single_api(api_a, &cfg.rpc_url),
    }
}

async fn startup_wallet_check(
    cfg: &AppConfig,
    executor: &OrderExecutor,
) -> anyhow::Result<WalletRuntimeState> {
    let strategy_config = cfg.strategy.clone().unwrap_or_default();
    let onchain = BaseChainClient::new(Some(&cfg.rpc_url));

    let a = executor.api_a();
    let wallet_a = a.verify_auth().await?;
    let usdc_a = onchain.get_usdc_balance(&wallet_a).await?;
    let eth_a = onchain.get_eth_balance(&wallet_a).await?;

    let b = executor
        .api_b()
        .ok_or_else(|| anyhow::anyhow!("必须配置 B 号"))?;
    let wallet_b = b.verify_auth().await?;
    let usdc_b = onchain.get_usdc_balance(&wallet_b).await?;
    let eth_b = onchain.get_eth_balance(&wallet_b).await?;

    info!(
        wallet_a = %wallet_a,
        usdc_a = %usdc_a,
        eth_a = %eth_a,
        wallet_b = %wallet_b,
        usdc_b = %usdc_b,
        eth_b = %eth_b,
        min_eth_balance = %strategy_config.min_eth_balance,
        "启动前钱包余额检查"
    );

    if eth_a < strategy_config.min_eth_balance {
        anyhow::bail!(
            "A 号 Base ETH 不足：当前 {}，最低要求 {}",
            eth_a,
            strategy_config.min_eth_balance
        );
    }
    if eth_b < strategy_config.min_eth_balance {
        anyhow::bail!(
            "B 号 Base ETH 不足：当前 {}，最低要求 {}",
            eth_b,
            strategy_config.min_eth_balance
        );
    }

    let (big_wallet, small_wallet) = if usdc_a >= usdc_b {
        (AccountId::A, AccountId::B)
    } else {
        (AccountId::B, AccountId::A)
    };
    let initial_total_usdc = usdc_a + usdc_b;

    info!(
        initial_total_usdc = %initial_total_usdc,
        big_wallet = ?big_wallet,
        small_wallet = ?small_wallet,
        "启动资金基线已记录"
    );

    Ok(WalletRuntimeState {
        wallet_a,
        wallet_b,
        usdc_a,
        usdc_b,
        initial_total_usdc,
        big_wallet,
        small_wallet,
    })
}

async fn create_strategy(
    cfg: &AppConfig,
    wallet_state: &WalletRuntimeState,
) -> anyhow::Result<Box<dyn Strategy>> {
    let strategy_config = cfg.strategy.clone().unwrap_or_default();
    let mut strategy = DefaultDirectionalStrategy::new(strategy_config);

    info!(balance_a = %wallet_state.usdc_a, balance_b = %wallet_state.usdc_b, "USDC 余额");
    strategy.init_wallets(wallet_state.usdc_a, wallet_state.usdc_b);

    Ok(Box::new(strategy))
}

async fn handle_post_redeem_funding_and_risk(
    cfg: &AppConfig,
    executor: &OrderExecutor,
    wallet_state: &WalletRuntimeState,
    results: &[OrderResult],
    trading_halted: &mut bool,
) {
    let strategy_config = cfg.strategy.clone().unwrap_or_default();
    let mut redeem_success_by_market: HashMap<String, Vec<AccountId>> = HashMap::new();

    for result in results {
        if result.request.order_kind == OrderKind::Redeem && result.success {
            redeem_success_by_market
                .entry(result.request.market_slug.clone())
                .or_default()
                .push(result.request.account);
        }
    }

    for (slug, accounts) in redeem_success_by_market {
        let has_a = accounts.contains(&AccountId::A);
        let has_b = accounts.contains(&AccountId::B);
        if !has_a || !has_b {
            continue;
        }

        info!(
            slug = %slug,
            from = ?wallet_state.big_wallet,
            to = ?wallet_state.small_wallet,
            amount_usdc = %strategy_config.post_redeem_transfer_usdc,
            "双账号 redeem 成功，准备资金回补"
        );

        if let Err(e) = executor
            .transfer_usdc_between_accounts(
                wallet_state.big_wallet,
                wallet_state.small_wallet,
                strategy_config.post_redeem_transfer_usdc,
            )
            .await
        {
            error!(slug = %slug, error = %e, "资金回补转账失败");
        }

        check_loss_guard(cfg, wallet_state, trading_halted).await;
    }
}

async fn check_loss_guard(
    cfg: &AppConfig,
    wallet_state: &WalletRuntimeState,
    trading_halted: &mut bool,
) {
    if *trading_halted {
        return;
    }

    let strategy_config = cfg.strategy.clone().unwrap_or_default();
    let onchain = BaseChainClient::new(Some(&cfg.rpc_url));

    let current_a = match onchain.get_usdc_balance(&wallet_state.wallet_a).await {
        Ok(balance) => balance,
        Err(e) => {
            error!(error = %e, "查询 A 号 USDC 余额失败，跳过亏损检查");
            return;
        }
    };
    let current_b = match onchain.get_usdc_balance(&wallet_state.wallet_b).await {
        Ok(balance) => balance,
        Err(e) => {
            error!(error = %e, "查询 B 号 USDC 余额失败，跳过亏损检查");
            return;
        }
    };

    let current_total_usdc = current_a + current_b;
    let initial_total_usdc = wallet_state.initial_total_usdc;
    if initial_total_usdc <= 0.0 {
        warn!("初始 USDC 总余额为 0，跳过亏损检查");
        return;
    }

    let loss_pct = ((initial_total_usdc - current_total_usdc) / initial_total_usdc).max(0.0);
    info!(
        initial_total_usdc = %initial_total_usdc,
        current_total_usdc = %current_total_usdc,
        loss_pct = format!("{:.2}%", loss_pct * 100.0),
        max_loss_pct = format!("{:.2}%", strategy_config.max_loss_pct * 100.0),
        "资金检查"
    );

    if loss_pct > strategy_config.max_loss_pct {
        *trading_halted = true;
        error!(
            loss_pct = format!("{:.2}%", loss_pct * 100.0),
            max_loss_pct = format!("{:.2}%", strategy_config.max_loss_pct * 100.0),
            "资金亏损超过阈值，停止新交易，请操作员检查"
        );
    }
}

async fn main_loop(
    cfg: &AppConfig,
    executor: &OrderExecutor,
    wallet_state: &WalletRuntimeState,
    discovery: &mut MarketDiscovery,
    ws: &mut WsClient,
    strategy: &mut Box<dyn Strategy>,
    price_rx: &mut broadcast::Receiver<PriceEvent>,
    stat_tx: &broadcast::Sender<StatEvent>,
) -> anyhow::Result<()> {
    let mut discovery_timer =
        tokio::time::interval(std::time::Duration::from_secs(cfg.hedge.scan_interval_secs));
    let mut tick_timer = tokio::time::interval(std::time::Duration::from_secs(1));
    let mut trading_halted = false;

    let mut market_cache: HashMap<String, MarketInfo> = HashMap::new();

    loop {
        tokio::select! {
            // ── 市场发现（30-60s）──
            _ = discovery_timer.tick() => {
                match discovery.scan(executor.sdk_client()).await {
                    Ok(result) => {
                        let _ = stat_tx.send(StatEvent::ScanComplete {
                            markets_found: result.all_markets.len(),
                        });

                        if !result.new_markets.is_empty() {
                            info!(count = result.new_markets.len(), "发现新市场");
                            strategy.on_market_discovered(&result.new_markets);
                        }

                        let add_slugs: Vec<String> = result.new_markets.iter().map(|m| m.slug.clone()).collect();
                        let diff = ws.update_subscriptions(&add_slugs, &result.removed_slugs).await;

                        for info in &result.new_markets {
                            market_cache.insert(info.slug.clone(), info.clone());
                        }
                        for info in result.all_markets {
                            market_cache.entry(info.slug.clone()).or_insert(info);
                        }
                        for slug in &result.removed_slugs {
                            market_cache.remove(slug);
                        }

                        info!(
                            subscribe = diff.to_subscribe.len(),
                            unsubscribe = diff.to_unsubscribe.len(),
                            total = ws.subscribed_slugs().len(),
                            "WS 订阅更新"
                        );
                    }
                    Err(e) => {
                        error!(error = %e, "市场扫描失败");
                    }
                }
            }

            // ── WS 价格事件 ──
            Ok(event) = price_rx.recv() => {
                if trading_halted {
                    continue;
                }

                let orders = strategy.on_event(&event).await;
                if orders.is_empty() {
                    continue;
                }

                info!(count = orders.len(), "策略产出下单请求");

                // 逐笔执行：GTC 失败则跳过 FOK
                let mut results: Vec<limitless_0613::domain::order::OrderResult> = Vec::new();
                for (i, order) in orders.iter().enumerate() {
                    if i > 0 {
                        tokio::time::sleep(std::time::Duration::from_millis(cfg.hedge.request_delay_ms)).await;
                        if !results.last().map_or(true, |r| r.success) {
                            info!("前一笔订单失败，跳过后续订单");
                            break;
                        }
                    }
                    results.push(executor.execute(order).await);
                }

                // 通知策略：FOK 失败 → 先撤大单，再通知策略移除市场
                for result in &results {
                    if !result.success
                        && result.request.order_kind == OrderKind::Fok
                    {
                        if let Some(big_account) = strategy.big_wallet_for_market(&result.request.market_slug) {
                            warn!(slug = %result.request.market_slug, "小单失败，撤大单");
                            if let Err(e) = executor.cancel_all(big_account, &result.request.market_slug).await {
                                error!(error = %e, "撤大单失败");
                            }
                        }
                    }

                    strategy.on_order_result(result).await;

                    if !result.success {
                        let _ = stat_tx.send(StatEvent::OrderFailed {
                            slug: result.request.market_slug.clone(),
                            error: result.error.clone().unwrap_or_default(),
                        });
                    }
                }
            }

            // ── 心跳（1秒）→ 检查赎回 ──
            _ = tick_timer.tick() => {
                let orders = strategy.on_tick().await;
                if !orders.is_empty() {
                    info!(count = orders.len(), "心跳产出赎回请求");
                    let results = executor.execute_batch(&orders, cfg.hedge.request_delay_ms).await;
                    handle_post_redeem_funding_and_risk(
                        cfg,
                        executor,
                        wallet_state,
                        &results,
                        &mut trading_halted,
                    ).await;
                    for result in &results {
                        // 赎回结果反馈给策略
                        strategy.on_order_result(result).await;
                    }
                }
            }

            // ── Ctrl+C ──
            _ = tokio::signal::ctrl_c() => {
                info!("收到 Ctrl+C，优雅退出");
                break;
            }
        }
    }

    ws.disconnect().await;
    info!("主循环已退出");
    Ok(())
}
