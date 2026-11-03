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

## Day to day

| Task | Command |
|---|---|
| Bot logs | `docker compose -f /opt/tempest/deploy/hetzner/docker-compose.yml logs -f --tail 200 bot` |
| Last deploys | `journalctl -u tempest-deploy --since today` |
| Change a setting | edit `/etc/tempest/bot.env`, then `deploy.sh --force` |
| Stop trading now | `docker compose -f /opt/tempest/deploy/hetzner/docker-compose.yml stop bot` (stops on Binance stay) |
| Close everything | set `FORCE_CLOSE=true` in `bot.env`, `deploy.sh --force` |
| Last lab report | `grep LAB_REPORT /var/log/tempest/lab-$(date +%F).log` |
| Pause auto-deploy | `systemctl stop tempest-deploy.timer` |
