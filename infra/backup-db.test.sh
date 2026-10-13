#!/usr/bin/env bash
# Тесты infra/backup-db.sh: заглушки docker/gpg/rclone/curl в $CASE_DIR/bin
# (подставляются в начало PATH), свой HOME, свой notify-файл. Запуск:
#   bash infra/backup-db.test.sh
# Один сценарий -- одна строка "ok N - описание" / "not ok N - описание".
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
BACKUP="$SCRIPT_DIR/backup-db.sh"

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

write_stubs() {
  local bin="$CASE_DIR/bin"
  mkdir -p "$bin"

  cat > "$bin/docker" <<'STUB'
#!/usr/bin/env bash
echo "docker $*" >> "$CASE_DIR/calls.log"
if [[ "${STUB_DOCKER_FAIL:-}" == "1" ]]; then
  exit 1
fi
host=""
prev=""
for a in "$@"; do
  if [[ "$prev" == "-v" && "$a" == *:/out ]]; then
    host="${a%:/out}"
  fi
  case "$a" in
    .backup*) printf 'DB' > "$host/rcdesk.db" ;;
    "PRAGMA integrity_check") echo "${STUB_INTEGRITY:-ok}" ;;
  esac
  prev="$a"
done
exit 0
STUB

  cat > "$bin/gpg" <<'STUB'
#!/usr/bin/env bash
echo "gpg $*" >> "$CASE_DIR/calls.log"
out=""
src=""
while [[ $# -gt 0 ]]; do
  case "$1" in
    --output) out="$2"; shift 2 ;;
    --recipient) shift 2 ;;
    --*) shift ;;
    *) src="$1"; shift ;;
  esac
done
{ printf 'ENC:'; cat "$src"; } > "$out"
STUB

  cat > "$bin/rclone" <<'STUB'
#!/usr/bin/env bash
echo "rclone $*" >> "$CASE_DIR/calls.log"
if [[ "$1" == "copy" ]]; then
  mkdir -p "$CASE_DIR/remote"
  cp "$2" "$CASE_DIR/remote/"
fi
exit 0
STUB

  cat > "$bin/curl" <<'STUB'
#!/usr/bin/env bash
echo "curl $*" >> "$CASE_DIR/calls.log"
exit 0
STUB

  chmod +x "$bin"/*
}

new_case_dir() {
  CASE_DIR="$(mktemp -d "$WORK_DIR/case.XXXXXX")"
  export CASE_DIR
  mkdir -p "$CASE_DIR/home"
  : > "$CASE_DIR/env"
  : > "$CASE_DIR/calls.log"
  write_stubs
  BACKUP_DIR="$CASE_DIR/backups"
  NOTIFY_FILE="$CASE_DIR/notify.log"
  HEARTBEAT="https://hb.example/ping"
  RECIPIENT="TESTKEY"
}

run_backup() {
  env \
    HOME="$CASE_DIR/home" \
    PATH="$CASE_DIR/bin:$PATH" \
    CASE_DIR="$CASE_DIR" \
    BACKUP_DIR="$BACKUP_DIR" \
    BACKUP_ENV_FILE="$CASE_DIR/env" \
    BACKUP_NOTIFY_FILE="$NOTIFY_FILE" \
    BACKUP_GPG_RECIPIENT="$RECIPIENT" \
    BACKUP_HEARTBEAT_URL="$HEARTBEAT" \
    "$@" \
    "$BACKUP"
}

count_gpg() {
  find "$BACKUP_DIR" -maxdepth 1 -name 'rcdesk-*.db.gpg' -type f | wc -l | tr -d ' '
}

count_glob() {
  find "$BACKUP_DIR" -maxdepth 1 -name "$1" | wc -l | tr -d ' '
}

# 1. Успех.
test_success() {
  new_case_dir
  local pass=1 rc=0 f
  run_backup >/dev/null 2>&1 || rc=$?
  [[ "$rc" -eq 0 ]] || pass=0
  [[ "$(count_gpg)" == "1" ]] || pass=0
  f="$(find "$BACKUP_DIR" -maxdepth 1 -name 'rcdesk-*.db.gpg' -type f | head -n1)"
  [[ -n "$f" && "$(head -c 4 "$f")" == "ENC:" ]] || pass=0
  [[ "$(count_glob '*.part')" == "0" ]] || pass=0
  [[ "$(count_glob '.work.*')" == "0" ]] || pass=0
  grep -q '^rclone copy ' "$CASE_DIR/calls.log" || pass=0
  grep -q '^rclone delete --min-age 90d ' "$CASE_DIR/calls.log" || pass=0
  grep -q '^curl .*https://hb.example/ping' "$CASE_DIR/calls.log" || pass=0
  [[ ! -e "$NOTIFY_FILE" ]] || pass=0
  report "успех: один .gpg с ENC:, без .part/.work, rclone, heartbeat, без уведомлений" "$pass"
}

# 2. Локальная ротация.
test_rotation() {
  new_case_dir
  mkdir -p "$BACKUP_DIR"
  local old="$BACKUP_DIR/rcdesk-20200101-000000.db.gpg" pass=1
  : > "$old"
  touch -t 202001010000 "$old"
  run_backup >/dev/null 2>&1 || pass=0
  [[ ! -e "$old" ]] || pass=0
  [[ "$(count_gpg)" == "1" ]] || pass=0
  report "ротация: старый файл удалён, свежий остался" "$pass"
}

# 3. Нет получателя GPG.
test_no_recipient() {
  new_case_dir
  RECIPIENT=""
  local pass=1 rc=0
  run_backup >/dev/null 2>&1 || rc=$?
  [[ "$rc" -eq 1 ]] || pass=0
  grep -q 'УПАЛ' "$NOTIFY_FILE" || pass=0
  [[ "$(count_gpg)" == "0" ]] || pass=0
  report "без BACKUP_GPG_RECIPIENT: exit 1, уведомление УПАЛ, .gpg нет" "$pass"
}

# 4. Падение docker.
test_docker_fail() {
  new_case_dir
  local pass=1 rc=0
  run_backup STUB_DOCKER_FAIL=1 >/dev/null 2>&1 || rc=$?
  [[ "$rc" -eq 1 ]] || pass=0
  grep -q 'УПАЛ' "$NOTIFY_FILE" || pass=0
  [[ "$(count_gpg)" == "0" ]] || pass=0
  [[ "$(count_glob '.work.*')" == "0" ]] || pass=0
  report "падение docker: exit 1, УПАЛ, .gpg и .work нет" "$pass"
}

# 5. integrity_check не ok.
test_integrity_fail() {
  new_case_dir
  local pass=1 rc=0
  run_backup STUB_INTEGRITY=corrupt >/dev/null 2>&1 || rc=$?
  [[ "$rc" -eq 1 ]] || pass=0
  [[ "$(count_gpg)" == "0" ]] || pass=0
  report "integrity_check=corrupt: exit 1, .gpg нет" "$pass"
}

# 6. Без heartbeat.
test_no_heartbeat() {
  new_case_dir
  HEARTBEAT=""
  local pass=1 rc=0
  run_backup >/dev/null 2>&1 || rc=$?
  [[ "$rc" -eq 0 ]] || pass=0
  ! grep -q '^curl ' "$CASE_DIR/calls.log" || pass=0
  report "без BACKUP_HEARTBEAT_URL: curl не вызывался, exit 0" "$pass"
}

# 7. Lock занят.
test_lock_busy() {
  new_case_dir
  mkdir -p "$BACKUP_DIR"
  (
    exec 9>"$BACKUP_DIR/.lock"
    flock 9
    sleep 5
  ) &
  local holder=$! pass=1 rc=0 out
  sleep 1
  out="$(run_backup 2>&1)" || rc=$?
  kill "$holder" 2>/dev/null || true
  wait "$holder" 2>/dev/null || true
  [[ "$rc" -eq 0 ]] || pass=0
  [[ "$out" == *"lock busy"* ]] || pass=0
  [[ "$(count_gpg)" == "0" ]] || pass=0
  report "lock занят: exit 0, lock busy" "$pass"
}

# 8. Образ по умолчанию берёт IMAGE_TAG из env-файла.
test_default_image() {
  new_case_dir
  printf 'IMAGE_TAG=sha-abc1234\n' > "$CASE_DIR/env"
  local pass=1
  run_backup >/dev/null 2>&1 || pass=0
  grep -q 'ghcr.io/1ax/rcdesk-server:sha-abc1234' "$CASE_DIR/calls.log" || pass=0
  report "BACKUP_IMAGE по умолчанию: IMAGE_TAG из env-файла" "$pass"
}

test_success
test_rotation
test_no_recipient
test_docker_fail
test_integrity_fail
test_no_heartbeat
test_lock_busy
test_default_image

if [[ "$fail" -eq 0 ]]; then
  echo "ok $test_n"
  exit 0
else
  echo "FAIL"
  exit 1
fi
