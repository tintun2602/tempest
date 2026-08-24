# Barter-rs migration — research findings (step 1)

Verified against crates.io on 2026-08-24 by resolving and reading the published
sources, not the README.

## Versions (mutually consistent — these are exactly what `barter 0.14` pulls in)

| Crate | Version |
|---|---|
| barter | 0.14.0 |
| barter-data | 0.13.0 |
| barter-execution | 0.9.0 |
| barter-instrument | 0.3.3 |
| barter-integration | 0.12.0 |

All five resolve together with no version conflicts (272 packages locked).

## Capability audit

| Tempest needs | Barter provides | Evidence |
|---|---|---|
| Binance Spot trades / L1 / L2 book | **Yes** | `Binance<Server>` impls `StreamSelector` for `PublicTrades` + `OrderBooksL1` (`exchange/binance/mod.rs:105,115`); `BinanceSpot` adds `OrderBooksL2` (`spot/mod.rs:41`) |
| Binance Spot **klines / candles** | **No** | `Candles` is a declared `SubscriptionKind` (`subscription/candle.rs:12`) but **no exchange implements `StreamSelector<_, Candles>`** — zero candle streams exist in barter-data 0.13 |
| Binance Spot **execution** | **No** | `barter-execution-0.9.0/src/client/binance/mod.rs` is a **1-byte empty stub**, declared privately as `mod binance;` (`client/mod.rs:21`) |
| **Stop / stop-limit / OCO** orders | **No** | `OrderKind` is `Market \| Limit` only (`order/mod.rs:158-161`). Grep for `oco\|OneCancels` across barter + barter-execution: **no matches** |
| Paper trading (MockExchange) | **Partial** | `exchange/mock/` exists but only special-cases `OrderKind::Market` (`mock/mod.rs:400`) — no trigger/stop/OCO simulation |
| Backtest / risk / strategy scaffolding | **Yes** | `barter-0.14.0/src/{backtest,risk,strategy}/` |

## Consequence for the plan

The two things tempest depends on most are the two barter does not supply:

1. **The strategy is entirely candle-driven** (daily EMA50/EMA200, daily RSI14,
   4H MACD). Barter streams no candles, so **REST klines stay** as the indicator
   source. Barter market data can only *supplement* (live tick price for exits),
   not replace.
2. **Protective exits are OCO.** Barter has no OCO and no stop orders at all, so
   the existing `oco_sell` REST path **must be kept** and cannot be expressed
   through `ExecutionClient`.

Revised approach: barter is adopted for **backtesting, paper trading, and event
plumbing**. The live Binance path stays on the in-house REST client behind the
step-2 traits. Any paper/mock exchange must simulate OCO in our own code, since
`MockExchange` cannot represent it.

This vindicates the plan's own hedge ("Do not assume the generic execution API
handles Binance OCO orders directly") — the answer is that it does not, at all.
