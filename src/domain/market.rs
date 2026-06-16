/// 市场静态信息
#[derive(Debug, Clone)]
pub struct MarketInfo {
    /// 市场标识
    pub slug: String,
    /// YES token ID
    pub yes_token_id: String,
    /// NO token ID
    pub no_token_id: String,
    /// 底层资产代码
    pub ticker: String,
    /// 到期时间戳（秒）
    pub expiration_ts: i64,
    /// 流动性（USDC）
    pub liquidity: f64,
    /// 市场类型
    pub market_type: String,
    /// 分类标签
    pub categories: Vec<String>,
    /// 开盘价（从 metadata.openPrice 提取）
    pub open_price: Option<f64>,
    /// 最小下单金额（从 settings.minSize 提取，USDC）
    pub min_size: Option<f64>,
    /// CTF condition ID（用于链上赎回）
    pub condition_id: Option<String>,
}
