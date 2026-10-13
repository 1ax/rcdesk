#!/usr/bin/env bash
# Тесты infra/release-manifest.sh. Запуск:
#   bash infra/release-manifest.test.sh
# Один сценарий -- одна строка "ok N - описание" / "not ok N - описание".
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
MANIFEST="$SCRIPT_DIR/release-manifest.sh"

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

run_manifest() {
  rc=0
  out="$(bash "$MANIFEST" "$@" 2>&1)" || rc=$?
}

KEY="$WORK_DIR/key.pem"
openssl genpkey -algorithm ed25519 -out "$KEY"
openssl pkey -in "$KEY" -pubout -out "$WORK_DIR/pub.pem"

mkdir "$WORK_DIR/in" "$WORK_DIR/out"
printf 'setup stub\n' > "$WORK_DIR/in/rcdesk-setup.exe"
printf 'pkg stub\n' > "$WORK_DIR/in/rcdesk-macos.pkg"

if command -v sha256sum >/dev/null 2>&1; then
  h_exe="$(sha256sum "$WORK_DIR/in/rcdesk-setup.exe" | cut -d' ' -f1)"
  h_pkg="$(sha256sum "$WORK_DIR/in/rcdesk-macos.pkg" | cut -d' ' -f1)"
else
  h_exe="$(shasum -a 256 "$WORK_DIR/in/rcdesk-setup.exe" | cut -d' ' -f1)"
  h_pkg="$(shasum -a 256 "$WORK_DIR/in/rcdesk-macos.pkg" | cut -d' ' -f1)"
fi

# Файлы переданы не по алфавиту -- в манифесте порядок должен быть алфавитный.
run_manifest 1.2.3 "$KEY" "$WORK_DIR/out" "$WORK_DIR/in/rcdesk-setup.exe" "$WORK_DIR/in/rcdesk-macos.pkg"
if [[ $rc -eq 0 ]]; then report "успешный запуск" 1; else report "успешный запуск" 0; fi

expected="{\"version\":\"1.2.3\",\"assets\":{\"rcdesk-macos.pkg\":\"$h_pkg\",\"rcdesk-setup.exe\":\"$h_exe\"}}"
actual="$(cat "$WORK_DIR/out/rcdesk-update.json")"
if [[ "$actual" == "$expected" && "$(wc -l < "$WORK_DIR/out/rcdesk-update.json" | tr -d ' ')" -eq 1 ]]; then
  report "манифест точного вида" 1
else
  report "манифест точного вида" 0
fi

if [[ "$(wc -c < "$WORK_DIR/out/rcdesk-update.json.sig" | tr -d ' ')" -eq 64 ]]; then
  report "подпись 64 байта" 1
else
  report "подпись 64 байта" 0
fi

if openssl pkeyutl -verify -pubin -inkey "$WORK_DIR/pub.pem" -rawin \
  -in "$WORK_DIR/out/rcdesk-update.json" -sigfile "$WORK_DIR/out/rcdesk-update.json.sig" >/dev/null 2>&1; then
  report "подпись проверяется открытым ключом" 1
else
  report "подпись проверяется открытым ключом" 0
fi

cp "$WORK_DIR/out/rcdesk-update.json" "$WORK_DIR/tampered.json"
printf 'x' >> "$WORK_DIR/tampered.json"
if openssl pkeyutl -verify -pubin -inkey "$WORK_DIR/pub.pem" -rawin \
  -in "$WORK_DIR/tampered.json" -sigfile "$WORK_DIR/out/rcdesk-update.json.sig" >/dev/null 2>&1; then
  report "изменённый манифест не проходит" 0
else
  report "изменённый манифест не проходит" 1
fi

run_manifest 1.2.3 "$KEY" "$WORK_DIR/out"
if [[ $rc -eq 2 ]]; then report "мало аргументов" 1; else report "мало аргументов" 0; fi

run_manifest 1.2.3 "$WORK_DIR/нет-ключа.pem" "$WORK_DIR/out" "$WORK_DIR/in/rcdesk-setup.exe"
if [[ $rc -eq 2 && "$out" == *"не найден"* ]]; then report "нет ключа" 1; else report "нет ключа" 0; fi

run_manifest 1.2.3 "$KEY" "$WORK_DIR/out" "$WORK_DIR/in/нет-файла.exe"
if [[ $rc -eq 2 && "$out" == *"не найден"* ]]; then report "нет файла" 1; else report "нет файла" 0; fi

run_manifest 1.2.3 "$KEY" "$WORK_DIR/нет-каталога" "$WORK_DIR/in/rcdesk-setup.exe"
if [[ $rc -eq 2 && "$out" == *"не найден"* ]]; then report "нет каталога" 1; else report "нет каталога" 0; fi

exit "$fail"
