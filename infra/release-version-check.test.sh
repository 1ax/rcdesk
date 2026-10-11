#!/usr/bin/env bash
# Тесты infra/release-version-check.sh. Запуск:
#   bash infra/release-version-check.test.sh
# Один сценарий -- одна строка "ok N - описание" / "not ok N - описание".
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
CHECK="$SCRIPT_DIR/release-version-check.sh"

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

# Запускает проверку, возвращает код выхода в $rc (вывод в $out).
run_check() {
  rc=0
  out="$(bash "$CHECK" "$@" 2>&1)" || rc=$?
}

cat > "$WORK_DIR/Cargo.toml" <<'TOML'
[workspace]
members = ["a"]

[workspace.package]
version = "1.2.3"
edition = "2021"

[workspace.dependencies]
version = "9.9.9"
TOML

run_check v1.2.3 "$WORK_DIR/Cargo.toml"
if [[ $rc -eq 0 ]]; then report "тег совпадает с версией" 1; else report "тег совпадает с версией" 0; fi

run_check v1.2.4 "$WORK_DIR/Cargo.toml"
if [[ $rc -eq 1 && "$out" == *"не совпадает"* ]]; then report "тег не совпадает" 1; else report "тег не совпадает" 0; fi

run_check 1.2.3 "$WORK_DIR/Cargo.toml"
if [[ $rc -eq 1 && "$out" == *"начинаться с 'v'"* ]]; then report "тег без v" 1; else report "тег без v" 0; fi

run_check v1.2.3 "$WORK_DIR/нет-файла.toml"
if [[ $rc -eq 2 && "$out" == *"не найден"* ]]; then report "нет файла" 1; else report "нет файла" 0; fi

exit "$fail"
