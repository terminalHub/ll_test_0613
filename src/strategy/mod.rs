use async_trait::async_trait;

use crate::domain::event::PriceEvent;
use crate::domain::market::MarketInfo;
use crate::domain::order::{AccountId, OrderRequest, OrderResult};

/// 策略 trait
///
/// 所有策略实现此 trait。主循环将市场事件传递给策略，
/// 策略返回需要执行的下单请求（`Vec<OrderRequest>`）。
///
/// 设计原则：
/// - 策略只负责"是否下单"和"参数"，不负责 SDK 调用
/// - 执行器只负责"怎么下单"，不关心策略逻辑
/// - 通过 `OrderRequest` 解耦
/// - 订单执行结果通过 `on_order_result` 反馈，策略据此推进内部状态机
#[async_trait]
pub trait Strategy: Send + Sync {
    /// 策略名称（用于日志标识）
    fn name(&self) -> &str;

    /// 处理价格事件，返回需要执行的下单请求
    async fn on_event(&mut self, event: &PriceEvent) -> Vec<OrderRequest>;

    /// 定时心跳回调（每秒）
    ///
    /// 用于检查赎回、超时撤单等定时任务。
    async fn on_tick(&mut self) -> Vec<OrderRequest>;

    /// 订单执行结果反馈（每笔订单执行后调用）
    ///
    /// 策略据此推进市场状态机：大单成功 → BigPlaced，小单成功 → Hedged，
    /// 赎回成功 → 移除市场。失败则触发撤单或允许重试。
    async fn on_order_result(&mut self, _result: &OrderResult) {}

    /// 新市场发现通知
    ///
    /// MarketDiscovery 扫描到新市场时调用，让策略做初始化准备。
    fn on_market_discovered(&mut self, markets: &[MarketInfo]);

    /// 获取指定市场的大单钱包账号
    ///
    /// 当小单失败需要撤大单时，主循环调用此方法确定撤哪个账号的单。
    fn big_wallet_for_market(&self, slug: &str) -> Option<AccountId>;

    /// 获取指定市场的大单订单 ID（撤单用）
    fn big_order_id_for_market(&self, _slug: &str) -> Option<String> {
        None
    }

    /// 初始化钱包余额信息
    ///
    /// 启动时查询链上余额后调用，策略据此决定大单/小单钱包分配。
    fn init_wallets(&mut self, balance_a: f64, balance_b: f64);
}

pub mod directional;
