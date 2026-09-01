# Tempest

A Binance swing trading bot.

A Rust-based Binance spot trading bot for swing trading (1-7 day holds). Runs as a persistent polling loop, evaluates technical indicators, enforces strict risk management, and executes trades via the Binance REST API.

## Features

- **Technical Indicators** — EMA(50/200), RSI(14), MACD(12,26,9), swing-low detection, all computed in pure Rust
- **Strategy Engine** — Weighted signal evaluation (trend 40%, momentum 30%, volume 20%, S/R 10%) with strict entry/exit rules
- **Risk Management** — 1.5% max risk per trade, position sizing, 4 max open positions, 5% daily drawdown circuit breaker
- **Order Execution** — Market buy + OCO sell (stop-loss + take-profit) via Binance REST API
- **Startup Reconciliation** — Detects unprotected positions after a crash and places emergency OCO orders
- **Telegram Alerts** — Optional notifications on BUY, SELL, HALT, errors, and startup
- **Backtesting** — Run the strategy against historical klines with a full trade report
- **Deployment** — Dockerfile and fly.toml included for Fly.io

## Project Structure

```
src/
├── main.rs           # Event loop, startup, reconciliation
├── config.rs         # API keys, pairs, risk params (from env)
├── market.rs         # Binance REST client (klines, orders, OCO, account)
├── indicators.rs     # EMA, RSI, MACD, swing-low pivot detection
├── strategy.rs       # Signal logic: BUY / SELL / HOLD / HALT
├── risk.rs           # Position sizing, stop-loss, drawdown circuit breaker
├── executor.rs       # Order placement (market buy + OCO for SL/TP)
├── notify.rs         # Telegram Bot API notifications
├── backtest.rs       # Historical simulation with trade report
Cargo.toml
Dockerfile
fly.toml
```

## Setup

1. Clone and create a `.env` file:

   ```
   BINANCE_API_KEY=your_key
   BINANCE_API_SECRET=your_secret
   BINANCE_BASE_URL=https://testnet.binance.vision
   TRADING_PAIRS=BTCUSDT,ETHUSDT,SOLUSDT
   POLL_INTERVAL_SECONDS=300
   LOG_LEVEL=info
   ```

2. Optional — Telegram alerts:

   ```
   TELEGRAM_BOT_TOKEN=123456:ABC-DEF...
   TELEGRAM_CHAT_ID=your_chat_id
   ```

   To trade a USDC pair instead, set `QUOTE_ASSET=USDC` and use a supported pair,
   for example `TRADING_PAIRS=BTCUSDC`.

3. Build and run:
   ```bash
   cargo run
   ```

## Backtesting

Run the strategy against historical Binance data:

```bash
cargo run -- --backtest
# or
MODE=backtest cargo run
```

Fetches up to 1000 daily + 1000 4H candles per pair and prints a report with win rate, profit factor, max drawdown, a full trade log, and a Monte Carlo stress analysis. The Monte Carlo section resamples historical trade returns to estimate final-balance ranges, drawdown, and losing streaks; it does not predict future performance.

## Trading Logic

All indicators are computed from **closed candles only** — the in-progress bar
is dropped, so a signal cannot appear mid-bar and vanish before the next poll.
The live price comes from the ticker.

### Entry (BUY) — all must be true:

1. Price > EMA(50) > EMA(200) on daily
2. RSI(14) daily within the configured band (default 35–55)
3. MACD bullish on 4H — by default a cross within the last 3 candles
4. Reward-to-risk ratio >= 2.0

### Tuning entry frequency

The defaults are strict: all four conditions hold on roughly 1.4% of days per
symbol, so a single-symbol deployment sees about five signals a year. Two gates
dominate that number, and both are tunable from the environment:

| Variable | Default | Effect |
| --- | --- | --- |
| `RSI_MIN` / `RSI_MAX` | `35` / `55` | The 55 ceiling fights the trend filter — an established uptrend keeps daily RSI nearer 55–70. Raising it is the largest single lever. |
| `MACD_REQUIRE_CROSS` | `true` | `false` accepts MACD merely being above its signal (a state, not an event), which fires far more often. |
| `MACD_LOOKBACK_BARS` | `3` | How recent the cross must be, in 4H bars. |

Widening the universe raises frequency without touching any gate, and is the
one lever that does not trade away selectivity.

Run `cargo run -- --backtest` before enabling any of these: the entry-gate
sweep reports return, trade count, win rate and drawdown for each combination.
More trades is not more profit — each round trip pays roughly 0.3% in fees and
slippage, so frequency amplifies whichever sign your edge actually has.

### Exit (SELL) — any of:

1. Stop-loss hit (nearest swing low)
2. Take-profit hit (2x stop distance)
3. RSI > 70 and MACD histogram negative
4. `FORCE_CLOSE=true` environment variable

### Risk Rules:

- Max risk per trade: 1.5% of equity (`RISK_PER_TRADE_PCT`)
- Position size: `(equity * risk) / stop_distance`, capped at 95% of free cash
- Max simultaneous positions: 4 (`MAX_OPEN_POSITIONS`)
- Daily drawdown halt: portfolio down >5% from day-open halts new trades until
  the next UTC midnight (`DAILY_DRAWDOWN_PCT`)

On spot the sizer is bounded by free cash, so notional is
`equity * risk / stop_distance` capped at 95% of what is uncommitted. With a
typical 6% stop, four positions at 1.5% risk is already ~100% of equity
deployed. Raising `RISK_PER_TRADE_PCT` much past 3% therefore buys no extra
exposure — the sizer clamps and logs `Position size capped by balance`.

## Deployment

### Docker

```bash
docker build -t tempest .
docker run --env-file .env tempest
```

### Fly.io

```bash
fly secrets set BINANCE_API_KEY=... BINANCE_API_SECRET=...
fly deploy
```

## Dependencies

| Crate                            | Purpose                          |
| -------------------------------- | -------------------------------- |
| `reqwest`                        | HTTP client for Binance REST API |
| `tokio`                          | Async runtime                    |
| `serde` / `serde_json`           | JSON serialization               |
| `hmac` / `sha2` / `hex`          | HMAC-SHA256 request signing      |
| `dotenvy`                        | Load `.env` config               |
| `tracing` / `tracing-subscriber` | Structured logging               |

## License

MIT
