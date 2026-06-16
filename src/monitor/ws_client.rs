//! WebSocket 客户端封装
//!
//! 封装 SDK 的 WebSocket 客户端，提供价格事件广播。
//!
//! 数据流：
//! ```text
//! SDK WebSocketClient ──on_orderbook_update──▶ PriceEvent::Orderbook ─┐
//!                     ──on_oracle_price_data─▶ PriceEvent::OraclePrice ┴─▶ broadcast::Sender
//! ```
//! `connect` 把 SDK 推送的行情转成项目内部的 [`PriceEvent`] 后广播；
//! `update_subscriptions` 在维护本地订阅列表的同时，真正向 SDK 发送 subscribe/unsubscribe。

use tokio::sync::broadcast;
use tracing::{info, warn};

use crate::domain::event::PriceEvent;

/// WebSocket 客户端
///
/// 职责：
/// 1. 连接 Limitless WS（SDK 内部 spawn 后台收消息循环 + 自动重连）
/// 2. 订阅指定市场的 orderbook 和 oracle 价格
/// 3. 将更新转成 [`PriceEvent`] 广播给所有订阅者
pub struct WsClient {
    tx: broadcast::Sender<PriceEvent>,
    subscribed_slugs: Vec<String>,
    /// SDK WebSocket 客户端（`connect` 后注入）。
    /// 未连接时为 `None`，此时 `update_subscriptions` 只维护本地列表。
    sdk_ws: Option<limitless_exchange_rust_sdk::WebSocketClient>,
}

impl WsClient {
    /// 创建 WS 客户端
    pub fn new(capacity: Option<usize>) -> Self {
        let cap = capacity.unwrap_or(1024);
        let (tx, _) = broadcast::channel(cap);
        info!(capacity = cap, "WS 客户端已创建");
        Self {
            tx,
            subscribed_slugs: Vec::new(),
            sdk_ws: None,
        }
    }

    /// 连接 SDK WebSocket 并注册价格回调
    ///
    /// 注册 `orderbookUpdate` 和 `oraclePriceData` 两个回调，把 SDK 数据结构
    /// 转成 [`PriceEvent`] 后广播。随后调用 `connect()`（SDK 内部 spawn 后台
    /// 收消息循环、默认 auto_reconnect、重连后自动 resubscribe）。
    ///
    /// 注意：回调要先注册再 `connect`，避免连接后到注册前的消息丢失。
    pub async fn connect(
        &mut self,
        sdk_ws: limitless_exchange_rust_sdk::WebSocketClient,
    ) -> anyhow::Result<()> {
        // orderbook 回调：取 best_bid/best_ask（bids/asks 首条），零价过滤
        let tx = self.tx.clone();
        sdk_ws.on_orderbook_update(move |u| {
            let bid = u.orderbook.bids.first().map(|b| b.price).unwrap_or(0.0);
            let ask = u.orderbook.asks.first().map(|a| a.price).unwrap_or(0.0);
            if bid <= 0.0 && ask <= 0.0 {
                return;
            }
            let _ = tx.send(PriceEvent::Orderbook {
                slug: u.market_slug,
                bid,
                ask,
                midpoint: u.orderbook.adjusted_midpoint,
                timestamp: chrono::Utc::now().timestamp(),
            });
        });

        // oracle 价格回调：零价过滤
        let tx = self.tx.clone();
        sdk_ws.on_oracle_price_data(move |d| {
            if d.value <= 0.0 {
                return;
            }
            let _ = tx.send(PriceEvent::OraclePrice {
                slug: d.market_slug,
                price: d.value,
                timestamp: chrono::Utc::now().timestamp(),
            });
        });

        sdk_ws
            .connect()
            .await
            .map_err(|e| anyhow::anyhow!("WS 连接失败: {e}"))?;
        info!("SDK WebSocket 已连接");
        self.sdk_ws = Some(sdk_ws);
        Ok(())
    }

    /// 断开 SDK WebSocket（供优雅退出 / 测试使用）
    pub async fn disconnect(&self) {
        if let Some(ws) = &self.sdk_ws {
            if let Err(e) = ws.disconnect().await {
                warn!(error = %e, "WS 断开失败");
            }
        }
    }

    /// 订阅价格事件，返回 receiver
    pub fn subscribe(&self) -> broadcast::Receiver<PriceEvent> {
        self.tx.subscribe()
    }

    /// 当前已订阅的市场 slug 列表
    pub fn subscribed_slugs(&self) -> &[String] {
        &self.subscribed_slugs
    }

    /// 更新订阅列表
    ///
    /// - `add`: 新增订阅的市场
    /// - `remove`: 需要取消订阅的市场 slug
    ///
    /// 先计算与本地 `subscribed_slugs` 的差异并更新本地列表，再把差异真正
    /// 同步到 SDK（订阅 `SubscribeMarketPrices` 频道，免认证）。SDK 调用失败
    /// 只 `warn!` 不中断主循环 —— 下一轮市场发现会重算并重试。
    pub async fn update_subscriptions(
        &mut self,
        add: &[String],
        remove: &[String],
    ) -> SubscriptionDiff {
        let mut to_subscribe = Vec::new();
        let mut to_unsubscribe = Vec::new();

        // 添加
        for slug in add {
            if !self.subscribed_slugs.contains(slug) {
                self.subscribed_slugs.push(slug.clone());
                to_subscribe.push(slug.clone());
            }
        }

        // 移除
        for slug in remove {
            if let Some(pos) = self.subscribed_slugs.iter().position(|s| s == slug) {
                self.subscribed_slugs.remove(pos);
                to_unsubscribe.push(slug.clone());
            }
        }

        // 真正同步到 SDK
        if let Some(ws) = &self.sdk_ws {
            use limitless_exchange_rust_sdk::{SubscriptionChannel, SubscriptionOptions};
            if !to_subscribe.is_empty() {
                if let Err(e) = ws
                    .subscribe(
                        SubscriptionChannel::SubscribeMarketPrices,
                        SubscriptionOptions {
                            market_slugs: to_subscribe.clone(),
                            ..Default::default()
                        },
                    )
                    .await
                {
                    warn!(error = %e, count = to_subscribe.len(), "SDK 订阅失败");
                }
            }
            if !to_unsubscribe.is_empty() {
                if let Err(e) = ws
                    .unsubscribe(
                        SubscriptionChannel::SubscribeMarketPrices,
                        SubscriptionOptions {
                            market_slugs: to_unsubscribe.clone(),
                            ..Default::default()
                        },
                    )
                    .await
                {
                    warn!(error = %e, count = to_unsubscribe.len(), "SDK 取消订阅失败");
                }
            }
        }

        SubscriptionDiff {
            to_subscribe,
            to_unsubscribe,
        }
    }

    /// 处理 orderbook 更新（由外部 WS 回调调用）
    pub fn handle_orderbook_update(&self, slug: &str, bid: f64, ask: f64, midpoint: f64) {
        if bid <= 0.0 && ask <= 0.0 {
            return;
        }

        let event = PriceEvent::Orderbook {
            slug: slug.to_string(),
            bid,
            ask,
            midpoint,
            timestamp: chrono::Utc::now().timestamp(),
        };

        let _ = self.tx.send(event);
    }

    /// 处理 oracle 价格更新（由外部 WS 回调调用）
    pub fn handle_oracle_price(&self, slug: &str, price: f64) {
        if price <= 0.0 {
            return;
        }

        let event = PriceEvent::OraclePrice {
            slug: slug.to_string(),
            price,
            timestamp: chrono::Utc::now().timestamp(),
        };

        let _ = self.tx.send(event);
    }
}

/// WS 订阅变更结果
#[derive(Debug)]
pub struct SubscriptionDiff {
    pub to_subscribe: Vec<String>,
    pub to_unsubscribe: Vec<String>,
}
