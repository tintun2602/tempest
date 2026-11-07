# Tempest

Rust trading bot for Binance USDⓈ-M futures (USDC perpetuals), long and
short. The owner (Tintun) treats the account as fun money he has written
off; risk settings are his call. Explain consequences once, then do what he
asks.

## Where it runs

- **Server:** Hetzner CX23, Nuremberg, `2.31.125.21`, Ubuntu 24.04. Repo at
  `/opt/tempest`, settings and secrets in `/etc/tempest/bot.env` (never in
  git). Setup and day-to-day commands: `deploy/hetzner/README.md`.
- **Deploys itself:** a timer runs `deploy/hetzner/deploy.sh` every 5
  minutes; a new commit on `master` is live within ~5 minutes (plus build).
  `deploy.sh` re-executes itself from the new commit and runs `apply.sh`, so
  server-side setup (cron jobs etc.) also changes by pushing to master.
  Pushing to master is therefore a production deploy: run `cargo test` and
  `cargo clippy --all-targets` first.
- **Status back:** the server pushes `status.md` (bot state, trades, errors,
  settings without secrets) to the private repo `tintun2602/diablo` hourly
  and a minute after every deploy. Read it there; there is no other access
  to the server from a cloud session.
- **On the server itself** the owner runs Claude Code as root
  (`ssh tempest`, `cd /opt/tempest && claude`) for secrets and one-off fixes.
- Railway (the old host) is deleted. Its shared IP got the bot rate-limited
  and banned by Binance.

## Live settings (in /etc/tempest/bot.env)

`MARKET=futures`, `STRATEGY=breakout`, `TREND_INTERVAL=1h`,
`SIGNAL_INTERVAL=5m`, `POLL_INTERVAL_SECONDS=300`, `LEVERAGE=20`,
`RISK_PER_TRADE_PCT=80`, `DAILY_DRAWDOWN_PCT=50`, ten pairs (ETH, BTC, SOL,
XRP, BNB, DOGE, ADA, LINK, AVAX, LTC; all `…USDC`). Account ~75 USDC.
Binance key `jmy1…` is IP-restricted to the server, futures on, withdrawals
off.

## Code map

- `src/futures.rs`: the live futures loop (positions and orders read from the
  exchange each cycle; stop + 2R target as Algo Orders; rate-limit back-off).
- `src/exchange/binance_futures.rs`: Binance futures REST adapter.
- `src/strategies.rs`, `src/strategy.rs`: strategies and their short mirrors.
- `src/lab.rs`, `src/archive.rs`: backtest lab (`MODE=lab LAB_MARKET=futures`,
  data from data.binance.vision; `LAB_END_DAYS_AGO` for older periods).
- Spot mode (`src/main.rs`, `executor.rs`) is the original bot, unused now.

## What the lab showed

Out of sample, only `breakout` on 1d trend / 4h signal across ten coins held
up (+0.24%/trade, PF 1.45), and only in trending or falling markets; it lost
in 2023-24. Everything on 15m or faster lost to fees. The owner chose 5m
signals anyway, knowing fees take ~4% of the account per trade at this size.

## Rules

- Never print or commit API keys, secrets or tokens. `diablo` is private
  because it holds trades and equity; `tempest` is public.
- Commits: no model names in messages.
