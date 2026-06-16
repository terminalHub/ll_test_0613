/// 价格事件（WS 推送）
#[derive(Debug, Clone)]
pub enum PriceEvent {
    /// 订单簿更新
    Orderbook {
        slug: String,
        bid: f64,
        ask: f64,
        midpoint: f64,
        timestamp: i64,
    },
    /// Oracle 价格更新
    OraclePrice {
        slug: String,
        price: f64,
        timestamp: i64,
    },
}

/// 市场事件（WS 推送）
#[derive(Debug, Clone)]
pub enum MarketEvent {
    /// 市场已结算
    Resolved {
        slug: String,
        winning_outcome: Option<i32>,
    },
}

/// 统计事件（策略/执行器发出，StatsCollector 收集）
#[derive(Debug, Clone)]
pub enum StatEvent {
    /// 交易执行完成
    TradeExecuted {
        slug: String,
        pnl: f64,
        timestamp: i64,
    },
    /// 市场扫描完成
    ScanComplete {
        markets_found: usize,
    },
    /// 下单失败
    OrderFailed {
        slug: String,
        error: String,
    },
    /// 赎回完成
    Redeemed {
        slug: String,
        amount: f64,
    },
}
