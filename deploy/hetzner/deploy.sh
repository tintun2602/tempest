#!/usr/bin/env bash
# Bring the bot up to origin/master. Run by the tempest-deploy timer every
# 5 minutes; safe to run by hand. `deploy.sh --force` rebuilds and restarts
# even when nothing changed.
set -euo pipefail
cd /opt/tempest
compose="docker compose -f deploy/hetzner/docker-compose.yml"
stamp=/var/lib/tempest/deployed-commit

# One deploy at a time: two at once (the timer and a run by hand) race to
# recreate the bot container and one fails on the name. The lock stays held
# across the re-exec below, since fd 9 is inherited. The timer skips a run
# that finds it taken; a run by hand waits up to 10 minutes for it.
if [[ -z "${TEMPEST_DEPLOY_LOCKED:-}" ]]; then
  exec 9>/var/lib/tempest/deploy.lock
  if [[ "${1:-}" == "--force" ]]; then
    flock -w 600 9 || { echo "another deploy is still running" >&2; exit 1; }
  else
    flock -n 9 || exit 0
  fi
  export TEMPEST_DEPLOY_LOCKED=1
fi

git fetch --quiet origin master
target=$(git rev-parse origin/master)
deployed=$(cat "$stamp" 2>/dev/null || true)
if [[ "$target" == "$deployed" && "${1:-}" != "--force" ]]; then
  exit 0
fi

if [[ -z "${TEMPEST_DEPLOY_CHECKED_OUT:-}" ]]; then
  echo "$(date -u +%FT%TZ) deploying ${target:0:7} (was ${deployed:0:7})"
  git reset --quiet --hard "$target"
  # Carry on as the commit being deployed defines this script, so a change
  # to the deploy steps takes effect in the same deploy.
  TEMPEST_DEPLOY_CHECKED_OUT=1 exec bash deploy/hetzner/deploy.sh "$@"
fi
# Build first: a failed build leaves the running bot as it was, and the next
# run tries again because the stamp still names the old commit.
$compose build bot
# The meme paper test gets only the Telegram settings, never the exchange keys.
grep -E '^TELEGRAM_(BOT_TOKEN|CHAT_ID)=' /etc/tempest/bot.env > /etc/tempest/meme.env || true
chmod 600 /etc/tempest/meme.env
$compose up -d bot meme
if [[ -f /etc/tempest/freqai-secrets.json ]]; then
  # Restart FreqAI only when its own files changed: a restart costs it the
  # current candle's predictions, and Tempest commits do not concern it.
  if [[ -z "$deployed" || "${1:-}" == "--force" ]] \
     || ! git diff --quiet "$deployed" "$target" -- deploy/hetzner/freqai deploy/hetzner/docker-compose.yml; then
    $compose --profile freqai pull freqai
    $compose --profile freqai up -d --force-recreate freqai
  else
    $compose --profile freqai up -d freqai
  fi
fi
bash deploy/hetzner/apply.sh
echo "$target" > "$stamp"
docker image prune -f >/dev/null
