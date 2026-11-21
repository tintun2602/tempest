# Tempest

Owner: **Naing ENK** (100 %). HQ and standing rules: `tintun2602/playbook`
(`CLAUDE.md`, project register `PROJECTS.md`). In short: keep working and ask
only for what needs a person; check state with a tool before reporting it;
commits authored as Tintun <tintunn26@gmail.com> with no AI attribution or
model names; secrets never in chat, logs or git.

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
  and a minute after every deploy, including the nightly lab's `LAB_REPORT`.
  Read it there; there is no other access
  to the server from a cloud session.
- **On the server itself** the owner runs Claude Code as root
  (`ssh tempest`, `cd /opt/tempest && claude`) for secrets and one-off fixes.
- Railway (the old host) is deleted. Its shared IP got the bot rate-limited
  and banned by Binance.

## Live settings

In git, `deploy/hetzner/settings.env` (wins over bot.env; change by pushing):
`TRADING_PAIRS` = BTC, ETH, BNB, XRP (owner, 2026-11-14 evening; the lab's
ten pairs before that, so its numbers are not confirmed for these four), `MAX_OPEN_POSITIONS=3` (owner, 2026-11-20; two from 2026-11-14 evening),
`TREND_INTERVAL=1d`, `SIGNAL_INTERVAL=4h` (owner, 2026-11-07). Each position
gets at most a third of the margin (equity / MAX_OPEN_POSITIONS).
`BREAKEVEN_ROI_PCT=100`: at +100 % on margin the stop moves to the entry
(owner, 2026-11-10; a stop already at or past the entry is left alone).
`TRAILING_STOP_ATR=2` (owner, 2026-11-20; 3 from 2026-11-19): no fixed target; the stop trails the
mark at 2 x the trend ATR, tightening only, moved at most every 0.1 ATR.
`PAUSE_ENTRIES=true` (owner, 2026-11-21): no new positions; open ones are still managed.
`RISK_PER_TRADE_PCT=30` (owner, 2026-11-14 evening; 20 earlier that day; was 80, which the margin cap held
at about 58 % of equity per stop).

Profit withdrawal and top-up are code but off: `SKIM_ABOVE_USDC`,
`SKIM_KEEP_USDC`, `SKIM_ASSET`, `SKIM_TO`, `SKIM_BUY`, `TOPUP_BELOW_USDC`, `TOPUP_TO_USDC` (`src/wallet.rs`;
details in `deploy/hetzner/README.md`). They need "Enable Spot & Margin Trading"
and "Permit Universal Transfer" on the key. The account is EEA credits mode, but
the collateral has been USDC since 2026-11-16 and a USDC skim worked on the real
account, so `SKIM_ASSET` stays blank (BNB collateral earlier: BNFCR profit is not
transferable, so the skim took BNB and sold it with `SKIM_TO=USDC`).
**Half out** (owner, 2026-11-16, off by default): `SKIM_STAKE_USDC` (start base),
`SKIM_STEP_USDC` (gain that triggers), `SKIM_SHARE_PCT` (share of the gain taken
out, 1-100). At equity >= base + step on a flat account, share % of
`equity - base` goes to spot and the base becomes the equity left; it never falls
and is kept in `wallet-state.json` (a new stake restarts it). It clashes with
`SKIM_ABOVE_USDC`/`SKIM_KEEP_USDC` (skim off). On since 2026-11-16 in
`settings.env`: 210 / 150 / 50 (owner). A manual deposit counts as gain; do not
combine with `TOPUP_*`.

In `/etc/tempest/bot.env` on the server: `MARKET=futures`,
`STRATEGY=breakout`, `POLL_INTERVAL_SECONDS=300`, `LEVERAGE=20`,
`DAILY_DRAWDOWN_PCT=50`, secrets. Account ~75 USDC.
Binance key `jmy1…` is IP-restricted to the server, futures on, withdrawals
off.

## Code map

- `src/futures.rs`: the live futures loop (positions and orders read from the
  exchange each cycle; stop, plus a 2R target unless the trailing stop is on,
  as Algo Orders; rate-limit back-off).
- `src/exchange/binance_futures.rs`: Binance futures REST adapter.
- `src/wallet.rs`, `src/exchange/binance_wallet.rs`: skim to spot and top-up
  from spot (universal transfer), off by default.
- `src/strategies.rs`, `src/strategy.rs`: strategies and their short mirrors.
- `src/lab.rs`, `src/archive.rs`: backtest lab (`MODE=lab LAB_MARKET=futures`,
  data from data.binance.vision; `LAB_END_DAYS_AGO` for older periods).
- `src/meme.rs`: meme-coin paper test (`MODE=memepaper`, container `meme`,
  2026-11-10 for 14 days): DexScreener + RugCheck every 5 min, fixed rules,
  no money. Output in `/var/lib/tempest/meme`, copied to diablo `meme/`.
  Plan: playbook `agents/trading/meme-paper-plan.md`.
- Spot mode (`src/main.rs`, `executor.rs`) is the original bot, unused now.

## What the lab showed

Out of sample, only `breakout` on 1d trend / 4h signal across ten coins held
up (+0.24%/trade, PF 1.45), and only in trending or falling markets; it lost
in 2023-24. Everything on 15m or faster lost to fees. The owner ran 5m
signals for a while (fees took ~4% of the account per trade) and moved to
1d/4h on 2026-11-07.

## Rules

- Never print or commit API keys, secrets or tokens. `diablo` is private
  because it holds trades and equity; `tempest` is public.
- Commits: no model names in messages.
