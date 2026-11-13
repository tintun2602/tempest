#!/usr/bin/env bash
# Server-side setup that may change over time, applied from git on every
# deploy (deploy.sh calls it), so changes reach the server by pushing to
# master instead of by hand. Must stay idempotent and quick. setup.sh remains
# the one-time install; secrets stay in /etc/tempest, never here.
set -euo pipefail
here=/opt/tempest/deploy/hetzner

# The bot's state directory (mounted into the container; holds the day's
# drawdown baseline and halt across restarts).
install -d /var/lib/tempest/bot

# The uptime monitor was tried and dropped; remove its cron job if an
# earlier deploy installed it.
rm -f /etc/cron.d/tempest-monitor

# Status reports to the private repo tintun2602/diablo: hourly, and once now
# so a deploy's outcome shows up there within a minute. No-op until the
# deploy key /root/.ssh/diablo_deploy exists.
chmod +x "$here/status/report.sh"
cat > /etc/cron.d/tempest-status <<'CRON'
7 * * * * root /opt/tempest/deploy/hetzner/status/report.sh >> /var/log/tempest/status.log 2>&1
CRON
# After the bot has had a minute to start, so the report shows its startup.
# A transient timer, not a background job: systemd ends a oneshot deploy's
# leftover processes when deploy.sh exits.
systemd-run --quiet --on-active=60 --unit="tempest-status-$(date +%s)" \
  "$here/status/report.sh" || true
