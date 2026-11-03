#!/usr/bin/env bash
# One-time setup of a fresh Ubuntu 24.04 server for Tempest. Run as root:
#   curl -fsSL https://raw.githubusercontent.com/tintun2602/tempest/master/deploy/hetzner/setup.sh | bash
# Safe to run again. It does not start the bot: that waits until
# /etc/tempest/bot.env exists (see deploy/hetzner/README.md).
set -euo pipefail
REPO=https://github.com/tintun2602/tempest.git

echo "== packages"
export DEBIAN_FRONTEND=noninteractive
apt-get update -q
apt-get upgrade -yq
apt-get install -yq docker.io docker-compose-v2 docker-buildx git ufw unattended-upgrades
systemctl enable --now docker
dpkg-reconfigure -f noninteractive unattended-upgrades

echo "== time, swap, firewall"
timedatectl set-timezone UTC
if [[ ! -f /swapfile ]]; then
  # The Rust release build is the one memory-hungry step.
  fallocate -l 2G /swapfile && chmod 600 /swapfile && mkswap /swapfile && swapon /swapfile
  echo '/swapfile none swap sw 0 0' >> /etc/fstab
fi
ufw allow OpenSSH
ufw --force enable
# Key-only SSH. Hetzner installs your key for root when you create the server.
sed -i 's/^#\?PasswordAuthentication .*/PasswordAuthentication no/' /etc/ssh/sshd_config
systemctl reload ssh || systemctl reload sshd || true

echo "== code and directories"
if [[ ! -d /opt/tempest/.git ]]; then
  git clone --quiet "$REPO" /opt/tempest
fi
install -d -m 700 /etc/tempest
install -d /var/lib/tempest /var/log/tempest
for f in bot lab; do
  if [[ ! -f /etc/tempest/$f.env ]]; then
    install -m 600 /opt/tempest/deploy/hetzner/$f.env.example /etc/tempest/$f.env
  fi
done

echo "== deploy timer (pulls master every 5 minutes)"
cat > /etc/systemd/system/tempest-deploy.service <<'UNIT'
[Unit]
Description=Deploy Tempest from origin/master
After=docker.service network-online.target
Wants=network-online.target
ConditionPathExists=/etc/tempest/bot.env

[Service]
Type=oneshot
ExecStart=/opt/tempest/deploy/hetzner/deploy.sh
UNIT
cat > /etc/systemd/system/tempest-deploy.timer <<'UNIT'
[Unit]
Description=Check for a new Tempest commit every 5 minutes

[Timer]
OnBootSec=2min
OnUnitActiveSec=5min

[Install]
WantedBy=timers.target
UNIT
systemctl daemon-reload

echo "== nightly lab, 01:17 UTC"
cat > /etc/cron.d/tempest-lab <<'CRON'
17 1 * * * root cd /opt/tempest && docker compose -f deploy/hetzner/docker-compose.yml run --rm lab >> /var/log/tempest/lab-$(date +\%F).log 2>&1
CRON
cat > /etc/logrotate.d/tempest <<'ROT'
/var/log/tempest/*.log {
  monthly
  maxage 90
  missingok
  notifempty
}
ROT

echo
code=$(curl -s -o /dev/null -w '%{http_code}' https://fapi.binance.com/fapi/v1/ping || true)
if [[ "$code" == "200" ]]; then
  echo "Binance futures API reachable from this server: OK"
else
  echo "WARNING: Binance futures API answered HTTP $code from this server."
  echo "         451 means a restricted location: pick another Hetzner location."
fi
echo
echo "Done. Next:"
echo "  1. nano /etc/tempest/bot.env   (paste the values from Railway's tempest-bot)"
echo "  2. Stop the bot on Railway, then:"
echo "     /opt/tempest/deploy/hetzner/deploy.sh --force && systemctl enable --now tempest-deploy.timer"
echo "  This server's public IP for the Binance whitelist: $(curl -fsS -4 https://ifconfig.me || echo unknown)"
