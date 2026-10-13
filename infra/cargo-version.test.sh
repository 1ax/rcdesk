#!/usr/bin/env bash
# Тесты infra/cargo-version.sh. Запуск:
#   bash infra/cargo-version.test.sh
# Один сценарий -- одна строка "ok N - описание" / "not ok N - описание".
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
SCRIPT="$SCRIPT_DIR/cargo-version.sh"

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

run_script() {
  rc=0
  out="$(bash "$SCRIPT" "$@" 2>&1)" || rc=$?
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

run_script "$WORK_DIR/Cargo.toml"
if [[ $rc -eq 0 && "$out" == "1.2.3" ]]; then report "версия из [workspace.package], не из зависимостей" 1; else report "версия из [workspace.package], не из зависимостей" 0; fi

cat > "$WORK_DIR/NoVersion.toml" <<'TOML'
[workspace.package]
edition = "2021"
TOML
run_script "$WORK_DIR/NoVersion.toml"
if [[ $rc -eq 2 && "$out" == *"нет version"* ]]; then report "нет версии" 1; else report "нет версии" 0; fi

run_script "$WORK_DIR/нет-файла.toml"
if [[ $rc -eq 2 && "$out" == *"не найден"* ]]; then report "нет файла" 1; else report "нет файла" 0; fi

run_script
if [[ $rc -eq 2 && "$out" == *"usage"* ]]; then report "без аргументов" 1; else report "без аргументов" 0; fi

exit "$fail"
