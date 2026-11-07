#!/usr/bin/env bash
# Server-side setup that may change over time, applied from git on every
# deploy (deploy.sh calls it), so changes reach the server by pushing to
# master instead of by hand. Must stay idempotent and quick. setup.sh remains
# the one-time install; secrets stay in /etc/tempest, never here.
set -euo pipefail

# The uptime monitor was tried and dropped; remove its cron job if an
# earlier deploy installed it.
rm -f /etc/cron.d/tempest-monitor
