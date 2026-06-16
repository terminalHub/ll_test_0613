use std::collections::{HashMap, HashSet};

use tracing::{debug, info, warn};

use crate::domain::config::HedgeConfig;
use crate::domain::market::MarketInfo;

/// 市场发现器 - 阶段 1 版本
pub struct MarketDiscovery {
    config: HedgeConfig,
    known_slugs: HashSet<String>,
}

/// 扫描结果
#[derive(Debug)]
pub struct ScanResult {
    /// 新发现的市场
    pub new_markets: Vec<MarketInfo>,
    /// 消失的市场 slug
    pub removed_slugs: Vec<String>,
    /// 当前所有市场
    pub all_markets: Vec<MarketInfo>,
}

impl MarketDiscovery {
    pub fn new(config: HedgeConfig) -> Self {
        info!(
            market_category = %config.market_category,
            has_filters = config.filters.is_some(),
            has_markets = config.markets.is_some(),
            "市场发现器已创建"
        );
        Self {
            config,
            known_slugs: HashSet::new(),
        }
    }

    /// 扫描市场
    pub async fn scan(
        &mut self,
        client: &limitless_exchange_rust_sdk::Client,
    ) -> anyhow::Result<ScanResult> {
        if let Some(slugs) = self.config.markets.clone() {
            return self.scan_by_slugs(client, &slugs).await;
        }
        self.scan_by_filters(client).await
    }

    async fn scan_by_slugs(
        &mut self,
        client: &limitless_exchange_rust_sdk::Client,
        slugs: &[String],
    ) -> anyhow::Result<ScanResult> {
        let mut current_slugs = HashSet::new();
        let mut all_markets = Vec::new();

        for slug in slugs {
            current_slugs.insert(slug.clone());
            if !self.known_slugs.contains(slug) {
                if let Some(info) = self.fetch_market_info(client, slug).await {
                    all_markets.push(info);
                }
            }
        }

        let new_slugs: Vec<String> = current_slugs
            .difference(&self.known_slugs)
            .cloned()
            .collect();
        let removed_slugs: Vec<String> = self
            .known_slugs
            .difference(&current_slugs)
            .cloned()
            .collect();

        self.known_slugs = current_slugs;

        info!(
            new = new_slugs.len(),
            removed = removed_slugs.len(),
            "市场扫描完成（指定 slug 模式）"
        );
        Ok(ScanResult {
            new_markets: all_markets,
            removed_slugs,
            all_markets: Vec::new(),
        })
    }

    async fn scan_by_filters(
        &mut self,
        client: &limitless_exchange_rust_sdk::Client,
    ) -> anyhow::Result<ScanResult> {
        info!(category = %self.config.market_category, "开始扫描市场...");

        let page = match client
            .pages
            .get_market_page_by_path(&self.config.market_category)
            .await
        {
            Ok(p) => p,
            Err(e) => {
                warn!(error = %e, "获取市场类别页面失败");
                return Ok(ScanResult {
                    new_markets: Vec::new(),
                    removed_slugs: Vec::new(),
                    all_markets: Vec::new(),
                });
            }
        };

        let filters = self.config.get_filters();
        debug!(?filters, "筛选条件");

        let result = match client
            .pages
            .get_markets(
                &page.id,
                Some(&limitless_exchange_rust_sdk::MarketPageMarketsParams {
                    limit: Some(100),
                    sort: Some(limitless_exchange_rust_sdk::MarketPageSort::DeadlineDesc),
                    filters,
                    ..Default::default()
                }),
            )
            .await
        {
            Ok(r) => r,
            Err(e) => {
                warn!(error = %e, "获取市场列表失败");
                return Ok(ScanResult {
                    new_markets: Vec::new(),
                    removed_slugs: Vec::new(),
                    all_markets: Vec::new(),
                });
            }
        };

        let mut current_slugs = HashSet::new();
        let mut all_markets = Vec::new();
        let mut new_markets = Vec::new();

        for market in &result.data {
            let slug = market.slug.clone();
            current_slugs.insert(slug.clone());

            let tokens = match market.tokens.as_ref() {
                Some(t) => t,
                None => {
                    debug!(slug = %market.slug, "市场无 token IDs，跳过");
                    continue;
                }
            };

            let ticker = slug
                .split('-')
                .next()
                .unwrap_or("")
                .to_uppercase();

            let expiration_ts = market.expiration_timestamp / 1000;

            let liquidity = market
                .liquidity
                .as_ref()
                .and_then(|s| s.parse::<f64>().ok())
                .unwrap_or(0.0);

            // ✅ 从 metadata 提取 open_price
            let open_price = market
                .metadata
                .open_price
                .as_ref()
                .and_then(|s| s.parse::<f64>().ok());

            // ✅ 从 settings 提取 min_size（原始值转 USDC）
            let min_size = market
                .settings
                .as_ref()
                .and_then(|s| s.min_size.parse::<f64>().ok())
                .map(|v| v / 1_000_000.0);

            let info = MarketInfo {
                slug: slug.clone(),
                yes_token_id: tokens.yes.clone(),
                no_token_id: tokens.no.clone(),
                ticker,
                expiration_ts,
                liquidity,
                market_type: market.trade_type.clone(),
                categories: market.categories.clone(),
                open_price,
                min_size,
                condition_id: market.condition_id.clone(),
            };

            all_markets.push(info.clone());

            if !self.known_slugs.contains(&slug) {
                new_markets.push(info);
            }
        }

        let removed_slugs: Vec<String> = self
            .known_slugs
            .difference(&current_slugs)
            .cloned()
            .collect();

        self.known_slugs = current_slugs;

        info!(
            new = new_markets.len(),
            removed = removed_slugs.len(),
            total = all_markets.len(),
            "市场扫描完成"
        );

        Ok(ScanResult {
            new_markets,
            removed_slugs,
            all_markets,
        })
    }

    /// 获取单个市场信息
    async fn fetch_market_info(
        &self,
        client: &limitless_exchange_rust_sdk::Client,
        slug: &str,
    ) -> Option<MarketInfo> {
        let page = client
            .pages
            .get_market_page_by_path(&self.config.market_category)
            .await
            .ok()?;

        let mut filters = HashMap::new();
        filters.insert(
            "slug".to_string(),
            serde_json::Value::String(slug.to_string()),
        );

        let result = client
            .pages
            .get_markets(
                &page.id,
                Some(&limitless_exchange_rust_sdk::MarketPageMarketsParams {
                    limit: Some(1),
                    filters,
                    ..Default::default()
                }),
            )
            .await
            .ok()?;

        let market = result.data.first()?;
        let tokens = market.tokens.as_ref()?;

        let ticker = slug.split('-').next().unwrap_or("").to_uppercase();
        let expiration_ts = market.expiration_timestamp / 1000;
        let liquidity = market
            .liquidity
            .as_ref()
            .and_then(|s| s.parse::<f64>().ok())
            .unwrap_or(0.0);

        let open_price = market
            .metadata
            .open_price
            .as_ref()
            .and_then(|s| s.parse::<f64>().ok());

        let min_size = market
            .settings
            .as_ref()
            .and_then(|s| s.min_size.parse::<f64>().ok())
            .map(|v| v / 1_000_000.0);

        Some(MarketInfo {
            slug: slug.to_string(),
            yes_token_id: tokens.yes.clone(),
            no_token_id: tokens.no.clone(),
            ticker,
            expiration_ts,
            liquidity,
            market_type: market.trade_type.clone(),
            categories: market.categories.clone(),
            open_price,
            min_size,
            condition_id: market.condition_id.clone(),
        })
    }

    pub fn known_count(&self) -> usize {
        self.known_slugs.len()
    }
}
