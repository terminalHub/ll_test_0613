use tokio::sync::broadcast;
use tracing::info;

use crate::domain::event::StatEvent;

/// 统计数据收集器
///
/// 通过 `broadcast::Receiver<StatEvent>` 订阅交易事件，
/// 内部聚合：胜率、盈亏、订单成功率等。
pub struct StatsCollector {
    rx: broadcast::Receiver<StatEvent>,
    data: StatsData,
}

#[derive(Debug, Default)]
struct StatsData {
    total_trades: u64,
    winning_trades: u64,
    losing_trades: u64,
    total_pnl: f64,
    order_failures: u64,
    total_redeemed: f64,
    scan_count: u64,
    last_scan_markets: usize,
}

/// 统计摘要
#[derive(Debug, Clone)]
pub struct StatsSummary {
    pub total_trades: u64,
    pub win_rate: f64,
    pub total_pnl: f64,
    pub avg_pnl: f64,
    pub order_failures: u64,
    pub total_redeemed: f64,
    pub scan_count: u64,
    pub last_scan_markets: usize,
}

impl StatsCollector {
    pub fn new(rx: broadcast::Receiver<StatEvent>) -> Self {
        info!("统计收集器已创建");
        Self {
            rx,
            data: StatsData::default(),
        }
    }

    /// 处理所有待处理的统计事件
    pub fn poll(&mut self) {
        loop {
            match self.rx.try_recv() {
                Ok(event) => self.handle_event(event),
                Err(broadcast::error::TryRecvError::Empty) => break,
                Err(broadcast::error::TryRecvError::Lagged(n)) => {
                    tracing::warn!(skipped = n, "统计事件丢失");
                }
                Err(broadcast::error::TryRecvError::Closed) => break,
            }
        }
    }

    fn handle_event(&mut self, event: StatEvent) {
        match event {
            StatEvent::TradeExecuted { pnl, .. } => {
                self.data.total_trades += 1;
                self.data.total_pnl += pnl;
                if pnl > 0.0 {
                    self.data.winning_trades += 1;
                } else if pnl < 0.0 {
                    self.data.losing_trades += 1;
                }
            }
            StatEvent::ScanComplete { markets_found } => {
                self.data.scan_count += 1;
                self.data.last_scan_markets = markets_found;
            }
            StatEvent::OrderFailed { .. } => {
                self.data.order_failures += 1;
            }
            StatEvent::Redeemed { amount, .. } => {
                self.data.total_redeemed += amount;
            }
        }
    }

    pub fn summary(&self) -> StatsSummary {
        let win_rate = if self.data.total_trades > 0 {
            self.data.winning_trades as f64 / self.data.total_trades as f64
        } else {
            0.0
        };
        let avg_pnl = if self.data.total_trades > 0 {
            self.data.total_pnl / self.data.total_trades as f64
        } else {
            0.0
        };
        StatsSummary {
            total_trades: self.data.total_trades,
            win_rate,
            total_pnl: self.data.total_pnl,
            avg_pnl,
            order_failures: self.data.order_failures,
            total_redeemed: self.data.total_redeemed,
            scan_count: self.data.scan_count,
            last_scan_markets: self.data.last_scan_markets,
        }
    }
}
