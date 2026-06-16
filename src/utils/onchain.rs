//! 链上工具：USDC 余额查询 / 授权 / 赎回
//!
//! 基于 alloy-rs 实现，通过 `sol!` 宏生成类型安全的合约调用，
//! 通过 Provider + PrivateKeySigner 处理签名与广播。

use std::str::FromStr;

use alloy::primitives::{address, Address, FixedBytes, TxHash, U256};
use alloy::providers::{Provider, ProviderBuilder};
use alloy::rpc::types::TransactionReceipt;
use alloy::signers::local::PrivateKeySigner;
use alloy::sol;
use tracing::info;

// ═══════════════════════════════════════════════════════════════════════════════
// Base 链常量（address! 宏在编译期校验 hex 合法性）
// ═══════════════════════════════════════════════════════════════════════════════

/// Base 主网 USDC 合约地址
const USDC_ADDRESS: Address = address!("833589fCD6eDb6E08f4c7C32D4f71b54bdA02913");

/// Limitless CLOB Exchange 合约地址（用于 USDC 授权）
const CLOB_EXCHANGE: Address = address!("05c748E2f4DcDe0ec9Fa8DDc40DE6b867f923fa5");

/// Limitless ConditionalTokens 合约地址（用于 redeemPositions / payoutDenominator）
const CTF_EXCHANGE: Address = address!("C9c98965297Bc527861c898329Ee280632B76e18");

/// USDC 精度（6 位小数）
const USDC_DECIMALS: u32 = 6;

// ═══════════════════════════════════════════════════════════════════════════════
// 合约接口（sol! 宏生成）
// ═══════════════════════════════════════════════════════════════════════════════

sol! {
    /// ERC20 子集（仅用到 balanceOf / allowance / approve）
    #[sol(rpc)]
    contract IERC20 {
        function balanceOf(address account) external view returns (uint256);
        function allowance(address owner, address spender) external view returns (uint256);
        function approve(address spender, uint256 amount) external returns (bool);
        function transfer(address to, uint256 amount) external returns (bool);
    }

    /// Gnosis ConditionalTokens（Limitless 部署在 Base 上）
    #[sol(rpc)]
    contract IConditionalTokens {
        function payoutDenominator(bytes32 conditionId) external view returns (uint256);
        function redeemPositions(
            address collateralToken,
            bytes32 parentCollectionId,
            bytes32 conditionId,
            uint256[] indexSets
        ) external;
    }
}

// ═══════════════════════════════════════════════════════════════════════════════
// 链上客户端
// ═══════════════════════════════════════════════════════════════════════════════

/// Base 链工具
pub struct BaseChainClient {
    rpc_url: String,
}

impl BaseChainClient {
    pub fn new(rpc_url: Option<&str>) -> Self {
        Self {
            rpc_url: rpc_url
                .unwrap_or("https://mainnet.base.org")
                .to_string(),
        }
    }

    /// 构建只读 provider（用于 view 调用）
    async fn read_provider(&self) -> anyhow::Result<impl Provider + '_> {
        let url = self.rpc_url.parse()?;
        Ok(ProviderBuilder::new().connect_http(url))
    }

    /// 构建带签名钱包的 provider（用于发交易）
    fn wallet_provider(&self, private_key: &str) -> anyhow::Result<impl Provider + '_> {
        let signer = PrivateKeySigner::from_str(private_key)?;
        let ethereum_wallet = alloy::network::EthereumWallet::from(signer);
        let url = self.rpc_url.parse()?;
        Ok(ProviderBuilder::new().wallet(ethereum_wallet).connect_http(url))
    }

    /// 查询 USDC 余额
    pub async fn get_usdc_balance(&self, wallet: &str) -> anyhow::Result<f64> {
        let addr = parse_address(wallet)?;
        let provider = self.read_provider().await?;
        let contract = IERC20::new(USDC_ADDRESS, &provider);
        let balance = contract.balanceOf(addr).call().await?;
        Ok(scale_usdc(balance))
    }

    /// 查询 Base 原生 ETH 余额
    pub async fn get_eth_balance(&self, wallet: &str) -> anyhow::Result<f64> {
        let addr = parse_address(wallet)?;
        let provider = self.read_provider().await?;
        let balance = provider.get_balance(addr).await?;
        Ok(scale_eth(balance))
    }

    /// 查询 USDC 授权额度（owner 授权给 CLOB Exchange 的金额）
    ///
    /// 返回 true 表示已授权（allowance > 0），false 表示未授权
    pub async fn is_usdc_approved(&self, owner: &str) -> anyhow::Result<bool> {
        let owner = parse_address(owner)?;
        let provider = self.read_provider().await?;
        let contract = IERC20::new(USDC_ADDRESS, &provider);
        let allowance = contract.allowance(owner, CLOB_EXCHANGE).call().await?;
        Ok(!allowance.is_zero())
    }

    /// 查询 condition 是否已 resolve（payoutDenominator > 0）
    pub async fn is_condition_resolved(&self, condition_id: &str) -> anyhow::Result<bool> {
        let cond_id = parse_bytes32(condition_id)?;
        let provider = self.read_provider().await?;
        let contract = IConditionalTokens::new(CTF_EXCHANGE, &provider);
        let denominator = contract.payoutDenominator(cond_id).call().await?;
        Ok(!denominator.is_zero())
    }

    /// 发送 USDC approve 交易（授权 CLOB Exchange 使用最大额度 USDC）
    ///
    /// 返回交易哈希。approve 是幂等操作，重复调用无副作用。
    /// alloy 自动处理 nonce / gas 估算 / EIP-155 签名。
    pub async fn send_approve_tx(&self, private_key: &str) -> anyhow::Result<String> {
        let provider = self.wallet_provider(private_key)?;
        let from = derive_address_from_pk(private_key)?;
        let contract = IERC20::new(USDC_ADDRESS, &provider);

        info!(from = %from, spender = %CLOB_EXCHANGE, "构造 USDC approve 交易");

        let pending = contract
            .approve(CLOB_EXCHANGE, U256::MAX)
            .from(from)
            .send()
            .await?;
        let tx_hash = *pending.tx_hash();
        info!(tx_hash = %tx_hash, "USDC approve 交易已发送");

        Ok(tx_hash.to_string())
    }

    /// 发送 USDC transfer 交易
    ///
    /// `amount_usdc` 使用十进制 USDC 数量，例如 10.0 表示 10 USDC。
    pub async fn send_usdc_transfer_tx(
        &self,
        private_key: &str,
        to: &str,
        amount_usdc: f64,
    ) -> anyhow::Result<String> {
        if amount_usdc <= 0.0 {
            anyhow::bail!("USDC 转账金额必须大于 0，当前 {amount_usdc}");
        }

        let provider = self.wallet_provider(private_key)?;
        let from = derive_address_from_pk(private_key)?;
        let to = parse_address(to)?;
        let amount = usdc_to_raw(amount_usdc)?;
        let contract = IERC20::new(USDC_ADDRESS, &provider);

        info!(from = %from, to = %to, amount_usdc = %amount_usdc, "构造 USDC transfer 交易");

        let pending = contract
            .transfer(to, amount)
            .from(from)
            .send()
            .await?;
        let tx_hash = *pending.tx_hash();
        info!(tx_hash = %tx_hash, amount_usdc = %amount_usdc, "USDC transfer 交易已发送");

        Ok(tx_hash.to_string())
    }

    /// 赎回已结算市场的仓位（链上调用 ConditionalTokens.redeemPositions）
    ///
    /// 前提：condition 已经 resolve（payoutDenominator > 0）。
    /// binary market 的 indexSets = [1, 2]（赎回 YES 和 NO 两个方向）。
    /// ABI 编码由 alloy sol! 宏生成的类型自动处理。
    pub async fn send_redeem_tx(
        &self,
        private_key: &str,
        condition_id: &str,
    ) -> anyhow::Result<String> {
        let provider = self.wallet_provider(private_key)?;
        let from = derive_address_from_pk(private_key)?;
        let cond_id = parse_bytes32(condition_id)?;
        let contract = IConditionalTokens::new(CTF_EXCHANGE, &provider);

        // binary market: indexSets = [1, 2]
        let index_sets = vec![U256::from(1u64), U256::from(2u64)];

        info!(from = %from, condition_id = %condition_id, "构造 redeemPositions 交易");

        let pending = contract
            .redeemPositions(USDC_ADDRESS, FixedBytes::<32>::ZERO, cond_id, index_sets)
            .from(from)
            .send()
            .await?;
        let tx_hash = *pending.tx_hash();
        info!(tx_hash = %tx_hash, "redeemPositions 交易已发送");
        Ok(tx_hash.to_string())
    }

    /// 查询交易回执
    pub async fn get_transaction_receipt(&self, tx_hash: &str) -> anyhow::Result<TransactionReceipt> {
        let hash = TxHash::from_str(tx_hash)?;
        let provider = self.read_provider().await?;
        let receipt = provider
            .get_transaction_receipt(hash)
            .await?
            .ok_or_else(|| anyhow::anyhow!("交易回执不存在: {tx_hash}"))?;
        Ok(receipt)
    }
}

// ═══════════════════════════════════════════════════════════════════════════════
// 工具函数
// ═══════════════════════════════════════════════════════════════════════════════

/// 将 uint256 按 USDC 精度转为 f64
fn scale_usdc(raw: U256) -> f64 {
    let raw_u128: u128 = raw.try_into().unwrap_or(0);
    raw_u128 as f64 / 10f64.powi(USDC_DECIMALS as i32)
}

/// 将 uint256 按 ETH 精度转为 f64
fn scale_eth(raw: U256) -> f64 {
    let raw_u128: u128 = raw.try_into().unwrap_or(0);
    raw_u128 as f64 / 1e18
}

/// 将十进制 USDC 数量转为链上 raw amount
fn usdc_to_raw(amount_usdc: f64) -> anyhow::Result<U256> {
    if !amount_usdc.is_finite() || amount_usdc <= 0.0 {
        anyhow::bail!("USDC 金额非法: {amount_usdc}");
    }
    let scaled = (amount_usdc * 10f64.powi(USDC_DECIMALS as i32)).round();
    Ok(U256::from(scaled as u128))
}

/// 解析地址字符串（支持 0x 前缀或 checksum 格式）
fn parse_address(s: &str) -> anyhow::Result<Address> {
    Address::from_str(s).map_err(|e| anyhow::anyhow!("解析地址失败 {s}: {e}"))
}

/// 解析 bytes32（condition_id，支持 0x 开头的 64 位 hex）
fn parse_bytes32(s: &str) -> anyhow::Result<FixedBytes<32>> {
    let clean = s.strip_prefix("0x").unwrap_or(s);
    let bytes = hex::decode(clean)?;
    if bytes.len() != 32 {
        anyhow::bail!("condition_id 必须是 32 字节，当前 {} 字节", bytes.len());
    }
    let mut arr = [0u8; 32];
    arr.copy_from_slice(&bytes);
    Ok(FixedBytes::from_slice(&arr))
}

/// 从私钥派生地址（用于日志与 from 字段）
fn derive_address_from_pk(private_key: &str) -> anyhow::Result<Address> {
    let signer = PrivateKeySigner::from_str(private_key)?;
    Ok(signer.address())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_bytes32() {
        let valid = "0x0000000000000000000000000000000000000000000000000000000000000001";
        let fb = parse_bytes32(valid).unwrap();
        assert_eq!(fb[31], 1);

        let too_short = "0x1234";
        assert!(parse_bytes32(too_short).is_err());
    }

    #[test]
    fn test_scale_usdc() {
        // 100 USDC = 100 * 1e6
        let raw = U256::from(100_000_000u64);
        assert!((scale_usdc(raw) - 100.0).abs() < 1e-10);
    }

    #[test]
    fn test_usdc_to_raw() {
        assert_eq!(usdc_to_raw(10.0).unwrap(), U256::from(10_000_000u64));
        assert_eq!(usdc_to_raw(0.5).unwrap(), U256::from(500_000u64));
        assert!(usdc_to_raw(0.0).is_err());
        assert!(usdc_to_raw(-1.0).is_err());
    }

    #[test]
    fn test_scale_eth() {
        let raw = U256::from(1_000_000_000_000_000_000u128);
        assert!((scale_eth(raw) - 1.0).abs() < 1e-10);
    }

    #[test]
    fn test_parse_address() {
        let addr = parse_address("0x833589fCD6eDb6E08f4c7C32D4f71b54bdA02913");
        assert!(addr.is_ok());
    }
}
