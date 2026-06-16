use std::collections::HashMap;

use serde::Deserialize;

/// 账号配置
///
/// 支持两种认证方式：
/// 1. API Key：lmts_xxx 格式
/// 2. HMAC：token_id + secret（Server/Partner 账号）
#[derive(Debug, Clone, Deserialize)]
pub struct AccountConfig {
    /// API Key（格式：lmts_xxx），与 hmac_token_id 二选一
    #[serde(default)]
    pub api_key: String,
    /// HMAC token_id，与 api_key 二选一
    #[serde(default)]
    pub hmac_token_id: String,
    /// HMAC secret，配合 hmac_token_id 使用
    #[serde(default)]
    pub hmac_secret: String,
    /// 钱包私钥（用于 EIP-712 订单签名，格式：0x...）
    pub private_key: String,
}

/// 市场过滤条件
#[derive(Debug, Clone, Default, Deserialize)]
pub struct FiltersConfig {
    #[serde(flatten)]
    pub fields: HashMap<String, serde_json::Value>,
}

/// 对冲配置
#[derive(Debug, Clone, Deserialize)]
pub struct HedgeConfig {
    #[serde(default = "default_market_category")]
    pub market_category: String,
    #[serde(default = "default_request_delay")]
    pub request_delay_ms: u64,
    #[serde(default)]
    pub filters: Option<FiltersConfig>,
    pub markets: Option<Vec<String>>,

    // 旧配置（向后兼容）
    #[serde(default)]
    pub frequency: String,
    #[serde(default)]
    pub sub_frequency: String,
    #[serde(default)]
    pub symbols: Vec<String>,

    #[serde(default)]
    pub order_size: f64,
    #[serde(default)]
    pub entry_delay_secs: u64,
    #[serde(default = "default_scan_interval")]
    pub scan_interval_secs: u64,
}

fn default_market_category() -> String { "/crypto".into() }
fn default_request_delay() -> u64 { 200 }
fn default_scan_interval() -> u64 { 60 }

impl Default for HedgeConfig {
    fn default() -> Self {
        Self {
            order_size: 10.0,
            entry_delay_secs: 3,
            scan_interval_secs: 60,
            market_category: "/crypto".into(),
            request_delay_ms: 200,
            filters: None,
            markets: None,
            frequency: String::new(),
            sub_frequency: String::new(),
            symbols: Vec::new(),
        }
    }
}

impl HedgeConfig {
    /// 向后兼容迁移：旧字段 → filters
    pub fn migrate_legacy(&mut self) {
        if self.filters.is_some() {
            return;
        }
        let mut fields = HashMap::new();
        if !self.sub_frequency.is_empty() {
            let duration = match self.sub_frequency.as_str() {
                "minutes_5" => "5-min",
                "minutes_15" => "15-min",
                "hours_1" => "hourly",
                other => other,
            };
            fields.insert("duration".into(), serde_json::Value::String(duration.into()));
        }
        if !self.symbols.is_empty() && !(self.symbols.len() == 1 && self.symbols[0] == "*") {
            let tickers: Vec<serde_json::Value> = self
                .symbols
                .iter()
                .map(|s| serde_json::Value::String(s.to_lowercase()))
                .collect();
            fields.insert("ticker".into(), serde_json::Value::Array(tickers));
        }
        if !fields.is_empty() {
            self.filters = Some(FiltersConfig { fields });
        }
    }

    pub fn get_filters(&self) -> HashMap<String, serde_json::Value> {
        let mut config = self.clone();
        config.migrate_legacy();
        config.filters.map(|f| f.fields).unwrap_or_default()
    }
}

/// 策略配置
///
/// 控制方向性对冲策略的行为参数。
/// 所有参数都有默认值，可在配置文件的 [strategy] 节覆盖。
#[derive(Debug, Clone, Deserialize)]
pub struct StrategyConfig {
    /// 策略名称（用于日志标识）
    #[serde(default = "default_strategy_name")]
    pub name: String,
    /// 合约数量（每笔订单的合约数）
    #[serde(default = "default_contract_count")]
    pub contract_count: f64,
    /// 结算后等待赎回秒数
    #[serde(default = "default_settle_redeem_delay")]
    pub settle_redeem_delay_secs: u64,
    /// 排除的币种列表（不交易这些币种）
    #[serde(default = "default_excluded_tickers")]
    pub excluded_tickers: Vec<String>,
    /// 最大同时入场数（防止资金分散）
    #[serde(default = "default_max_active_markets")]
    pub max_active_markets: usize,
    /// ask 价格区间 [min, max]（best_ask 在此区间内才入场）
    #[serde(default = "default_ask_range")]
    pub ask_range: Vec<f64>,
    /// 最小 spread（spread > 此值才入场）
    #[serde(default = "default_min_spread")]
    pub min_spread: f64,
    /// 价格精度（最小价格单位，用于计算大单挂单价）
    #[serde(default = "default_price_tick")]
    pub price_tick: f64,
    /// 入场时间窗口 [min, max]（秒）
    /// 剩余时间在此区间内才入场
    #[serde(default = "default_settle_time_range")]
    pub settle_time_range: Vec<u64>,
    /// Oracle 价格最大允许延迟（秒）
    #[serde(default = "default_oracle_max_age_secs")]
    pub oracle_max_age_secs: u64,
    /// 是否要求 oracle 价格必须新鲜（true 时过期/缺失直接跳过）
    #[serde(default = "default_require_fresh_oracle")]
    pub require_fresh_oracle: bool,
    /// 当前 oracle 相对 open_price 的最小偏离比例（0.0005 = 0.05%）
    #[serde(default = "default_min_oracle_move_pct")]
    pub min_oracle_move_pct: f64,
    /// 挂单后等待状态查询的延迟（毫秒）
    #[serde(default = "default_order_status_delay")]
    pub order_status_delay_ms: u64,
    /// API 重试次数
    #[serde(default = "default_max_retries")]
    pub max_retries: u32,
    /// 重试间隔（毫秒）
    #[serde(default = "default_retry_delay")]
    pub retry_delay_ms: u64,
    /// 赎回后从大钱包转给小钱包的 USDC 数量
    #[serde(default = "default_post_redeem_transfer_usdc")]
    pub post_redeem_transfer_usdc: f64,
    /// 最大允许亏损比例（0.05 = 5%）
    #[serde(default = "default_max_loss_pct")]
    pub max_loss_pct: f64,
    /// 启动前每个钱包最低 Base 原生 ETH 余额
    #[serde(default = "default_min_eth_balance")]
    pub min_eth_balance: f64,
    /// 大单价格覆盖（可选，测试用）
    ///
    /// - 不配置（None）：用实时 best_ask - price_tick
    /// - 配置固定值（如 0.5）：直接用这个价格，降低成本用于测试
    #[serde(default)]
    pub gtc_price_override: Option<f64>,
}

fn default_strategy_name() -> String { "directional_hedge".into() }
fn default_contract_count() -> f64 { 10.0 }
fn default_settle_redeem_delay() -> u64 { 30 }
fn default_excluded_tickers() -> Vec<String> { vec!["xrp".into(), "doge".into()] }
fn default_max_active_markets() -> usize { 5 }
fn default_ask_range() -> Vec<f64> { vec![0.989, 1.0] }
fn default_min_spread() -> f64 { 0.001 }
fn default_price_tick() -> f64 { 0.001 }
fn default_settle_time_range() -> Vec<u64> { vec![3, 300] }
fn default_oracle_max_age_secs() -> u64 { 3 }
fn default_require_fresh_oracle() -> bool { true }
fn default_min_oracle_move_pct() -> f64 { 0.0 }
fn default_order_status_delay() -> u64 { 500 }
fn default_max_retries() -> u32 { 2 }
fn default_retry_delay() -> u64 { 200 }
fn default_post_redeem_transfer_usdc() -> f64 { 10.0 }
fn default_max_loss_pct() -> f64 { 0.05 }
fn default_min_eth_balance() -> f64 { 0.00056 }

impl Default for StrategyConfig {
    fn default() -> Self {
        Self {
            name: default_strategy_name(),
            contract_count: default_contract_count(),
            settle_redeem_delay_secs: default_settle_redeem_delay(),
            excluded_tickers: default_excluded_tickers(),
            max_active_markets: default_max_active_markets(),
            ask_range: default_ask_range(),
            min_spread: default_min_spread(),
            price_tick: default_price_tick(),
            settle_time_range: default_settle_time_range(),
            oracle_max_age_secs: default_oracle_max_age_secs(),
            require_fresh_oracle: default_require_fresh_oracle(),
            min_oracle_move_pct: default_min_oracle_move_pct(),
            order_status_delay_ms: default_order_status_delay(),
            max_retries: default_max_retries(),
            retry_delay_ms: default_retry_delay(),
            post_redeem_transfer_usdc: default_post_redeem_transfer_usdc(),
            max_loss_pct: default_max_loss_pct(),
            min_eth_balance: default_min_eth_balance(),
            gtc_price_override: None,
        }
    }
}

/// 日志配置
#[derive(Debug, Clone, Deserialize)]
pub struct LoggingConfig {
    #[serde(default = "default_log_level")]
    pub level: String,
}
fn default_log_level() -> String { "info".into() }
impl Default for LoggingConfig {
    fn default() -> Self { Self { level: "info".into() } }
}

/// 应用配置
#[derive(Debug, Clone, Deserialize)]
pub struct AppConfig {
    pub hedge: HedgeConfig,
    pub account_a: AccountConfig,
    pub account_b: AccountConfig,
    /// 策略配置（可选，不填则使用默认值）
    #[serde(default)]
    pub strategy: Option<StrategyConfig>,
    #[serde(default)]
    pub logging: LoggingConfig,
    #[serde(default = "default_rpc_url")]
    pub rpc_url: String,
}

fn default_rpc_url() -> String { "https://mainnet.base.org".into() }

impl AppConfig {
    pub fn load_from_file(path: impl AsRef<std::path::Path>) -> anyhow::Result<Self> {
        let raw = std::fs::read_to_string(path)?;
        let mut cfg: Self = toml::from_str(&raw)?;
        cfg.hedge.migrate_legacy();
        Ok(cfg)
    }
}
