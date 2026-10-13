#!/usr/bin/env bash
# Печатает `version` из секции [workspace.package] корневого Cargo.toml.
# Использование: cargo-version.sh <Cargo.toml>
# Выход 0 -- версия напечатана; 2 -- неверные аргументы/нет файла/нет версии.
set -euo pipefail

if [[ $# -ne 1 ]]; then
  echo "usage: $0 <path/to/Cargo.toml>" >&2
  exit 2
fi
cargo_toml="$1"

if [[ ! -f "$cargo_toml" ]]; then
  echo "ошибка: файл $cargo_toml не найден" >&2
  exit 2
fi

version="$(awk '
  /^\[/ { in_pkg = ($0 == "[workspace.package]"); next }
  in_pkg && /^version[[:space:]]*=/ {
    gsub(/^version[[:space:]]*=[[:space:]]*"/, ""); gsub(/".*$/, ""); print; exit
  }
' "$cargo_toml")"

if [[ -z "$version" ]]; then
  echo "ошибка: в $cargo_toml нет version в [workspace.package]" >&2
  exit 2
fi
echo "$version"
