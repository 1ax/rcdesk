#!/usr/bin/env bash
# Тесты infra/traffic-monitor.sh: без внешних зависимостей (мок метрик через
# file://, свой tx_bytes, свой notify-файл, маленькие бюджеты). Запуск:
#   bash infra/traffic-monitor.test.sh
# Один сценарий -- одна строка "ok N - описание" / "not ok N - описание".
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
MONITOR="$SCRIPT_DIR/traffic-monitor.sh"

WORK_DIR="$(mktemp -d)"
trap 'rm -rf "$WORK_DIR"' EXIT

test_n=0
fail=0

report() {
  local desc="$1" passed="$2"
  test_n=$(( test_n + 1 ))
  if [[ "$passed" -eq 1 ]]; then
    echo "ok $test_n - $desc"
  else
    echo "not ok $test_n - $desc"
    fail=1
  fi
}

state_value() {
  local key="$1" file="$2"
  grep -E "^${key}=" "$file" | tail -n1 | cut -d= -f2-
}

# Общее окружение для запуска скрипта под тестом: свой каталог, свои файлы
# метрик/tx/notify, маленькие бюджеты (переопределяются по сценарию).
run_monitor() {
  env \
    TRAFFIC_DIR="$CASE_DIR/state" \
    TRAFFIC_ENV_FILE="$CASE_DIR/env" \
    TRAFFIC_METRICS_URL="${METRICS_URL:-file://$CASE_DIR/metrics}" \
    TRAFFIC_TX_FILE="$CASE_DIR/tx_bytes" \
    TRAFFIC_RELAY_BUDGET="${RELAY_BUDGET:-1000000}" \
    TRAFFIC_IFACE_BUDGET="${IFACE_BUDGET:-1000000}" \
    TRAFFIC_MONTH="${MONTH:-2026-01}" \
    TRAFFIC_NOTIFY_FILE="$NOTIFY_FILE" \
    TRAFFIC_TG_BOT_TOKEN="${TG_TOKEN:-}" \
    TRAFFIC_TG_CHAT_ID="${TG_CHAT:-}" \
    "$MONITOR"
}

new_case_dir() {
  CASE_DIR="$(mktemp -d "$WORK_DIR/case.XXXXXX")"
  mkdir -p "$CASE_DIR/state"
  : > "$CASE_DIR/env"
  RELAY_BUDGET=1000000
  IFACE_BUDGET=1000000
  MONTH=2026-01
  NOTIFY_FILE="$CASE_DIR/notify.log"
  METRICS_URL=""
  TG_TOKEN=""
  TG_CHAT=""
}

write_metrics() {
  printf '%s\n' "$1" > "$CASE_DIR/metrics"
}

write_tx() {
  printf '%s\n' "$1" > "$CASE_DIR/tx_bytes"
}

state_file() {
  printf '%s/state/%s.state' "$CASE_DIR" "$MONTH"
}

notify_lines() {
  if [[ -f "$NOTIFY_FILE" ]]; then
    wc -l < "$NOTIFY_FILE" | tr -d ' '
  else
    echo 0
  fi
}

# 1. Первый запуск: relay_total=0, iface_total=0, уведомлений нет.
test_first_run() {
  new_case_dir
  write_metrics 'turn_total_traffic_sentb{} 0'
  write_tx '0'
  run_monitor
  local sf pass=1
  sf="$(state_file)"
  [[ "$(state_value relay_total "$sf")" == "0" ]] || pass=0
  [[ "$(state_value iface_total "$sf")" == "0" ]] || pass=0
  [[ "$(state_value notified "$sf")" == "" ]] || pass=0
  report "первый запуск: totals=0, уведомлений нет" "$pass"
}

# 2. Рост релея на 1000 и iface на 5000: totals 1000 и 5000.
test_growth() {
  new_case_dir
  write_metrics 'turn_total_traffic_sentb{} 0'
  write_tx '0'
  run_monitor
  write_metrics 'turn_total_traffic_sentb{} 1000'
  write_tx '5000'
  run_monitor
  local sf pass=1
  sf="$(state_file)"
  [[ "$(state_value relay_total "$sf")" == "1000" ]] || pass=0
  [[ "$(state_value iface_total "$sf")" == "5000" ]] || pass=0
  report "рост релея на 1000 и iface на 5000" "$pass"
}

# 3. Сброс счётчика релея (рестарт coturn): current < last -> delta=current.
test_counter_reset() {
  new_case_dir
  write_metrics 'turn_total_traffic_sentb{} 0'
  write_tx '0'
  run_monitor
  write_metrics 'turn_total_traffic_sentb{} 1000'
  run_monitor
  write_metrics 'turn_total_traffic_sentb{} 300'
  run_monitor
  local sf pass=1
  sf="$(state_file)"
  [[ "$(state_value relay_total "$sf")" == "1300" ]] || pass=0
  report "сброс счётчика релея: relay_total=1300" "$pass"
}

# 4. Labels + экспонента суммируются (2000), rcvb игнорируется.
test_labels_and_exponent() {
  new_case_dir
  write_metrics 'turn_total_traffic_sentb{} 0'
  write_tx '0'
  run_monitor
  write_metrics '# HELP turn_total_traffic_sentb x
turn_total_traffic_sentb{realm="rcdesk.app"} 1.5e+03
turn_total_traffic_peer_sentb 500
turn_total_traffic_rcvb{realm="rcdesk.app"} 999'
  run_monitor
  local sf pass=1
  sf="$(state_file)"
  [[ "$(state_value relay_total "$sf")" == "2000" ]] || pass=0
  report "labels+экспонента суммируются, rcvb игнорируется" "$pass"
}

# 5. Порог relay 50/80/100 при бюджете 2000.
test_relay_thresholds() {
  new_case_dir
  RELAY_BUDGET=2000
  IFACE_BUDGET=1000000000
  write_metrics 'turn_total_traffic_sentb{} 0'
  write_tx '0'
  run_monitor

  local pass=1

  write_metrics 'turn_total_traffic_sentb{} 1000'
  run_monitor
  [[ "$(notify_lines)" == "1" ]] || pass=0
  grep -q '(50 %)' "$NOTIFY_FILE" || pass=0

  run_monitor
  [[ "$(notify_lines)" == "1" ]] || pass=0

  write_metrics 'turn_total_traffic_sentb{} 2000'
  run_monitor
  [[ "$(notify_lines)" == "3" ]] || pass=0

  report "пороги relay 50/80/100%: 1, затем без роста 1, затем 3" "$pass"
}

# 6. Порог iface 90% -- одно уведомление.
test_iface_threshold() {
  new_case_dir
  RELAY_BUDGET=1000000000
  IFACE_BUDGET=1000
  write_metrics 'turn_total_traffic_sentb{} 0'
  write_tx '0'
  run_monitor
  write_tx '900'
  run_monitor
  local pass=1
  [[ "$(notify_lines)" == "1" ]] || pass=0
  grep -q 'исходящий VPS' "$NOTIFY_FILE" || pass=0
  grep -q '(90 %)' "$NOTIFY_FILE" || pass=0
  report "порог iface 90%: одно уведомление" "$pass"
}

# 7. Метрики недоступны: скрипт завершается 0, iface_total продолжает расти.
test_metrics_unavailable() {
  new_case_dir
  METRICS_URL="file://$CASE_DIR/does-not-exist"
  write_tx '0'
  run_monitor
  write_tx '4000'
  local pass=1
  run_monitor 2>/dev/null || pass=0
  local sf
  sf="$(state_file)"
  [[ "$(state_value iface_total "$sf")" == "4000" ]] || pass=0
  [[ "$(state_value relay_total "$sf")" == "0" ]] || pass=0
  report "метрики недоступны: exit 0, iface_total растёт" "$pass"
}

# 8. Другой TRAFFIC_MONTH -- отдельный state-файл с нуля.
test_month_isolation() {
  new_case_dir
  write_metrics 'turn_total_traffic_sentb{} 1000'
  write_tx '1000'
  run_monitor
  MONTH=2026-02
  run_monitor
  local sf pass=1
  sf="$(state_file)"
  [[ "$(state_value relay_total "$sf")" == "0" ]] || pass=0
  report "другой TRAFFIC_MONTH: новый state-файл с нуля" "$pass"
}

# 9. Без TRAFFIC_NOTIFY_FILE и без токена: метка не появляется, exit 0.
test_notify_skipped_without_token() {
  new_case_dir
  RELAY_BUDGET=100
  NOTIFY_FILE=""
  write_metrics 'turn_total_traffic_sentb{} 1000'
  write_tx '0'
  local pass=1
  run_monitor 2>/dev/null || pass=0
  local sf
  sf="$(state_file)"
  [[ "$(state_value notified "$sf")" == "" ]] || pass=0
  report "без notify-файла и токена: без метки, exit 0" "$pass"
}

test_first_run
test_growth
test_counter_reset
test_labels_and_exponent
test_relay_thresholds
test_iface_threshold
test_metrics_unavailable
test_month_isolation
test_notify_skipped_without_token

if [[ "$fail" -eq 0 ]]; then
  echo "ok $test_n"
  exit 0
else
  echo "FAIL"
  exit 1
fi
