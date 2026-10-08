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

   Alerts go out on BUY and SELL (including stops and targets filled on the
   exchange), each cycle that trades ends with an account summary (equity,
   today's P&L, open positions), and HALT, errors and startup are always sent.
   The per-pair "conditions met" status messages are off unless
   `STATUS_ALERTS=true`.

   To trade a USDC pair instead, set `QUOTE_ASSET=USDC` and use a supported pair,
   for example `TRADING_PAIRS=BTCUSDC`.

3. Optional — timeframes and risk limits (defaults shown):

   ```
   TREND_INTERVAL=1d          # EMA50/EMA200 trend, RSI and swing-low stop
   SIGNAL_INTERVAL=4h         # MACD entry trigger
   RISK_PER_TRADE_PCT=1.5     # % of equity lost if the stop is hit
   MAX_OPEN_POSITIONS=4
   DAILY_DRAWDOWN_PCT=5       # % down on the day that halts new entries
   ```

   Intervals take any Binance kline interval (`5m`, `15m`, `1h`, `4h`, `1d`, ...).
   Spot has no leverage, so an entry is also capped at 95% of free cash: with a
   tight stop, the cash cap binds before `RISK_PER_TRADE_PCT` does. The backtest
   still walks `1d`/`4h` candles regardless of these settings.

4. Build and run:
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

### Entry (BUY) — all must be true:

1. Price > EMA(50) > EMA(200) on daily
2. RSI(14) daily between 35 and 55
3. MACD crossed bullish within last 3 four-hour candles
4. Reward-to-risk ratio >= 2.0

### Exit (SELL) — any of:

1. Stop-loss hit (nearest swing low)
2. Take-profit hit (2x stop distance)
3. RSI > 70 and MACD histogram negative
4. `FORCE_CLOSE=true` environment variable

### Risk Rules (hardcoded, non-negotiable):

- Max risk per trade: 1.5% of USDT balance
- Position size: `(balance * 0.015) / stop_distance`
- Max simultaneous positions: 4
- Daily drawdown halt: portfolio down >5% from day-open triggers HALT until next UTC midnight

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
