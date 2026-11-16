# Running Tempest on a Hetzner server

Why: Railway's outbound IPs are shared with other customers, and their traffic
got the bot rate-limited and then banned by Binance for eight hours
(2–3 Nov 2026). A small VPS has an IP of its own. One server runs both the
futures bot (always on) and the nightly lab (cron, 01:17 UTC).

What is where on the server:

| Path | What |
|---|---|
| `/opt/tempest` | This repo, kept at `origin/master` |
| `/etc/tempest/bot.env`, `lab.env` | Settings and secrets (mode 600, never in git) |
| `/var/lib/tempest/bot/risk-state.json` | Today's drawdown baseline and halt, kept across restarts. Delete it and run `deploy.sh --force` to lift a halt by hand |
| `tempest-deploy.timer` | Every 5 min: if master moved, rebuild and restart the bot |
| `/etc/cron.d/tempest-lab` | Nightly lab, output in `/var/log/tempest/lab-YYYY-MM-DD.log` |

Every push to master goes live within about 5 minutes (plus the build, a few
minutes). A failed build leaves the running bot untouched and is retried.

## 1. Create the server

Hetzner Cloud → new server:
- **Location:** an EU location such as Falkenstein, Nuremberg or Helsinki.
  Binance refuses futures from restricted locations; the setup script tests
  this and warns if the server's location is refused.
- **Image:** Ubuntu 24.04. **Type:** CX22 (2 vCPU, 4 GB), about €4–5/month.
- **SSH key:** add yours. No password login is set up.

## 2. Run the setup

```bash
ssh root@<server-ip>
curl -fsSL https://raw.githubusercontent.com/tintun2602/tempest/master/deploy/hetzner/setup.sh | bash
```

It installs Docker, a firewall (SSH only), automatic security updates and
2 GB swap, clones the repo, and prints the server's IP at the end.

## 3. Binance: allow the new IP

Binance → API Management → your key → edit IP restriction → add the server's
IP. Keep the Railway IPs until the move is done, then remove them.

## 4. Fill in the settings

```bash
nano /etc/tempest/bot.env
```

Copy every value from Railway → tempest-bot → Variables, including the keys
and the Telegram token. `lab.env` is already filled in; adjust it only if the
lab should test something else.

## 5. Switch over

Only one bot may run at a time, or both will trade the same account.

1. **Railway:** stop tempest-bot (Settings → remove the deployment, or scale
   to 0). Also stop tempest-lab's cron.
2. **Server:**
   ```bash
   /opt/tempest/deploy/hetzner/deploy.sh --force
   systemctl enable --now tempest-deploy.timer
   docker compose -f /opt/tempest/deploy/hetzner/docker-compose.yml logs -f bot
   ```
   The first build takes a few minutes. Telegram gets the usual
   "Tempest futures started" message. Open positions carry over: the bot reads
   them from Binance.

When it has run cleanly for a day, cancel Railway Pro and remove the Railway
IPs from the Binance key.

## FreqAI (optional, live)

[Freqtrade](https://www.freqtrade.io) with FreqAI: a LightGBM model retrained
every 6 hours on 60 days of 1h candles, trading long and short on its own
pairs (SUI, ENA, AAVE, NEAR). Settings are in `freqai/config.json`, the
strategy in `freqai/strategies/TempestFreqAI.py`.

- **It trades real money** (`dry_run: false`), from the same futures wallet
  as Tempest: 20 USDC per trade at 3x, at most 2 trades, stop on Binance at a
  5% price move. Its losses count toward Tempest's daily drawdown halt.
- **Never give it Tempest's pairs.** Tempest adopts any position on its own
  10 pairs and would manage, or close, FreqAI's trades.
- It is built on Freqtrade's example strategy, which its authors call a
  showcase rather than a production strategy, and it has not been
  backtested here.

To turn it on:

```bash
cp /opt/tempest/deploy/hetzner/freqai-secrets.example.json /etc/tempest/freqai-secrets.json
nano /etc/tempest/freqai-secrets.json   # Binance key/secret, Telegram token/chat
chown 1000:1000 /etc/tempest/freqai-secrets.json && chmod 600 /etc/tempest/freqai-secrets.json
/opt/tempest/deploy/hetzner/deploy.sh --force
```

The first start downloads history and trains before it trades (several
minutes). Telegram then gets Freqtrade's own messages, and `/status`,
`/profit` and `/stop` work in the chat. To turn it off:
`docker compose -f /opt/tempest/deploy/hetzner/docker-compose.yml --profile freqai stop freqai`
and remove the secrets file so deploys do not start it again.

## Changing the server from git

`deploy.sh` runs `apply.sh` on every deploy, so server-side setup that
changes over time (cron jobs and the like) lives in the repo and reaches the
server by pushing to master. Secrets stay in `/etc/tempest` and are changed on
the server itself.

## Status reports (server → GitHub)

`status/report.sh` pushes `status.md` to the private repo
[tintun2602/diablo](https://github.com/tintun2602/diablo) every hour and a
minute after every deploy: bot state, deployed commit, trades and errors
from the bot's log, and the settings without keys, secrets or tokens (and
anything key-shaped is blanked). It needs a deploy key once:

```bash
ssh-keygen -t ed25519 -N "" -C "tempest-server -> diablo" -f /root/.ssh/diablo_deploy
cat /root/.ssh/diablo_deploy.pub
```

Add the printed line on GitHub: diablo → Settings → Deploy keys → Add deploy
key, tick **Allow write access**. Then `/opt/tempest/deploy/hetzner/status/report.sh`
(or wait for the hour). Log: `/var/log/tempest/status.log`.

## Profit withdrawal and top-up (off until set)

The bot can move USDC between the futures and spot wallets itself. Both are
off until the numbers are set in `/etc/tempest/bot.env` (or `settings.env`),
and both need two extra permissions on the Binance API key: **Enable Spot &
Margin Trading** and **Permit Universal Transfer** (withdrawals stay off; the
key stays IP-restricted to this server).

| Setting | Meaning |
|---|---|
| `SKIM_ABOVE_USDC` | Skim when futures equity is above this, and no position or resting order exists. |
| `SKIM_KEEP_USDC` | What stays in futures after a skim (default 0). The bot moves `available - keep`, in whole cents, at least 5 USDC, never more than available. |
| `SKIM_ASSET` | What leaves futures: `USDC` (default) or the collateral the account holds, `BNB`, `BTC` or `ETH` (the only ones Binance lets out of a credits-mode wallet besides USDC). For a non-USDC asset the bot prices it fresh against USDC, takes 98 % of the excess (so haircut and rounding never leave equity under the keep amount), never more than the wallet holds, and books the transfer at what equity actually fell by. |
| `SKIM_TO` | `USDC`: sell a non-USDC skim in spot (`<ASSET>USDC` market sell, rounded to the step, held to the minimum order). A sale that cannot be made leaves the asset in spot. |
| `SKIM_BUY` | Optional, e.g. `BNB:50,ETH:50`: percent of the skimmed amount spent on `<COIN>USDC` spot market buys (`quoteOrderQty`). Empty keeps USDC. A share under the symbol's minimum order, or a refused buy, stays as USDC; the withdrawal is not undone. |
| `TOPUP_BELOW_USDC` | Top up when futures equity is below this. |
| `TOPUP_TO_USDC` | Fill up to this equity from USDC already in spot. Never sells coins. |
| `SPOT_BASE_URL` | Spot and `/sapi` host, default `https://api.binance.com`. |

Contradicting settings (keep not below the threshold, top-up target not above
its threshold, a top-up that lands above the skim line) switch the feature off
with a warning in the log. Transfers are recorded in `risk-state.json` as cash
flows, so they never count as day profit or loss against `DAILY_DRAWDOWN_PCT`.
A cycle that moves money ends there; entries resume next cycle. A key without
the permission gives one Telegram message and the feature is off until the next
UTC day; other failures retry after an hour. Moves are logged and sent to
Telegram with amounts and the new balance.

### EEA credits-mode account (BNB as collateral)

Profit is booked in BNFCR, which cannot be transferred or withdrawn; only BTC,
ETH, BNB and USDC can leave the futures wallet. So with BNB as collateral the
bot takes the excess out as BNB, not as the BNFCR profit: the profit stays as
BNFCR margin and equity falls back towards `SKIM_KEEP_USDC`. Example for a
200 stake: `SKIM_ABOVE_USDC=205 SKIM_KEEP_USDC=200 SKIM_ASSET=BNB SKIM_TO=USDC`.
If the excess is larger than the BNB held, only the BNB leaves. Try it with a
small amount first (see the PR for the plan).

## Day to day

| Task | Command |
|---|---|
| Bot logs | `docker compose -f /opt/tempest/deploy/hetzner/docker-compose.yml logs -f --tail 200 bot` |
| FreqAI logs | `tail -f /var/lib/tempest/freqai/logs/freqtrade.log` |
| Last deploys | `journalctl -u tempest-deploy --since today` |
| Change a setting | pairs, max positions and intervals: `deploy/hetzner/settings.env` in git (push to master); the rest: edit `/etc/tempest/bot.env`, then `deploy.sh --force` |
| Stop trading now | `docker compose -f /opt/tempest/deploy/hetzner/docker-compose.yml stop bot` (stops on Binance stay) |
| Close everything | set `FORCE_CLOSE=true` in `bot.env`, `deploy.sh --force` |
| Last lab report | `grep LAB_REPORT /var/log/tempest/lab-$(date +%F).log` |
| Pause auto-deploy | `systemctl stop tempest-deploy.timer` |
