#!/usr/bin/env bash
# Монитор исходящего трафика VPS rcdesk: TURN-релей (coturn Prometheus-метрики
# на 127.0.0.1:9641) + весь исходящий трафик интерфейса (tx_bytes). Копит
# помесячный итог в $TRAFFIC_DIR/<YYYY-MM>.state и шлёт уведомление в Telegram
# при пересечении порогов бюджета (ARCHITECTURE.md §11). Запускается по cron
# пользователя rcdesk раз в 10 минут (infra/README.md, раздел «Монитор трафика»).
#
# Зависимости: bash, curl, awk, flock, date, mkdir -- без jq (на сервере его нет).
set -euo pipefail

TRAFFIC_DIR="${TRAFFIC_DIR:-$HOME/app/traffic}"
TRAFFIC_ENV_FILE="${TRAFFIC_ENV_FILE:-$HOME/app/.env}"
TRAFFIC_METRICS_URL="${TRAFFIC_METRICS_URL:-http://127.0.0.1:9641/metrics}"
TRAFFIC_TX_FILE="${TRAFFIC_TX_FILE:-/sys/class/net/ens3/statistics/tx_bytes}"
TRAFFIC_RELAY_BUDGET="${TRAFFIC_RELAY_BUDGET:-5000000000000}"
TRAFFIC_IFACE_BUDGET="${TRAFFIC_IFACE_BUDGET:-10000000000000}"
TRAFFIC_MONTH="${TRAFFIC_MONTH:-$(date -u +%Y-%m)}"
TRAFFIC_NOTIFY_FILE="${TRAFFIC_NOTIFY_FILE:-}"
TRAFFIC_TG_BOT_TOKEN="${TRAFFIC_TG_BOT_TOKEN:-}"
TRAFFIC_TG_CHAT_ID="${TRAFFIC_TG_CHAT_ID:-}"

# Читает значение KEY=VALUE из файла через grep/cut (не source -- файл это .env
# с секретами, читаем только конкретный ключ, не выполняем его как shell).
read_kv() {
  local key="$1" file="$2" val=""
  if [[ -f "$file" ]]; then
    val="$(grep -E "^${key}=" "$file" | tail -n1 | cut -d= -f2- || true)"
  fi
  printf '%s' "$val"
}

sanitize_int() {
  local val="$1" default="$2"
  if [[ "$val" =~ ^[0-9]+$ ]]; then
    printf '%s' "$val"
  else
    printf '%s' "$default"
  fi
}

has_label() {
  local label="$1" list="$2"
  case " $list " in
    *" $label "*) return 0 ;;
    *) return 1 ;;
  esac
}

add_label() {
  local label="$1"
  if [[ -z "$notified" ]]; then
    notified="$label"
  else
    notified="$notified $label"
  fi
}

fmt_tb() {
  awk -v b="$1" 'BEGIN { printf "%.2f", b / 1000000000000 }'
}

relay_msg() {
  local pct="$1" relay_tb budget_relay_tb iface_tb budget_iface_tb
  relay_tb="$(fmt_tb "$relay_total")"
  budget_relay_tb="$(fmt_tb "$TRAFFIC_RELAY_BUDGET")"
  iface_tb="$(fmt_tb "$iface_total")"
  budget_iface_tb="$(fmt_tb "$TRAFFIC_IFACE_BUDGET")"
  printf 'rcdesk трафик %s: TURN-релей %s ТБ из %s ТБ (%s %%). Исходящий VPS всего %s ТБ из %s ТБ.' \
    "$TRAFFIC_MONTH" "$relay_tb" "$budget_relay_tb" "$pct" "$iface_tb" "$budget_iface_tb"
}

iface_msg() {
  local pct="$1" relay_tb budget_relay_tb iface_tb budget_iface_tb
  iface_tb="$(fmt_tb "$iface_total")"
  budget_iface_tb="$(fmt_tb "$TRAFFIC_IFACE_BUDGET")"
  relay_tb="$(fmt_tb "$relay_total")"
  budget_relay_tb="$(fmt_tb "$TRAFFIC_RELAY_BUDGET")"
  printf 'rcdesk трафик %s: исходящий VPS %s ТБ из %s ТБ (%s %%). TURN-релей %s ТБ из %s ТБ.' \
    "$TRAFFIC_MONTH" "$iface_tb" "$budget_iface_tb" "$pct" "$relay_tb" "$budget_relay_tb"
}

send_notify() {
  local text="$1"
  if [[ -n "$TRAFFIC_NOTIFY_FILE" ]]; then
    printf '%s\n' "$text" >> "$TRAFFIC_NOTIFY_FILE"
    return 0
  fi
  if [[ -z "$TRAFFIC_TG_BOT_TOKEN" || -z "$TRAFFIC_TG_CHAT_ID" ]]; then
    echo "traffic-monitor: notify skipped: TRAFFIC_TG_BOT_TOKEN/TRAFFIC_TG_CHAT_ID not set" >&2
    return 1
  fi
  if curl -fsS -m 20 -X POST "https://api.telegram.org/bot${TRAFFIC_TG_BOT_TOKEN}/sendMessage" \
      --data-urlencode "chat_id=${TRAFFIC_TG_CHAT_ID}" \
      --data-urlencode "text=${text}" >/dev/null; then
    echo "traffic-monitor: notified: $text" >&2
    return 0
  fi
  echo "traffic-monitor: warning: telegram send failed" >&2
  return 1
}

mkdir -p "$TRAFFIC_DIR"

# Одновременный запуск (cron раз в 10 минут + ручной прогон) пропускаем молча.
exec 9>"$TRAFFIC_DIR/.lock"
if ! flock -n 9; then
  echo "traffic-monitor: lock busy, skip" >&2
  exit 0
fi

if [[ -z "$TRAFFIC_TG_BOT_TOKEN" ]]; then
  TRAFFIC_TG_BOT_TOKEN="$(read_kv TRAFFIC_TG_BOT_TOKEN "$TRAFFIC_ENV_FILE")"
fi
if [[ -z "$TRAFFIC_TG_CHAT_ID" ]]; then
  TRAFFIC_TG_CHAT_ID="$(read_kv TRAFFIC_TG_CHAT_ID "$TRAFFIC_ENV_FILE")"
fi

# --- Снимок метрик релея coturn ---
relay_fetch_ok=1
relay_reading=0
if metrics="$(curl -fsS -m 10 "$TRAFFIC_METRICS_URL" 2>/dev/null)"; then
  relay_reading="$(printf '%s\n' "$metrics" | awk '
    $1 ~ /^turn_total_traffic(_peer)?_sentb/ { sum += $2 }
    END { printf "%.0f", sum + 0 }
  ')"
  relay_reading="$(sanitize_int "$relay_reading" 0)"
else
  echo "traffic-monitor: warning: metrics fetch failed ($TRAFFIC_METRICS_URL)" >&2
  relay_fetch_ok=0
fi

# --- Снимок tx_bytes интерфейса ---
iface_fetch_ok=1
iface_reading=0
if [[ -r "$TRAFFIC_TX_FILE" ]]; then
  iface_reading="$(sanitize_int "$(cat "$TRAFFIC_TX_FILE")" 0)"
else
  echo "traffic-monitor: warning: tx file not found ($TRAFFIC_TX_FILE)" >&2
  iface_fetch_ok=0
fi

# --- Состояние месяца ---
STATE_FILE="$TRAFFIC_DIR/$TRAFFIC_MONTH.state"

relay_last="$relay_reading"
relay_total="0"
iface_last="$iface_reading"
iface_total="0"
notified=""
if [[ -f "$STATE_FILE" ]]; then
  relay_last="$(read_kv relay_last "$STATE_FILE")"
  relay_total="$(read_kv relay_total "$STATE_FILE")"
  iface_last="$(read_kv iface_last "$STATE_FILE")"
  iface_total="$(read_kv iface_total "$STATE_FILE")"
  notified="$(read_kv notified "$STATE_FILE")"
fi
relay_last="$(sanitize_int "$relay_last" "$relay_reading")"
relay_total="$(sanitize_int "$relay_total" 0)"
iface_last="$(sanitize_int "$iface_last" "$iface_reading")"
iface_total="$(sanitize_int "$iface_total" 0)"

# --- Дельты и накопление ---
if [[ "$relay_fetch_ok" -eq 1 ]]; then
  if (( relay_reading >= relay_last )); then
    relay_delta=$(( relay_reading - relay_last ))
  else
    relay_delta=$relay_reading
  fi
  relay_total=$(( relay_total + relay_delta ))
  relay_last=$relay_reading
fi

if [[ "$iface_fetch_ok" -eq 1 ]]; then
  if (( iface_reading >= iface_last )); then
    iface_delta=$(( iface_reading - iface_last ))
  else
    iface_delta=$iface_reading
  fi
  iface_total=$(( iface_total + iface_delta ))
  iface_last=$iface_reading
fi

# --- Пороги и уведомления ---
for pct in 50 80 100; do
  label="relay${pct}"
  if (( relay_total * 100 >= TRAFFIC_RELAY_BUDGET * pct )) && ! has_label "$label" "$notified"; then
    if send_notify "$(relay_msg "$pct")"; then
      add_label "$label"
    fi
  fi
done

pct=90
label="iface${pct}"
if (( iface_total * 100 >= TRAFFIC_IFACE_BUDGET * pct )) && ! has_label "$label" "$notified"; then
  if send_notify "$(iface_msg "$pct")"; then
    add_label "$label"
  fi
fi

# --- Запись состояния (временный файл + mv -- атомарно) ---
updated="$(date -u +%Y-%m-%dT%H:%M:%SZ)"
tmp_file="$(mktemp "$TRAFFIC_DIR/.${TRAFFIC_MONTH}.state.XXXXXX")"
{
  printf 'relay_last=%s\n' "$relay_last"
  printf 'relay_total=%s\n' "$relay_total"
  printf 'iface_last=%s\n' "$iface_last"
  printf 'iface_total=%s\n' "$iface_total"
  printf 'notified=%s\n' "$notified"
  printf 'updated=%s\n' "$updated"
} > "$tmp_file"
mv -f "$tmp_file" "$STATE_FILE"
