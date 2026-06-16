use tracing::{debug, error, info, warn};

use crate::domain::order::{AccountId, OrderKind, OrderRequest, OrderResult, Side};
use crate::infrastructure::limitless::LimitlessApi;
use crate::utils::onchain::BaseChainClient;

/// 订单执行器
///
/// 职责：将策略产出的 `OrderRequest` 转化为 SDK 调用，返回 `OrderResult`。
///
/// 支持操作：
/// - 下单：FOK/FAK/GTC
/// - 撤单：单笔撤单、按市场全撤
/// - 自动检查 USDC 授权，未授权则自动授权
///
/// 设计原则：
/// - 不关心策略逻辑，只负责"怎么下单"
/// - 支持 A/B 双账号，通过 `OrderRequest.account` 选择
pub struct OrderExecutor {
    api_a: LimitlessApi,
    api_b: Option<LimitlessApi>,
    rpc_url: String,
}

impl OrderExecutor {
    /// 双号模式
    pub fn new(api_a: LimitlessApi, api_b: LimitlessApi, rpc_url: &str) -> Self {
        info!("订单执行器已创建（双号模式）");
        Self {
            api_a,
            api_b: Some(api_b),
            rpc_url: rpc_url.to_string(),
        }
    }

    /// 单号模式（测试 / B 号未配置时）
    pub fn from_single_api(api_a: LimitlessApi, rpc_url: &str) -> Self {
        info!("订单执行器已创建（单号模式）");
        Self {
            api_a,
            api_b: None,
            rpc_url: rpc_url.to_string(),
        }
    }

    /// 确保 USDC 已授权给 Limitless CTF Exchange
    ///
    /// 直接发送 approve(max) 交易，不检查当前额度。
    /// approve 是幂等操作，重复调用无副作用。
    /// 返回 tx_hash（交易哈希）
    pub async fn ensure_usdc_approved(&self, account: AccountId) -> anyhow::Result<String> {
        let api = match self.select_api(account) {
            Some(api) => api,
            None => anyhow::bail!("B 号未配置"),
        };

        let wallet = api.verify_auth().await?;
        let onchain = BaseChainClient::new(Some(&self.rpc_url));

        info!(wallet = %wallet, "发送 USDC approve 交易...");
        let tx_hash = onchain.send_approve_tx(&api.account.private_key).await?;
        info!(tx_hash = %tx_hash, "USDC approve 交易已发送，等待上链...");

        // 等待交易确认（轮询回执）
        for i in 0..10 {
            tokio::time::sleep(std::time::Duration::from_secs(2)).await;
            match onchain.get_transaction_receipt(&tx_hash).await {
                Ok(receipt) => {
                    let status = receipt.status();
                    let gas_used = receipt.gas_used;
                    info!(
                        attempt = i + 1,
                        status = ?status,
                        gas_used = %gas_used,
                        "交易回执"
                    );
                    if status {
                        info!("✅ USDC approve 成功");
                        return Ok(tx_hash);
                    } else {
                        anyhow::bail!("USDC approve 交易失败（reverted）");
                    }
                }
                Err(e) => {
                    debug!(attempt = i + 1, error = %e, "等待回执...");
                }
            }
        }

        warn!("⚠️  未获取到交易回执，请手动检查");
        Ok(tx_hash)
    }

    /// SDK 客户端引用（供 MarketDiscovery 使用）
    pub fn sdk_client(&self) -> &limitless_exchange_rust_sdk::Client {
        &self.api_a.sdk
    }

    /// 获取 A 号 API
    pub fn api_a(&self) -> &LimitlessApi {
        &self.api_a
    }

    /// 获取 B 号 API
    pub fn api_b(&self) -> Option<&LimitlessApi> {
        self.api_b.as_ref()
    }

    /// 执行单笔下单请求
    ///
    /// 流程：
    /// 1. 根据 `request.account` 选择 API 实例
    /// 2. 创建 OrderClient（SDK 自动处理 EIP-712 签名）
    /// 3. 根据 `order_kind` 调用对应的 SDK 方法
    /// 4. 返回 OrderResult
    pub async fn execute(&self, request: &OrderRequest) -> OrderResult {
        let api = match self.select_api(request.account) {
            Some(api) => api,
            None => {
                return OrderResult {
                    request: request.clone(),
                    success: false,
                    order_id: None,
                    error: Some("B 号未配置".into()),
                };
            }
        };

        // 创建 OrderClient
        let order_client = match api.order_client() {
            Ok(c) => c,
            Err(e) => {
                error!(
                    account = ?request.account,
                    market = %request.market_slug,
                    error = %e,
                    "创建 OrderClient 失败"
                );
                return OrderResult {
                    request: request.clone(),
                    success: false,
                    order_id: None,
                    error: Some(format!("创建 OrderClient 失败: {}", e)),
                };
            }
        };

        // 映射 Side
        let side = match request.side {
            Side::Buy => limitless_exchange_rust_sdk::Side::Buy,
            Side::Sell => limitless_exchange_rust_sdk::Side::Sell,
        };

        // 根据订单类型分发
        let result = match request.order_kind {
            OrderKind::Fok => {
                self.execute_fok(
                    &order_client,
                    &request.market_slug,
                    &request.token_id,
                    side,
                    request.price_or_amount,
                )
                .await
            }
            OrderKind::Fak => {
                self.execute_fak(
                    &order_client,
                    &request.market_slug,
                    &request.token_id,
                    side,
                    request.price_or_amount,
                    request.size.unwrap_or(0.0),
                )
                .await
            }
            OrderKind::Gtc => {
                self.execute_gtc(
                    &order_client,
                    &request.market_slug,
                    &request.token_id,
                    side,
                    request.price_or_amount,
                    request.size.unwrap_or(0.0),
                )
                .await
            }
            OrderKind::Redeem => {
                // 赎回走链上合约调用，必须携带 condition_id
                let cond_id = request.condition_id.as_deref().unwrap_or("");
                if cond_id.is_empty() {
                    error!(
                        account = ?request.account,
                        market = %request.market_slug,
                        "Redeem 请求缺少 condition_id"
                    );
                    return OrderResult {
                        request: request.clone(),
                        success: false,
                        order_id: None,
                        error: Some("Redeem 请求缺少 condition_id".into()),
                    };
                }
                info!(
                    account = ?request.account,
                    market = %request.market_slug,
                    condition_id = %cond_id,
                    "开始执行赎回"
                );
                api.redeem(cond_id, &self.rpc_url)
                    .await
                    .map(|_| "redeemed".to_string())
            }
        };

        // 包装结果
        match result {
            Ok(order_id) => {
                info!(
                    account = ?request.account,
                    market = %request.market_slug,
                    order_id = %order_id,
                    kind = ?request.order_kind,
                    "下单成功"
                );
                OrderResult {
                    request: request.clone(),
                    success: true,
                    order_id: Some(order_id),
                    error: None,
                }
            }
            Err(e) => {
                error!(
                    account = ?request.account,
                    market = %request.market_slug,
                    error = %e,
                    "下单失败"
                );
                OrderResult {
                    request: request.clone(),
                    success: false,
                    order_id: None,
                    error: Some(format!("{}", e)),
                }
            }
        }
    }

    /// 批量执行下单请求（串行，带请求间隔）
    pub async fn execute_batch(
        &self,
        requests: &[OrderRequest],
        delay_ms: u64,
    ) -> Vec<OrderResult> {
        let mut results = Vec::with_capacity(requests.len());
        for (i, request) in requests.iter().enumerate() {
            if i > 0 {
                tokio::time::sleep(std::time::Duration::from_millis(delay_ms)).await;
            }
            results.push(self.execute(request).await);
        }
        results
    }

    /// 撤销单笔订单
    pub async fn cancel(&self, account: AccountId, order_id: &str) -> anyhow::Result<()> {
        let api = match self.select_api(account) {
            Some(api) => api,
            None => anyhow::bail!("B 号未配置"),
        };

        info!(account = ?account, order_id = %order_id, "撤销订单");
        let order_client = api.order_client()?;
        order_client.cancel(order_id).await?;
        info!(account = ?account, order_id = %order_id, "撤单成功");
        Ok(())
    }

    /// 按市场撤销所有订单
    pub async fn cancel_all(&self, account: AccountId, market_slug: &str) -> anyhow::Result<()> {
        let api = match self.select_api(account) {
            Some(api) => api,
            None => anyhow::bail!("B 号未配置"),
        };

        info!(account = ?account, market = %market_slug, "撤销全部订单");
        let order_client = api.order_client()?;
        order_client.cancel_all(market_slug).await?;
        info!(account = ?account, market = %market_slug, "全撤成功");
        Ok(())
    }

    /// 账号间 USDC 转账（链上 ERC20 transfer）
    pub async fn transfer_usdc_between_accounts(
        &self,
        from: AccountId,
        to: AccountId,
        amount_usdc: f64,
    ) -> anyhow::Result<String> {
        let from_api = self
            .select_api(from)
            .ok_or_else(|| anyhow::anyhow!("转出账号未配置: {:?}", from))?;
        let to_api = self
            .select_api(to)
            .ok_or_else(|| anyhow::anyhow!("转入账号未配置: {:?}", to))?;

        let to_wallet = to_api.verify_auth().await?;
        let onchain = BaseChainClient::new(Some(&self.rpc_url));

        info!(
            from = ?from,
            to = ?to,
            to_wallet = %to_wallet,
            amount_usdc = %amount_usdc,
            "开始 USDC 资金回补转账"
        );

        let tx_hash = onchain
            .send_usdc_transfer_tx(&from_api.account.private_key, &to_wallet, amount_usdc)
            .await?;
        info!(tx_hash = %tx_hash, "USDC 转账已发送，等待上链...");

        for i in 0..10 {
            tokio::time::sleep(std::time::Duration::from_secs(2)).await;
            match onchain.get_transaction_receipt(&tx_hash).await {
                Ok(receipt) => {
                    let status = receipt.status();
                    info!(attempt = i + 1, tx_hash = %tx_hash, status = ?status, "USDC 转账回执");
                    if status {
                        info!(tx_hash = %tx_hash, amount_usdc = %amount_usdc, "✅ USDC 转账成功");
                        return Ok(tx_hash);
                    } else {
                        anyhow::bail!("USDC 转账交易失败（reverted）");
                    }
                }
                Err(e) => {
                    debug!(attempt = i + 1, tx_hash = %tx_hash, error = %e, "等待 USDC 转账回执...");
                }
            }
        }

        warn!(tx_hash = %tx_hash, "⚠️  未获取到 USDC 转账回执，请手动检查");
        Ok(tx_hash)
    }

    /// 根据 AccountId 选择 API 实例
    fn select_api(&self, account: AccountId) -> Option<&LimitlessApi> {
        match account {
            AccountId::A => Some(&self.api_a),
            AccountId::B => match &self.api_b {
                Some(api) => Some(api),
                None => {
                    warn!(account = ?account, "B 号未配置");
                    None
                }
            },
        }
    }

    /// FOK 下单（Fill-Or-Kill）
    async fn execute_fok(
        &self,
        order_client: &limitless_exchange_rust_sdk::OrderClient,
        market_slug: &str,
        token_id: &str,
        side: limitless_exchange_rust_sdk::Side,
        maker_amount: f64,
    ) -> Result<String, anyhow::Error> {
        let resp = order_client
            .create_order(limitless_exchange_rust_sdk::CreateOrderParams {
                order_type: limitless_exchange_rust_sdk::OrderType::Fok,
                market_slug: market_slug.to_string(),
                args: limitless_exchange_rust_sdk::FokOrderArgs {
                    token_id: token_id.to_string(),
                    side,
                    maker_amount,
                    expiration: None,
                    nonce: None,
                    taker: None,
                }
                .into(),
            })
            .await?;
        Ok(resp.order.id)
    }

    /// FAK 下单（Fill-And-Kill）
    async fn execute_fak(
        &self,
        order_client: &limitless_exchange_rust_sdk::OrderClient,
        market_slug: &str,
        token_id: &str,
        side: limitless_exchange_rust_sdk::Side,
        price: f64,
        size: f64,
    ) -> Result<String, anyhow::Error> {
        let resp = order_client
            .create_order(limitless_exchange_rust_sdk::CreateOrderParams {
                order_type: limitless_exchange_rust_sdk::OrderType::Fak,
                market_slug: market_slug.to_string(),
                args: limitless_exchange_rust_sdk::FakOrderArgs {
                    token_id: token_id.to_string(),
                    side,
                    price,
                    size,
                    expiration: None,
                    nonce: None,
                    taker: None,
                }
                .into(),
            })
            .await?;
        Ok(resp.order.id)
    }

    /// GTC 下单（Good-Til-Cancel）
    async fn execute_gtc(
        &self,
        order_client: &limitless_exchange_rust_sdk::OrderClient,
        market_slug: &str,
        token_id: &str,
        side: limitless_exchange_rust_sdk::Side,
        price: f64,
        size: f64,
    ) -> Result<String, anyhow::Error> {
        let resp = order_client
            .create_order(limitless_exchange_rust_sdk::CreateOrderParams {
                order_type: limitless_exchange_rust_sdk::OrderType::Gtc,
                market_slug: market_slug.to_string(),
                args: limitless_exchange_rust_sdk::GtcOrderArgs {
                    token_id: token_id.to_string(),
                    side,
                    price,
                    size,
                    post_only: true,
                    expiration: None,
                    nonce: None,
                    taker: None,
                }
                .into(),
            })
            .await?;
        Ok(resp.order.id)
    }
}
