use tracing::{debug, info, warn};

use crate::domain::config::AccountConfig;

/// Limitless API 封装 - 阶段 1 最小版本
#[allow(dead_code)]
pub struct LimitlessApi {
    pub sdk: limitless_exchange_rust_sdk::Client,
    pub account: AccountConfig,
}

impl LimitlessApi {
    /// 创建 API 实例
    ///
    /// 认证优先级：
    /// 1. HMAC (hmac_token_id + hmac_secret) → 用于 Server/Partner 账号
    /// 2. API Key (api_key) → 用于普通账号
    /// 3. 环境变量 LIMITLESS_API_KEY → 兜底
    pub fn new(account: AccountConfig) -> anyhow::Result<Self> {
        let sdk = if !account.hmac_token_id.is_empty() {
            info!(token_id = %account.hmac_token_id, "使用 HMAC 认证");
            let http_client = limitless_exchange_rust_sdk::HttpClient::builder()
                .hmac_credentials(limitless_exchange_rust_sdk::HmacCredentials {
                    token_id: account.hmac_token_id.clone(),
                    secret: account.hmac_secret.clone(),
                })
                .build()?;
            limitless_exchange_rust_sdk::Client::from_http_client(http_client)?
        } else if !account.api_key.is_empty() {
            info!(api_key = %account.api_key, "使用 API Key 认证");
            let http_client = limitless_exchange_rust_sdk::HttpClient::builder()
                .api_key(&account.api_key)
                .build()?;
            limitless_exchange_rust_sdk::Client::from_http_client(http_client)?
        } else {
            info!("尝试环境变量 LIMITLESS_API_KEY 认证");
            limitless_exchange_rust_sdk::Client::new()?
        };
        Ok(Self { sdk, account })
    }

    /// 验证登录状态，返回钱包地址
    pub async fn verify_auth(&self) -> anyhow::Result<String> {
        let profile = self.sdk.portfolio.get_current_profile().await?;
        info!(
            user_id = profile.id,
            account = %profile.account,
            username = ?profile.username,
            "登录验证成功"
        );
        Ok(profile.account)
    }

    /// 创建 OrderClient（用 private_key 做 EIP-712 签名）
    ///
    /// 前提：account.private_key 已填入
    pub fn order_client(&self) -> anyhow::Result<limitless_exchange_rust_sdk::OrderClient> {
        if self.account.private_key.is_empty() {
            anyhow::bail!("private_key 未配置，无法创建 OrderClient");
        }
        Ok(self.sdk.new_order_client(&self.account.private_key, None)?)
    }

    /// 赎回已结算市场的仓位
    ///
    /// 链上调用 ConditionalTokens 合约的 redeemPositions（由 alloy 处理签名与广播）。
    /// 前提：condition 已 resolve（payoutDenominator > 0）。
    pub async fn redeem(&self, condition_id: &str, rpc_url: &str) -> anyhow::Result<()> {
        if self.account.private_key.is_empty() {
            anyhow::bail!("redeem 需要 private_key");
        }

        let onchain = crate::utils::onchain::BaseChainClient::new(Some(rpc_url));

        // 检查 condition 是否已 resolve
        info!(condition_id = %condition_id, "检查 condition 是否已 resolve");
        let resolved = onchain.is_condition_resolved(condition_id).await?;
        if !resolved {
            anyhow::bail!("condition 还未 resolve，payoutDenominator = 0，condition_id={condition_id}");
        }

        info!(condition_id = %condition_id, "condition 已 resolve，准备发送 redeemPositions");
        let tx_hash = onchain.send_redeem_tx(&self.account.private_key, condition_id).await?;
        info!(tx_hash = %tx_hash, condition_id = %condition_id, "✅ redeemPositions 交易已发送");

        // 等待确认
        for i in 0..10 {
            tokio::time::sleep(std::time::Duration::from_secs(2)).await;
            match onchain.get_transaction_receipt(&tx_hash).await {
                Ok(receipt) => {
                    // alloy TransactionReceipt.status() → bool（true = 成功）
                    let success = receipt.status();
                    info!(attempt = i + 1, tx_hash = %tx_hash, status = ?success, "redeem 交易回执");
                    if success {
                        info!(tx_hash = %tx_hash, condition_id = %condition_id, "✅ 赎回成功");
                        return Ok(());
                    } else {
                        anyhow::bail!("赎回交易失败（reverted）");
                    }
                }
                Err(_) => debug!(attempt = i + 1, tx_hash = %tx_hash, "等待 redeem 回执..."),
            }
        }

        warn!("⚠️  未获取到交易回执");
        Ok(())
    }
}
