#!/usr/bin/env bash
# Push a status report to the private repo tintun2602/diablo: bot state, the
# deployed commit, trades and errors from the bot's log, the non-secret
# settings, the latest nightly lab report. Run hourly by cron and after every deploy (both from apply.sh).
# Does nothing until the deploy key /root/.ssh/diablo_deploy exists.
set -uo pipefail
key=/root/.ssh/diablo_deploy
[[ -f "$key" ]] || exit 0
work=/var/lib/tempest/diablo
export GIT_SSH_COMMAND="ssh -i $key -o IdentitiesOnly=yes -o StrictHostKeyChecking=accept-new"
git_id=(-c user.name="tempest-server" -c user.email="tintunn26@gmail.com")

if [[ ! -d "$work/.git" ]]; then
  git clone --quiet git@github.com:tintun2602/diablo.git "$work" || exit 1
fi
cd "$work"
git fetch --quiet origin main && git reset --quiet --hard origin/main

# Belt and braces: the bot never logs credentials, but anything shaped like a
# key, secret or Telegram token is blanked before it is written.
scrub() {
  sed -E -e 's/\x1b\[[0-9;]*m//g' \
         -e 's/[0-9]{6,}:[A-Za-z0-9_-]{30,}/[redacted]/g' \
         -e 's/[A-Za-z0-9]{40,}/[redacted]/g'
}
bot=tempest-bot-1
block() { echo '```'; cat; echo '```'; }

{
  echo "# Tempest status"
  echo
  echo "Reported $(date -u '+%Y-%m-%d %H:%M UTC') by the server."
  echo
  echo "## Server"
  {
    echo "deployed commit: $(cut -c1-7 /var/lib/tempest/deployed-commit 2>/dev/null || echo unknown)"
    docker inspect -f 'bot container: running={{.State.Running}} since {{.State.StartedAt}}, restarts {{.RestartCount}}' "$bot" 2>&1
    uptime
    free -m | head -2
    df -h / | tail -1
  } | block
  echo
  echo "## Settings (no keys, secrets or tokens)"
  # As the bot sees them: the git settings.env wins over bot.env.
  docker exec "$bot" env 2>/dev/null | grep -v -E 'KEY|SECRET|TOKEN|^PATH=|^HOSTNAME=|^HOME=' | sort | scrub | block
  echo
  echo "## Trades, exits, starts and problems, last 24 h"
  docker logs --since 24h "$bot" 2>&1 | scrub \
    | grep -E 'LONG|SHORT|closed|ERROR|WARN|started|Equity|New UTC|no stop|rate limit|banned' \
    | tail -80 | block
  echo
  echo "## Nightly lab (latest LAB_REPORT, run 01:17 UTC)"
  lab=$(ls -1t /var/log/tempest/lab-*.log 2>/dev/null | head -1)
  if [[ -n "$lab" ]]; then
    { echo "log: $(basename "$lab")"; grep -h 'LAB_REPORT' "$lab" | tail -1; } | scrub | block
  else
    echo "No lab log yet."
  fi
  echo
  echo "## Last 30 log lines"
  docker logs --tail 30 "$bot" 2>&1 | scrub | block
} > status.md

git add status.md
git "${git_id[@]}" commit --quiet -m "status $(date -u '+%Y-%m-%d %H:%M UTC')" || exit 0
git push --quiet origin HEAD:main
