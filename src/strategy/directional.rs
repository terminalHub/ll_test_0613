use std::collections::HashMap;

use async_trait::async_trait;
use chrono::Utc;
use tracing::{info, warn};

use crate::domain::config::StrategyConfig;
use crate::domain::event::PriceEvent;
use crate::domain::market::MarketInfo;
use crate::domain::order::{AccountId, OrderKind, OrderRequest, OrderResult, Side};
use crate::strategy::Strategy;

// ═══════════════════════════════════════════════════════════════════════════════
// 市场状态机
// ═══════════════════════════════════════════════════════════════════════════════

/// 市场生命周期阶段
///
/// 状态转移：
/// ```text
/// PendingEntry ──(GTC 成功)──▶ BigPlaced ──(FOK 成功)──▶ Hedged
///       │                         │                          │
///       ├──(GTC 失败)──▶ 移除    ├──(FOK 失败)──▶ 撤大单──▶ 移除
///       │                                                   │
///       └──(超时)──▶ 移除                          (到期+delay)
///                                                         │
///                                                    Redeeming
///                                                         │
///                                               (赎回成功)──▶ 移除
/// ```
#[derive(Debug, Clone, PartialEq)]
enum MarketPhase {
    /// 已产出 GTC+FOK 订单，等待 executor 结果
    PendingEntry,
    /// GTC 大单挂单成功，等待 FOK 小单结果
    BigPlaced,
    /// 两笔都成功，对冲完成，等待结算+赎回
    Hedged,
    /// 赎回已发起，等待链上确认
    Redeeming,
}

/// 单个活跃市场的状态
#[derive(Debug, Clone)]
pub struct MarketState {
    phase: MarketPhase,
    /// GTC 订单 ID（撤单用）
    big_order_id: Option<String>,
    /// 入场时间戳
    entry_time: i64,
}

/// Oracle 价格快照
#[derive(Debug, Clone, Copy)]
struct OracleSnapshot {
    price: f64,
    timestamp: i64,
}

// ═══════════════════════════════════════════════════════════════════════════════
// 方向性对冲策略
// ═══════════════════════════════════════════════════════════════════════════════

/// 方向性对冲策略
///
/// 核心目的：刷交易量博空投代币，不追求价差利润。
///
/// 对冲逻辑：
/// 1. 监听 Orderbook 事件，筛选 best_ask ∈ ask_range 且 spread > min_spread
/// 2. 通过 OraclePrice 判断方向（current >= initial → YES 赢）
/// 3. 余额多的钱包做大单（GTC 挂赢方 @ best_ask - price_tick），挂在 spread 内不成交
/// 4. 余额少的钱包做小单（FOK 吃输方，USDC = contract_count × (1 - gtc_price)）
/// 5. 结算后赎回两个钱包
///
/// 资金保护机制：
/// - GTC buy @ best_ask - price_tick → 价格在 spread 内部，不立刻成交
/// - best_ask ∈ ask_range → 方向判断准确率 > 99%
/// - 排除 XRP/DOGE → 减少 bot 干扰和黑天鹅风险
/// - 小单失败 → 立刻撤大单 → 避免单边暴露
/// - 小单 USDC = contract_count × (1 - gtc_price) → 两边合约数对齐，赎回时约等于完整价值
pub struct DefaultDirectionalStrategy {
    config: StrategyConfig,
    /// 活跃市场状态机：slug → MarketState
    active_markets: HashMap<String, MarketState>,
    /// 市场信息缓存：slug → MarketInfo
    pub market_info: HashMap<String, MarketInfo>,
    /// Oracle 价格缓存：slug → price + timestamp
    oracle_prices: HashMap<String, OracleSnapshot>,
    /// 大单钱包账号（余额多的）
    pub big_wallet: AccountId,
    /// 小单钱包账号（余额少的）
    pub small_wallet: AccountId,
}

impl DefaultDirectionalStrategy {
    pub fn new(config: StrategyConfig) -> Self {
        Self {
            config,
            active_markets: HashMap::new(),
            market_info: HashMap::new(),
            oracle_prices: HashMap::new(),
            big_wallet: AccountId::A,
            small_wallet: AccountId::B,
        }
    }

    fn is_excluded(&self, ticker: &str) -> bool {
        self.config
            .excluded_tickers
            .iter()
            .any(|t| t.eq_ignore_ascii_case(ticker))
    }

    /// GTC 大单挂单价
    ///
    /// - 配置了 gtc_price_override：直接用配置值（测试模式，降低成本）
    /// - 未配置：用 best_ask - price_tick（生产模式）
    pub fn gtc_price(&self, best_ask: f64) -> f64 {
        if let Some(override_price) = self.config.gtc_price_override {
            override_price.max(0.001)
        } else {
            (best_ask - self.config.price_tick).max(0.001)
        }
    }

    /// FOK 小单 USDC 金额 = contract_count × (1 - gtc_price)
    ///
    /// 对冲数学：大单买 contract_count 个赢方 @ gtc_price，
    /// 小单花 fok_amount USDC 买输方。两边合约数对齐，
    /// 结算后 1 赢方 + 1 输方 = 1 USDC 赎回价值。
    /// 总花费 = contract_count × gtc_price + contract_count × (1 - gtc_price)
    ///        = contract_count × 1 = contract_count USDC。
    pub fn fok_amount(&self, gtc_price: f64) -> f64 {
        let amount = self.config.contract_count * (1.0 - gtc_price);
        // 最小金额保护：至少 0.001 USDC
        amount.max(0.001)
    }

    /// 处理 Orderbook 快照 — 策略的核心决策方法
    async fn handle_orderbook(
        &mut self,
        slug: &str,
        best_bid: f64,
        best_ask: f64,
    ) -> Vec<OrderRequest> {
        // 1. 前置检查：已在活跃市场中 → 跳过
        if self.active_markets.contains_key(slug) {
            return Vec::new();
        }

        let info = match self.market_info.get(slug) {
            Some(i) => i,
            None => return Vec::new(),
        };

        if self.is_excluded(&info.ticker) {
            return Vec::new();
        }

        // 2. ask 价格区间检查（方向确定性）
        let ask_min = self.config.ask_range.first().copied().unwrap_or(0.989);
        let ask_max = self.config.ask_range.get(1).copied().unwrap_or(1.0);
        if best_ask < ask_min || best_ask > ask_max {
            return Vec::new();
        }

        // 3. spread 检查（有挂单空间）
        let spread = best_ask - best_bid;
        if spread <= self.config.min_spread {
            return Vec::new();
        }

        // 4. 入场时间窗口（扫描间隔内）
        let remaining = info.expiration_ts - Utc::now().timestamp();
        let range_min = *self.config.settle_time_range.first().unwrap_or(&3) as i64;
        let range_max = *self.config.settle_time_range.get(1).unwrap_or(&300) as i64;
        if remaining < range_min || remaining > range_max {
            return Vec::new();
        }

        // 5. 并发检查：有非 Redeeming 的活跃市场时跳过（同时只入场一个）
        if self.has_active_hedge() {
            return Vec::new();
        }

        // 6. 判断方向（必须使用新鲜 oracle；测试模式可 fallback）
        let now = Utc::now().timestamp();
        let oracle = self.oracle_prices.get(slug).copied();
        let fresh_oracle = oracle.filter(|snapshot| {
            now.saturating_sub(snapshot.timestamp) <= self.config.oracle_max_age_secs as i64
        });
        let initial = info.open_price;
        let mut oracle_price_for_log: Option<f64> = None;
        let mut open_price_for_log: Option<f64> = None;
        let mut oracle_move_pct_for_log: Option<f64> = None;

        let (winning_token, losing_token) = match (fresh_oracle, initial) {
            (Some(snapshot), Some(init)) => {
                if init <= 0.0 {
                    return Vec::new();
                }

                let oracle_move_pct = (snapshot.price - init) / init;
                let oracle_move_abs_pct = oracle_move_pct.abs();
                oracle_price_for_log = Some(snapshot.price);
                open_price_for_log = Some(init);
                oracle_move_pct_for_log = Some(oracle_move_pct);

                if oracle_move_abs_pct < self.config.min_oracle_move_pct {
                    return Vec::new();
                }

                if oracle_move_pct >= 0.0 {
                    (info.yes_token_id.clone(), info.no_token_id.clone())
                } else {
                    (info.no_token_id.clone(), info.yes_token_id.clone())
                }
            }
            _ if self.config.require_fresh_oracle => {
                return Vec::new();
            }
            _ => {
                if best_ask > 0.5 {
                    (info.yes_token_id.clone(), info.no_token_id.clone())
                } else {
                    (info.no_token_id.clone(), info.yes_token_id.clone())
                }
            }
        };

        // 7. 计算下单参数
        let gtc_price = self.gtc_price(best_ask);
        let fok_amount = self.fok_amount(gtc_price);

        // 8. 插入 PendingEntry 状态
        self.active_markets.insert(
            slug.to_string(),
            MarketState {
                phase: MarketPhase::PendingEntry,
                big_order_id: None,
                entry_time: Utc::now().timestamp(),
            },
        );

        info!(
            slug = %slug,
            ticker = %info.ticker,
            best_ask = %best_ask,
            best_bid = %best_bid,
            spread = %spread,
            oracle_price = ?oracle_price_for_log,
            open_price = ?open_price_for_log,
            oracle_move_pct = ?oracle_move_pct_for_log.map(|v| format!("{:.4}%", v * 100.0)),
            min_oracle_move_pct = format!("{:.4}%", self.config.min_oracle_move_pct * 100.0),
            gtc_price = %gtc_price,
            fok_amount = %fok_amount,
            contract_count = %self.config.contract_count,
            big_wallet = ?self.big_wallet,
            small_wallet = ?self.small_wallet,
            "策略入场"
        );

        // 9. 返回 [GTC 大单, FOK 小单]（先大后小）
        vec![
            // 大单：GTC 挂赢方 @ best_ask - price_tick
            OrderRequest {
                account: self.big_wallet,
                market_slug: slug.to_string(),
                token_id: winning_token,
                side: Side::Buy,
                order_kind: OrderKind::Gtc,
                price_or_amount: gtc_price,
                size: Some(self.config.contract_count),
                condition_id: None,
            },
            // 小单：FOK 吃输方，USDC = contract_count × (1 - gtc_price)
            OrderRequest {
                account: self.small_wallet,
                market_slug: slug.to_string(),
                token_id: losing_token,
                side: Side::Buy,
                order_kind: OrderKind::Fok,
                price_or_amount: fok_amount,
                size: None,
                condition_id: None,
            },
        ]
    }

    /// 从 active_markets 移除指定市场（清理并日志）
    fn remove_market(&mut self, slug: &str, reason: &str) {
        if self.active_markets.remove(slug).is_some() {
            info!(slug = %slug, reason = %reason, "市场已从活跃状态移除");
        }
    }

    /// 是否有非 Redeeming 的活跃市场（正在对冲中）
    fn has_active_hedge(&self) -> bool {
        self.active_markets.values().any(|s| s.phase != MarketPhase::Redeeming)
    }

    /// 清空所有缓存，为下一批市场做准备
    ///
    /// 当所有市场都赎回完成后调用，清空 active_markets、market_info、oracle_prices。
    /// 下一次 discovery 会重新拉取新一批市场。
    pub fn reset_for_next_cycle(&mut self) {
        let count = self.active_markets.len();
        self.active_markets.clear();
        self.market_info.clear();
        self.oracle_prices.clear();
        info!(cleared_markets = count, "缓存已清空，准备拉取下一批市场");
    }
}

#[async_trait]
impl Strategy for DefaultDirectionalStrategy {
    fn name(&self) -> &str {
        &self.config.name
    }

    async fn on_event(&mut self, event: &PriceEvent) -> Vec<OrderRequest> {
        match event {
            PriceEvent::Orderbook { slug, bid, ask, .. } => {
                self.handle_orderbook(slug, *bid, *ask).await
            }
            PriceEvent::OraclePrice { slug, price, timestamp } => {
                self.oracle_prices.insert(
                    slug.clone(),
                    OracleSnapshot {
                        price: *price,
                        timestamp: *timestamp,
                    },
                );
                Vec::new()
            }
        }
    }

    async fn on_order_result(&mut self, result: &OrderResult) {
        let slug = &result.request.market_slug;

        let state = match self.active_markets.get_mut(slug) {
            Some(s) => s,
            None => return,
        };

        match (&state.phase, result.request.order_kind, result.success) {
            // GTC 大单成功 → 推进到 BigPlaced
            (MarketPhase::PendingEntry, OrderKind::Gtc, true) => {
                state.phase = MarketPhase::BigPlaced;
                state.big_order_id = result.order_id.clone();
                info!(slug = %slug, order_id = ?result.order_id, "大单挂单成功，等待小单");
            }
            // GTC 大单失败 → 移除（允许重试）
            (MarketPhase::PendingEntry, OrderKind::Gtc, false) => {
                warn!(slug = %slug, error = ?result.error, "大单挂单失败，移除市场");
                self.remove_market(slug, "大单挂单失败");
            }
            // FOK 小单成功 → 对冲完成，推进到 Hedged
            (MarketPhase::PendingEntry | MarketPhase::BigPlaced, OrderKind::Fok, true) => {
                state.phase = MarketPhase::Hedged;
                info!(slug = %slug, "小单执行成功，对冲完成，等待结算");
            }
            // FOK 小单失败 → 撤大单，移除（main loop 会调用撤单）
            (MarketPhase::PendingEntry | MarketPhase::BigPlaced, OrderKind::Fok, false) => {
                warn!(slug = %slug, error = ?result.error, "小单执行失败，触发撤大单");
                self.remove_market(slug, "小单失败撤单");
            }
            // 赎回成功 → 移除市场，生命周期完成
            (_, OrderKind::Redeem, true) => {
                self.remove_market(slug, "赎回成功");
                // 所有市场都赎回完成 → 清空缓存，准备下一批
                if self.active_markets.is_empty() {
                    self.reset_for_next_cycle();
                }
            }
            // 赎回失败 → 保持 Redeeming，on_tick 下秒重试
            (_, OrderKind::Redeem, false) => {
                warn!(slug = %slug, error = ?result.error, "赎回失败，下秒重试");
                state.phase = MarketPhase::Hedged; // 退回 Hedged 等 on_tick 重试
            }
            _ => {
                info!(slug = %slug, phase = ?state.phase, success = result.success, "收到订单结果（忽略）");
            }
        }
    }

    async fn on_tick(&mut self) -> Vec<OrderRequest> {
        let mut orders = Vec::new();
        let now = Utc::now().timestamp();
        let delay = self.config.settle_redeem_delay_secs as i64;

        // 收集需要修改的 slug（避免在遍历中借用冲突）
        let mut to_redeem = Vec::new();   // Hedged → Redeeming + 产出赎回
        let mut to_remove = Vec::new();   // 超时 → 移除

        for (slug, state) in &self.active_markets {
            let info = match self.market_info.get(slug) {
                Some(i) => i,
                None => continue,
            };
            let remaining = info.expiration_ts - now;

            match state.phase {
                // Hedged：已对冲完成，等待结算后赎回
                MarketPhase::Hedged => {
                    let has_condition_id = info.condition_id.is_some();
                    let redeem_after_secs = (remaining + delay + 1).max(0);

                    if remaining <= 0 && remaining >= -delay {
                        info!(
                            slug = %slug,
                            remaining = %remaining,
                            delay = %delay,
                            redeem_after_secs = %redeem_after_secs,
                            has_condition_id = has_condition_id,
                            "市场已过期，等待 redeem 延迟"
                        );
                    }

                    if remaining < -delay {
                        if let Some(condition_id) = &info.condition_id {
                            info!(
                                slug = %slug,
                                remaining = %remaining,
                                delay = %delay,
                                condition_id = %condition_id,
                                "触发赎回"
                            );
                            to_redeem.push(slug.clone());
                        } else {
                            warn!(
                                slug = %slug,
                                remaining = %remaining,
                                delay = %delay,
                                "市场已过期但缺少 condition_id，无法赎回"
                            );
                        }
                    }
                    // 超时兜底：入场超过 10 分钟仍为 Hedged
                    if now - state.entry_time > 600 {
                        warn!(slug = %slug, "入场超过 10 分钟，强制移除");
                        to_remove.push(slug.clone());
                    }
                }
                // PendingEntry / BigPlaced：入场过程中，检查超时
                MarketPhase::PendingEntry | MarketPhase::BigPlaced => {
                    if now - state.entry_time > 120 {
                        warn!(slug = %slug, phase = ?state.phase, "入场超时（2分钟），强制移除");
                        to_remove.push(slug.clone());
                    }
                }
                // Redeeming：等 on_order_result 回调成功后移除
                MarketPhase::Redeeming => {
                    if now - state.entry_time > 300 {
                        warn!(slug = %slug, "赎回超时（5分钟），强制移除");
                        to_remove.push(slug.clone());
                    }
                }
            }
        }

        // 处理 Hedged → Redeeming：产出赎回请求 + 推进 phase
        for slug in &to_redeem {
            if let Some(info) = self.market_info.get(slug) {
                orders.push(OrderRequest {
                    account: self.big_wallet,
                    market_slug: slug.clone(),
                    token_id: String::new(),
                    side: Side::Buy,
                    order_kind: OrderKind::Redeem,
                    price_or_amount: 0.0,
                    size: None,
                    condition_id: info.condition_id.clone(),
                });
                orders.push(OrderRequest {
                    account: self.small_wallet,
                    market_slug: slug.clone(),
                    token_id: String::new(),
                    side: Side::Buy,
                    order_kind: OrderKind::Redeem,
                    price_or_amount: 0.0,
                    size: None,
                    condition_id: info.condition_id.clone(),
                });
            }
            // 推进 phase：Hedged → Redeeming，防止下一秒重复产出
            if let Some(state) = self.active_markets.get_mut(slug) {
                state.phase = MarketPhase::Redeeming;
            }
        }

        // 清理超时市场
        for slug in to_remove {
            self.remove_market(&slug, "超时清理");
        }

        orders
    }

    fn on_market_discovered(&mut self, markets: &[MarketInfo]) {
        for market in markets {
            self.market_info.insert(market.slug.clone(), market.clone());
            info!(
                slug = %market.slug,
                ticker = %market.ticker,
                expiration_ts = market.expiration_ts,
                "发现新市场"
            );
        }
    }

    fn big_wallet_for_market(&self, slug: &str) -> Option<AccountId> {
        if self.active_markets.contains_key(slug) {
            Some(self.big_wallet)
        } else {
            None
        }
    }

    fn big_order_id_for_market(&self, slug: &str) -> Option<String> {
        self.active_markets
            .get(slug)
            .and_then(|s| s.big_order_id.clone())
    }

    fn init_wallets(&mut self, balance_a: f64, balance_b: f64) {
        if balance_a >= balance_b {
            self.big_wallet = AccountId::A;
            self.small_wallet = AccountId::B;
        } else {
            self.big_wallet = AccountId::B;
            self.small_wallet = AccountId::A;
        }
        info!(
            big = ?self.big_wallet, small = ?self.small_wallet,
            balance_a = %balance_a, balance_b = %balance_b,
            "钱包初始化"
        );
    }
}

// ═══════════════════════════════════════════════════════════════════════════════
// 测试
// ═══════════════════════════════════════════════════════════════════════════════

#[cfg(test)]
mod tests {
    use super::*;

    fn test_config() -> StrategyConfig {
        StrategyConfig {
            name: "test".into(),
            contract_count: 10.0,
            excluded_tickers: vec!["xrp".into(), "doge".into()],
            max_active_markets: 5,
            ask_range: vec![0.989, 1.0],
            min_spread: 0.001,
            price_tick: 0.001,
            settle_time_range: vec![5, 300],
            oracle_max_age_secs: 3,
            require_fresh_oracle: false,
            min_oracle_move_pct: 0.0,
            settle_redeem_delay_secs: 30,
            order_status_delay_ms: 500,
            max_retries: 2,
            retry_delay_ms: 200,
            post_redeem_transfer_usdc: 10.0,
            max_loss_pct: 0.05,
            min_eth_balance: 0.00056,
            gtc_price_override: None,
        }
    }

    fn fresh_oracle(price: f64) -> OracleSnapshot {
        OracleSnapshot {
            price,
            timestamp: Utc::now().timestamp(),
        }
    }

    fn stale_oracle(price: f64) -> OracleSnapshot {
        OracleSnapshot {
            price,
            timestamp: Utc::now().timestamp() - 60,
        }
    }

    fn test_market(slug: &str, ticker: &str) -> MarketInfo {
        MarketInfo {
            slug: slug.into(),
            yes_token_id: format!("{}-yes", slug),
            no_token_id: format!("{}-no", slug),
            ticker: ticker.into(),
            expiration_ts: Utc::now().timestamp() + 100,
            liquidity: 10000.0,
            market_type: "binary".into(),
            categories: vec!["crypto".into()],
            open_price: Some(64000.0),
            min_size: Some(1.0),
            condition_id: Some(format!("0x{:0>64}", hex::encode(slug.as_bytes()))),
        }
    }

    // ── 入场过滤测试 ───────────────────────────────────────────────

    #[tokio::test]
    async fn test_filters_by_ask_range_below_min() {
        let mut s = DefaultDirectionalStrategy::new(test_config());
        s.market_info.insert("btc".into(), test_market("btc", "BTC"));
        let orders = s.handle_orderbook("btc", 0.97, 0.98).await;
        assert!(orders.is_empty(), "best_ask < ask_min 应跳过");
    }

    #[tokio::test]
    async fn test_filters_by_ask_range_above_max() {
        let mut s = DefaultDirectionalStrategy::new(test_config());
        s.market_info.insert("btc".into(), test_market("btc", "BTC"));
        // best_ask=1.005 > ask_max=1.0，应跳过
        let orders = s.handle_orderbook("btc", 0.99, 1.005).await;
        assert!(orders.is_empty(), "best_ask > ask_max 应跳过");
    }

    #[tokio::test]
    async fn test_filters_by_spread() {
        let mut s = DefaultDirectionalStrategy::new(test_config());
        s.market_info.insert("btc".into(), test_market("btc", "BTC"));
        let orders = s.handle_orderbook("btc", 0.9945, 0.995).await;
        assert!(orders.is_empty(), "spread <= min_spread 应跳过");
    }

    #[tokio::test]
    async fn test_excludes_tickers() {
        let mut s = DefaultDirectionalStrategy::new(test_config());
        s.market_info.insert("xrp".into(), test_market("xrp", "XRP"));
        let orders = s.handle_orderbook("xrp", 0.98, 0.995).await;
        assert!(orders.is_empty(), "排除币种应跳过");
    }

    #[tokio::test]
    async fn test_max_active_markets() {
        let mut s = DefaultDirectionalStrategy::new(test_config());
        s.init_wallets(1000.0, 500.0);

        // BTC 入场成功
        s.market_info.insert("btc".into(), test_market("btc", "BTC"));
        s.oracle_prices.insert("btc".into(), fresh_oracle(65000.0));
        let orders = s.handle_orderbook("btc", 0.99, 0.995).await;
        assert!(!orders.is_empty(), "BTC 应入场");

        // ETH 被拒绝（已有非 Redeeming 的活跃市场）
        s.market_info.insert("eth".into(), test_market("eth", "ETH"));
        s.oracle_prices.insert("eth".into(), fresh_oracle(3100.0));
        let orders = s.handle_orderbook("eth", 0.99, 0.995).await;
        assert!(orders.is_empty(), "ETH 应被拒绝（已有 BTC 在对冲中）");
    }

    // ── 防重复测试 ─────────────────────────────────────────────────

    #[tokio::test]
    async fn test_prevents_duplicate() {
        let mut s = DefaultDirectionalStrategy::new(test_config());
        s.market_info.insert("btc".into(), test_market("btc", "BTC"));
        s.oracle_prices.insert("btc".into(), fresh_oracle(65000.0));

        let orders1 = s.handle_orderbook("btc", 0.99, 0.995).await;
        assert_eq!(orders1.len(), 2, "第一次应产出");

        let orders2 = s.handle_orderbook("btc", 0.99, 0.996).await;
        assert!(orders2.is_empty(), "第二次应防重复");
    }

    // ── 下单参数测试 ───────────────────────────────────────────────

    #[tokio::test]
    async fn test_generates_orders_with_correct_params() {
        let mut s = DefaultDirectionalStrategy::new(test_config());
        s.market_info.insert("btc".into(), test_market("btc", "BTC"));
        s.oracle_prices.insert("btc".into(), fresh_oracle(65000.0));

        let orders = s.handle_orderbook("btc", 0.99, 0.995).await;
        assert_eq!(orders.len(), 2, "应产出 2 笔订单");

        // 大单：GTC @ 0.994，size=10
        assert_eq!(orders[0].order_kind, OrderKind::Gtc);
        assert_eq!(orders[0].side, Side::Buy);
        assert!((orders[0].price_or_amount - 0.994).abs() < 0.001);
        assert_eq!(orders[0].size, Some(10.0));

        // 小单：FOK，USDC = 10 × (1 - 0.994) = 0.06
        assert_eq!(orders[1].order_kind, OrderKind::Fok);
        assert_eq!(orders[1].side, Side::Buy);
        let expected_fok = 10.0 * (1.0 - 0.994);
        assert!(
            (orders[1].price_or_amount - expected_fok).abs() < 0.001,
            "FOK 金额应为 contract_count × (1 - gtc_price) = {}，实际 = {}",
            expected_fok,
            orders[1].price_or_amount
        );
        assert!(orders[1].size.is_none(), "FOK 不应有 size 字段");
    }

    #[tokio::test]
    async fn test_fok_amount_calculation() {
        let s = DefaultDirectionalStrategy::new(test_config());
        // gtc_price=0.994 → fok = 10 × 0.006 = 0.06
        assert!((s.fok_amount(0.994) - 0.06).abs() < 1e-10);
        // gtc_price=0.999 → fok = 10 × 0.001 = 0.01
        assert!((s.fok_amount(0.999) - 0.01).abs() < 1e-10);
        // gtc_price=0.9999 → fok = 10 × 0.0001 = 0.001（最小保护）
        assert!((s.fok_amount(0.9999) - 0.001).abs() < 1e-10);
    }

    #[tokio::test]
    async fn test_gtc_price_calculation() {
        let s = DefaultDirectionalStrategy::new(test_config());
        assert!((s.gtc_price(0.995) - 0.994).abs() < 1e-10);
        // 下限保护
        assert!((s.gtc_price(0.0005) - 0.001).abs() < 1e-10);
    }

    // ── 方向判断测试 ───────────────────────────────────────────────

    #[tokio::test]
    async fn test_direction_yes() {
        let mut s = DefaultDirectionalStrategy::new(test_config());
        s.market_info.insert("btc".into(), test_market("btc", "BTC"));
        s.oracle_prices.insert("btc".into(), fresh_oracle(65000.0));
        let orders = s.handle_orderbook("btc", 0.99, 0.995).await;
        assert!(orders[0].token_id.contains("yes"), "Oracle > open → 大单买 YES");
    }

    #[tokio::test]
    async fn test_direction_no() {
        let mut s = DefaultDirectionalStrategy::new(test_config());
        s.market_info.insert("btc".into(), test_market("btc", "BTC"));
        s.oracle_prices.insert("btc".into(), fresh_oracle(63000.0));
        let orders = s.handle_orderbook("btc", 0.99, 0.995).await;
        assert!(orders[0].token_id.contains("no"), "Oracle < open → 大单买 NO");
    }

    #[tokio::test]
    async fn test_oracle_move_below_threshold_skips_entry() {
        let mut config = test_config();
        config.require_fresh_oracle = true;
        config.min_oracle_move_pct = 0.001; // 0.1%
        let mut s = DefaultDirectionalStrategy::new(config);
        s.market_info.insert("btc".into(), test_market("btc", "BTC"));
        // open=64000，涨 10 = 0.015625%，低于 0.1%
        s.oracle_prices.insert("btc".into(), fresh_oracle(64010.0));

        let orders = s.handle_orderbook("btc", 0.99, 0.995).await;
        assert!(orders.is_empty(), "oracle 偏离未达到阈值应跳过");
    }

    #[tokio::test]
    async fn test_oracle_move_positive_threshold_buys_yes() {
        let mut config = test_config();
        config.require_fresh_oracle = true;
        config.min_oracle_move_pct = 0.001; // 0.1%
        let mut s = DefaultDirectionalStrategy::new(config);
        s.market_info.insert("btc".into(), test_market("btc", "BTC"));
        // open=64000，涨 100 = 0.15625%，超过 0.1%
        s.oracle_prices.insert("btc".into(), fresh_oracle(64100.0));

        let orders = s.handle_orderbook("btc", 0.99, 0.995).await;
        assert_eq!(orders.len(), 2);
        assert!(orders[0].token_id.contains("yes"), "正偏离应大单买 YES");
    }

    #[tokio::test]
    async fn test_oracle_move_negative_threshold_buys_no() {
        let mut config = test_config();
        config.require_fresh_oracle = true;
        config.min_oracle_move_pct = 0.001; // 0.1%
        let mut s = DefaultDirectionalStrategy::new(config);
        s.market_info.insert("btc".into(), test_market("btc", "BTC"));
        // open=64000，跌 100 = -0.15625%，超过 0.1%
        s.oracle_prices.insert("btc".into(), fresh_oracle(63900.0));

        let orders = s.handle_orderbook("btc", 0.99, 0.995).await;
        assert_eq!(orders.len(), 2);
        assert!(orders[0].token_id.contains("no"), "负偏离应大单买 NO");
    }

    #[tokio::test]
    async fn test_stale_oracle_required_skips_entry() {
        let mut config = test_config();
        config.require_fresh_oracle = true;
        config.oracle_max_age_secs = 3;
        let mut s = DefaultDirectionalStrategy::new(config);
        s.market_info.insert("btc".into(), test_market("btc", "BTC"));
        s.oracle_prices.insert("btc".into(), stale_oracle(65000.0));

        let orders = s.handle_orderbook("btc", 0.99, 0.995).await;
        assert!(orders.is_empty(), "要求新鲜 oracle 时，过期 oracle 应跳过");
    }

    #[tokio::test]
    async fn test_stale_oracle_can_fallback_when_not_required() {
        let mut config = test_config();
        config.require_fresh_oracle = false;
        config.oracle_max_age_secs = 3;
        let mut s = DefaultDirectionalStrategy::new(config);
        s.market_info.insert("btc".into(), test_market("btc", "BTC"));
        s.oracle_prices.insert("btc".into(), stale_oracle(63000.0));

        let orders = s.handle_orderbook("btc", 0.99, 0.995).await;
        assert_eq!(orders.len(), 2, "不要求新鲜 oracle 时，应走 fallback");
        assert!(orders[0].token_id.contains("yes"), "fallback: best_ask > 0.5 → YES");
    }

    // ── 钱包分配测试 ───────────────────────────────────────────────

    #[tokio::test]
    async fn test_wallet_selection() {
        let mut s = DefaultDirectionalStrategy::new(test_config());
        s.init_wallets(1000.0, 500.0);
        assert_eq!(s.big_wallet, AccountId::A);
        s.init_wallets(200.0, 800.0);
        assert_eq!(s.big_wallet, AccountId::B);
    }

    // ── 状态机测试 ─────────────────────────────────────────────────

    #[tokio::test]
    async fn test_state_machine_gtc_success_then_fok_success() {
        let mut s = DefaultDirectionalStrategy::new(test_config());
        s.market_info.insert("btc".into(), test_market("btc", "BTC"));
        s.oracle_prices.insert("btc".into(), fresh_oracle(65000.0));
        let orders = s.handle_orderbook("btc", 0.99, 0.995).await;
        assert_eq!(orders.len(), 2);

        // GTC 大单成功
        let gtc_result = OrderResult {
            request: orders[0].clone(),
            success: true,
            order_id: Some("order-123".into()),
            error: None,
        };
        s.on_order_result(&gtc_result).await;
        let state = s.active_markets.get("btc").unwrap();
        assert_eq!(state.phase, MarketPhase::BigPlaced);
        assert_eq!(state.big_order_id.as_deref(), Some("order-123"));

        // FOK 小单成功
        let fok_result = OrderResult {
            request: orders[1].clone(),
            success: true,
            order_id: Some("order-456".into()),
            error: None,
        };
        s.on_order_result(&fok_result).await;
        let state = s.active_markets.get("btc").unwrap();
        assert_eq!(state.phase, MarketPhase::Hedged);
    }

    #[tokio::test]
    async fn test_state_machine_gtc_fail_removes_market() {
        let mut s = DefaultDirectionalStrategy::new(test_config());
        s.market_info.insert("btc".into(), test_market("btc", "BTC"));
        s.oracle_prices.insert("btc".into(), fresh_oracle(65000.0));
        let orders = s.handle_orderbook("btc", 0.99, 0.995).await;

        // GTC 大单失败 → 移除
        let gtc_result = OrderResult {
            request: orders[0].clone(),
            success: false,
            order_id: None,
            error: Some("余额不足".into()),
        };
        s.on_order_result(&gtc_result).await;
        assert!(!s.active_markets.contains_key("btc"), "GTC 失败应移除市场");
    }

    #[tokio::test]
    async fn test_state_machine_fok_fail_removes_market() {
        let mut s = DefaultDirectionalStrategy::new(test_config());
        s.market_info.insert("btc".into(), test_market("btc", "BTC"));
        s.oracle_prices.insert("btc".into(), fresh_oracle(65000.0));
        let orders = s.handle_orderbook("btc", 0.99, 0.995).await;

        // GTC 先成功
        s.on_order_result(&OrderResult {
            request: orders[0].clone(),
            success: true,
            order_id: Some("order-1".into()),
            error: None,
        }).await;

        // FOK 失败 → 移除
        s.on_order_result(&OrderResult {
            request: orders[1].clone(),
            success: false,
            order_id: None,
            error: Some("流动性不足".into()),
        }).await;
        assert!(!s.active_markets.contains_key("btc"), "FOK 失败应移除市场");
    }

    #[tokio::test]
    async fn test_state_machine_redeem_success_removes_market() {
        let mut s = DefaultDirectionalStrategy::new(test_config());
        s.market_info.insert("btc".into(), test_market("btc", "BTC"));
        s.oracle_prices.insert("btc".into(), fresh_oracle(65000.0));
        let orders = s.handle_orderbook("btc", 0.99, 0.995).await;

        // 快进到 Hedged
        s.on_order_result(&OrderResult { request: orders[0].clone(), success: true, order_id: Some("1".into()), error: None }).await;
        s.on_order_result(&OrderResult { request: orders[1].clone(), success: true, order_id: Some("2".into()), error: None }).await;

        // 赎回成功
        let redeem_order = OrderRequest {
            account: s.big_wallet,
            market_slug: "btc".into(),
            token_id: String::new(),
            side: Side::Buy,
            order_kind: OrderKind::Redeem,
            price_or_amount: 0.0,
            size: None,
            condition_id: Some("0xabc".into()),
        };
        s.on_order_result(&OrderResult { request: redeem_order, success: true, order_id: Some("redeemed".into()), error: None }).await;
        assert!(!s.active_markets.contains_key("btc"), "赎回成功应移除市场");
    }

    #[tokio::test]
    async fn test_state_machine_redeem_fail_retries() {
        let mut s = DefaultDirectionalStrategy::new(test_config());
        s.market_info.insert("btc".into(), test_market("btc", "BTC"));
        s.oracle_prices.insert("btc".into(), fresh_oracle(65000.0));
        let orders = s.handle_orderbook("btc", 0.99, 0.995).await;

        // 快进到 Hedged
        s.on_order_result(&OrderResult { request: orders[0].clone(), success: true, order_id: Some("1".into()), error: None }).await;
        s.on_order_result(&OrderResult { request: orders[1].clone(), success: true, order_id: Some("2".into()), error: None }).await;

        // 赎回失败 → 退回 Hedged（on_tick 下秒重试）
        let redeem_order = OrderRequest {
            account: s.big_wallet,
            market_slug: "btc".into(),
            token_id: String::new(),
            side: Side::Buy,
            order_kind: OrderKind::Redeem,
            price_or_amount: 0.0,
            size: None,
            condition_id: Some("0xabc".into()),
        };
        s.on_order_result(&OrderResult { request: redeem_order, success: false, order_id: None, error: Some("revert".into()) }).await;
        let state = s.active_markets.get("btc").unwrap();
        assert_eq!(state.phase, MarketPhase::Hedged, "赎回失败应退回 Hedged 等重试");
    }

    // ── on_tick 测试 ───────────────────────────────────────────────

    #[tokio::test]
    async fn test_on_tick_produces_redemption_when_hedged_and_expired() {
        let mut s = DefaultDirectionalStrategy::new(test_config());
        s.market_info.insert("btc".into(), test_market("btc", "BTC"));
        s.oracle_prices.insert("btc".into(), fresh_oracle(65000.0));
        let orders = s.handle_orderbook("btc", 0.99, 0.995).await;

        // 快进到 Hedged
        s.on_order_result(&OrderResult { request: orders[0].clone(), success: true, order_id: Some("1".into()), error: None }).await;
        s.on_order_result(&OrderResult { request: orders[1].clone(), success: true, order_id: Some("2".into()), error: None }).await;

        // 把 market_info 的 expiration_ts 改为已过期
        let now = Utc::now().timestamp();
        s.market_info.get_mut("btc").unwrap().expiration_ts = now - 50;

        let redeem_orders = s.on_tick().await;
        assert_eq!(redeem_orders.len(), 2, "应产出 2 个赎回请求（双钱包）");
        assert!(redeem_orders.iter().all(|o| o.order_kind == OrderKind::Redeem));
        assert!(redeem_orders.iter().all(|o| o.condition_id.is_some()));
    }

    #[tokio::test]
    async fn test_on_tick_does_not_produce_redemption_before_delay() {
        let mut s = DefaultDirectionalStrategy::new(test_config());
        s.market_info.insert("btc".into(), test_market("btc", "BTC"));
        s.oracle_prices.insert("btc".into(), fresh_oracle(65000.0));
        let orders = s.handle_orderbook("btc", 0.99, 0.995).await;

        s.on_order_result(&OrderResult { request: orders[0].clone(), success: true, order_id: Some("1".into()), error: None }).await;
        s.on_order_result(&OrderResult { request: orders[1].clone(), success: true, order_id: Some("2".into()), error: None }).await;

        // 过期但还没到 delay（delay=30s，过期 10s < 30s）
        let now = Utc::now().timestamp();
        s.market_info.get_mut("btc").unwrap().expiration_ts = now - 10;

        let redeem_orders = s.on_tick().await;
        assert!(redeem_orders.is_empty(), "未到 delay 时间不应产出赎回");
    }

    // ── big_wallet_for_market 测试 ─────────────────────────────────

    #[tokio::test]
    async fn test_big_wallet_for_market() {
        let mut s = DefaultDirectionalStrategy::new(test_config());
        s.market_info.insert("btc".into(), test_market("btc", "BTC"));
        s.oracle_prices.insert("btc".into(), fresh_oracle(65000.0));
        let _orders = s.handle_orderbook("btc", 0.99, 0.995).await;
        assert!(s.big_wallet_for_market("btc").is_some());
        assert!(s.big_wallet_for_market("eth").is_none());
    }

    #[tokio::test]
    async fn test_big_order_id_for_market() {
        let mut s = DefaultDirectionalStrategy::new(test_config());
        s.market_info.insert("btc".into(), test_market("btc", "BTC"));
        s.oracle_prices.insert("btc".into(), fresh_oracle(65000.0));
        let orders = s.handle_orderbook("btc", 0.99, 0.995).await;

        // GTC 成功后有 order_id
        s.on_order_result(&OrderResult { request: orders[0].clone(), success: true, order_id: Some("order-123".into()), error: None }).await;
        assert_eq!(s.big_order_id_for_market("btc").as_deref(), Some("order-123"));
    }
}
