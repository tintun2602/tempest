#!/usr/bin/env bash
# Server-side setup that may change over time, applied from git on every
# deploy (deploy.sh calls it), so changes reach the server by pushing to
# master instead of by hand. Must stay idempotent and quick. setup.sh remains
# the one-time install; secrets stay in /etc/tempest, never here.
set -euo pipefail
here=/opt/tempest/deploy/hetzner

# Uptime monitoring: sites in monitor/urls.txt and the bot container.
chmod +x "$here/monitor/check.sh"
cat > /etc/cron.d/tempest-monitor <<'CRON'
*/5 * * * * root /opt/tempest/deploy/hetzner/monitor/check.sh >> /var/log/tempest/monitor.log 2>&1
CRON
