#!/usr/bin/env bash
# Проверка релиза: тег vX.Y.Z должен совпадать с `version` из [workspace.package]
# корневого Cargo.toml. Использование: release-version-check.sh <tag> <Cargo.toml>
# Выход 0 -- совпало; 1 -- не совпало/тег без `v`; 2 -- неверные аргументы/нет файла.
set -euo pipefail

if [[ $# -ne 2 ]]; then
  echo "usage: $0 <tag> <path/to/Cargo.toml>" >&2
  exit 2
fi
tag="$1"
cargo_toml="$2"

if [[ ! -f "$cargo_toml" ]]; then
  echo "ошибка: файл $cargo_toml не найден" >&2
  exit 2
fi
if [[ "$tag" != v* ]]; then
  echo "ошибка: тег '$tag' должен начинаться с 'v' (например v0.1.0)" >&2
  exit 1
fi

# version из секции [workspace.package]
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
if [[ "${tag#v}" != "$version" ]]; then
  echo "ошибка: тег '$tag' не совпадает с версией $version в $cargo_toml; поднимите version и перевыпустите тег" >&2
  exit 1
fi
echo "ok: тег $tag совпадает с версией $version"
