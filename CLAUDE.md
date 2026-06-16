# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## What this is

A directional hedging bot (方向性对冲系统) for the Limitless Exchange — prediction markets settled on the Base chain. The goal is **volume farming for airdrop eligibility, not spread profit**: it opens hedged positions across two accounts so net exposure is near-zero while generating trade volume.

Code, comments, and the `tests/stageN/` docs are written in Chinese. The SDK (`limitless-exchange-rust-sdk`) handles EIP-712 order signing and the market/orderbook REST + WS APIs.

## Build, run, test

```bash
cargo build
cargo run -- <config-path>          # defaults to tests/resource/test.toml if omitted
cargo test                          # offline unit + integration tests
cargo test --test stage5_strategy -- --nocapture   # one test file
cargo test test_direction_yes_when_oracle_up -- --nocapture  # one test by name
```

Network tests hit the live exchange and on-chain RPC, so they are gated behind `#[ignore]` and require real credentials in `tests/resource/test.toml`:

```bash
cargo test --test stage2_fok_order -- --ignored --nocapture
```

Generating API keys (stage0) needs a `LIMITLESS_IDENTITY_TOKEN` env var (grab `privy:token` from limitless.exchange localStorage in the browser).

## Configuration

`AppConfig::load_from_file` parses TOML (see `config/default.example.toml`, `tests/resource/test.example.toml`). Real configs (`test.toml`, `default.toml`, `production.toml`) hold private keys and HMAC secrets and are **not committed** — only `.example.toml` templates are. Copy a template and fill in credentials before running.

Auth priority per account (`LimitlessApi::new`): HMAC (`hmac_token_id` + `hmac_secret`) → API Key (`api_key`) → env `LIMITLESS_API_KEY`. `private_key` is separate and required for order signing and on-chain calls.

`HedgeConfig::migrate_legacy` maps old fields (`sub_frequency`, `symbols`) into the newer `[hedge.filters]` form — keep this when touching config parsing.

## Architecture

The system is a single `tokio::select!` loop in `main.rs` driving four event sources:

```
MarketDiscovery (30–60s timer) → WsClient (live price events) → Strategy → OrderExecutor → StatsCollector
                                  Heartbeat (1s) → Strategy.on_tick() → redeem checks
                                  Ctrl+C → graceful exit + stats summary
```

**Central design rule (do not break this):** the Strategy decides *whether to trade and with what parameters*; the Executor decides *how to place the order*. They are decoupled through the `OrderRequest` value type. Strategy code never calls the SDK directly.

Layers:

- **`domain/`** — pure data types, no I/O. `order` (`OrderRequest`, `OrderResult`, `AccountId::{A,B}`, `Side`, `OrderKind::{Fok,Fak,Gtc,Redeem}`), `market` (`MarketInfo`), `event` (`PriceEvent`, `MarketEvent`, `StatEvent`), `config` (all `*Config` structs + serde defaults).
- **`strategy/`** — `Strategy` trait + `DefaultDirectionalStrategy`. Produces `Vec<OrderRequest>` from price events; holds market/oracle caches and the big/small wallet assignment. This is where trading logic lives.
- **`executor.rs`** — `OrderExecutor` maps each `OrderRequest` to an SDK call (`execute_fok`/`fak`/`gtc`) or on-chain redeem, selecting account A or B. Also handles USDC `approve`. Supports single-account mode (B unconfigured).
- **`infrastructure/`** — `limitless` (`LimitlessApi` wraps the SDK `Client`: auth, `order_client`, `redeem`), `http_client` (`HmacHttpClient` for endpoints the SDK doesn't cover, with hand-rolled HMAC-SHA256 signing).
- **`discovery/`** — `MarketDiscovery::scan` lists markets by category+filters (or explicit slugs), diffs against known slugs, returns new/removed/all.
- **`monitor/`** — `WsClient` broadcasts `PriceEvent`s over a `tokio::broadcast` channel. NOTE: real WS subscription wiring is still a TODO in `main.rs` (`update_subscriptions` computes the diff but doesn't yet push subscribe/unsubscribe to the SDK socket).
- **`stats/`** — `StatsCollector` aggregates `StatEvent`s (win rate, PnL, failures, redeemed) for the shutdown summary.
- **`utils/onchain.rs`** — `BaseChainClient`: raw Base-chain JSON-RPC. USDC balance/allowance/approve, condition-resolved check, and manual EIP-155 transaction construction + signing (k256 + rlp + keccak256). Base mainnet chainId 8453 (0x2105); contract addresses and 6-decimal USDC are constants at the top.

### Strategy decision flow (`DefaultDirectionalStrategy`)

On each orderbook event, entry requires all of: market not already active, ticker not excluded (xrp/doge by default), `best_ask > min_ask_threshold` (0.989), `spread > min_spread` (0.001), remaining time within `settle_time_range`, and under `max_active_markets`. Direction: `oracle_price >= open_price` → YES wins, else NO (fallback when no oracle: `best_ask > 0.5` → YES).

On entry it emits two orders: the **big wallet** (higher USDC balance) places a **GTC buy on the winning side at `best_ask - price_tick`** (sits inside the spread, protecting capital from immediate fill), and the **small wallet** places a **FOK buy on the losing side at `best_ask + price_tick`** (slippage-protected, fills immediately to hedge). If the small FOK fails, `main.rs` cancels the big order via `big_wallet_for_market` to avoid one-sided exposure. `on_tick` emits `Redeem` requests for both wallets once a market is past expiry + `settle_redeem_delay_secs`.

## Test layout and conventions

Tests are organized by build **stage** (stage0 → stage5), mirroring how the system was built up: stage0 API-key generation, stage1 auth/config/scan, stage2 order placement (FOK/GTC), stage3 redeem, stage4 WS/scan, stage5 strategy integration. Each stage has a `tests/stageN/ISSUES.md` recording known bugs and verification notes, and `stage5/PLAN.md` documents the integration design.

When investigating or modifying a subsystem, **read its stage's `ISSUES.md` first** — they capture non-obvious findings. In particular, `tests/stage3/ISSUES.md` documents that **redeem is known-incomplete**: the executor passes `market_slug` where `condition_id` is expected, `OrderRequest` lacks a `condition_id` field, and the manual on-chain redeem in `infrastructure/limitless.rs` won't work for ERC-4337 server wallets (the fix is to use the SDK's `server_wallets.redeem_positions`). Treat redeem as not-yet-working unless these are resolved.
