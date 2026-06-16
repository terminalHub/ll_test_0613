use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Side {
    Buy,
    Sell,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum OrderKind {
    Fok,
    Fak,
    Gtc,
    Redeem,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AccountId {
    A,
    B,
}

/// 下单请求（策略层产出，执行层消费）
///
/// 字段说明：
/// - `account`: 使用哪个账号下单（A 或 B）
/// - `market_slug`: 市场标识
/// - `token_id`: YES 或 NO 的 token ID
/// - `side`: 买/卖方向
/// - `order_kind`: FOK/FAK/GTC/Redeem
/// - `price_or_amount`: FOK 时为 maker_amount (USDC)，GTC/FAK 时为 price
/// - `size`: GTC/FAK 时的合约数量，FOK 时忽略
/// - `condition_id`: Redeem 专用，CTF condition_id（0x 开头的 64 位 hex bytes32）
#[derive(Debug, Clone)]
pub struct OrderRequest {
    pub account: AccountId,
    pub market_slug: String,
    pub token_id: String,
    pub side: Side,
    pub order_kind: OrderKind,
    /// FOK: maker_amount (USDC); GTC/FAK: price
    pub price_or_amount: f64,
    /// GTC/FAK: size (contracts)
    pub size: Option<f64>,
    /// Redeem 操作专用：CTF condition_id（0x 开头的 64 位 hex bytes32）
    pub condition_id: Option<String>,
}

/// 下单结果（单笔订单）
#[derive(Debug, Clone)]
pub struct OrderResult {
    /// 关联的下单请求
    pub request: OrderRequest,
    /// 是否成功
    pub success: bool,
    /// 成功时返回的订单 ID
    pub order_id: Option<String>,
    /// 失败时的错误信息
    pub error: Option<String>,
}
