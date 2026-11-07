#!/usr/bin/env bash
# Uptime check, run every 5 minutes by cron (installed by apply.sh).
# Checks the sites in urls.txt and that the Tempest bot container is running,
# and tells Telegram only when something goes down or comes back, never on
# every run. Telegram credentials come from the bot's own /etc/tempest/bot.env.
set -uo pipefail
here=$(dirname "$(readlink -f "$0")")
state=/var/lib/tempest/monitor
mkdir -p "$state"

env_value() { grep -E "^$1=" /etc/tempest/bot.env 2>/dev/null | tail -1 | cut -d= -f2-; }
token=$(env_value TELEGRAM_BOT_TOKEN)
chat=$(env_value TELEGRAM_CHAT_ID)

notify() {
  [[ -n "$token" && -n "$chat" ]] || return 0
  curl -fsS -m 20 "https://api.telegram.org/bot${token}/sendMessage" \
    --data-urlencode "chat_id=${chat}" --data-urlencode "text=$1" >/dev/null || true
}

# Record a check's result; alert on the change only. A failure must repeat
# on two runs in a row (10 minutes) before it counts, so one blip is silent.
report() {
  local name=$1 ok=$2 detail=$3
  local down="$state/$name.down" strike="$state/$name.strike"
  if [[ "$ok" == 1 ]]; then
    rm -f "$strike"
    if [[ -f "$down" ]]; then
      rm -f "$down"
      notify "✅ ${name} is back up (${detail})"
    fi
  elif [[ ! -f "$down" ]]; then
    if [[ -f "$strike" ]]; then
      rm -f "$strike"
      touch "$down"
      notify "🔴 ${name} is down: ${detail}"
    else
      touch "$strike"
    fi
  fi
}

while read -r name url; do
  [[ -z "${name:-}" || "$name" == \#* ]] && continue
  code=$(curl -s -o /dev/null -m 20 -L -w '%{http_code}' "$url")
  if [[ "$code" =~ ^[23] ]]; then
    report "$name" 1 "HTTP $code"
  else
    report "$name" 0 "HTTP ${code:-no answer} from $url"
  fi
done < "$here/urls.txt"

running=$(docker inspect -f '{{.State.Running}}' tempest-bot-1 2>/dev/null || echo missing)
if [[ "$running" == true ]]; then
  report tempest-bot 1 "container running"
else
  report tempest-bot 0 "container not running ($running)"
fi
