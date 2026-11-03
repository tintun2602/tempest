#!/usr/bin/env bash
# Bring the bot up to origin/master. Run by the tempest-deploy timer every
# 5 minutes; safe to run by hand. `deploy.sh --force` rebuilds and restarts
# even when nothing changed.
set -euo pipefail
cd /opt/tempest
compose="docker compose -f deploy/hetzner/docker-compose.yml"
stamp=/var/lib/tempest/deployed-commit

git fetch --quiet origin master
target=$(git rev-parse origin/master)
deployed=$(cat "$stamp" 2>/dev/null || true)
if [[ "$target" == "$deployed" && "${1:-}" != "--force" ]]; then
  exit 0
fi

echo "$(date -u +%FT%TZ) deploying ${target:0:7} (was ${deployed:0:7})"
git reset --quiet --hard "$target"
# Build first: a failed build leaves the running bot as it was, and the next
# run tries again because the stamp still names the old commit.
$compose build bot
$compose up -d bot
echo "$target" > "$stamp"
docker image prune -f >/dev/null
