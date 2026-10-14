#!/usr/bin/env bash
# Подписанный манифест релиза (D50b): rcdesk-update.json + rcdesk-update.json.sig.
# Использование: release-manifest.sh <version> <key.pem> <out_dir> <file>...
# Манифест -- одна строка JSON:
#   {"version":"X.Y.Z","issued_at":N,"assets":{"<имя>":"<sha256>",...}}
# (имена по алфавиту), N -- unix-секунды UTC момента выпуска (без кавычек); берётся из
# переменной окружения RELEASE_MANIFEST_ISSUED_AT (только цифры), иначе `date -u +%s`.
# Подпись -- сырой ed25519 (64 байта) над байтами файла манифеста.
# Выход 0 -- готово; 1 -- сбой подписи; 2 -- неверные аргументы/нет файла.
set -euo pipefail

if [[ $# -lt 4 ]]; then
  echo "usage: $0 <version> <key.pem> <out_dir> <file>..." >&2
  exit 2
fi
version="$1"
key="$2"
out_dir="$3"
shift 3

if [[ ! -f "$key" ]]; then
  echo "ошибка: ключ $key не найден" >&2
  exit 2
fi
if [[ ! -d "$out_dir" ]]; then
  echo "ошибка: каталог $out_dir не найден" >&2
  exit 2
fi
for f in "$@"; do
  if [[ ! -f "$f" ]]; then
    echo "ошибка: файл $f не найден" >&2
    exit 2
  fi
done

issued_at="${RELEASE_MANIFEST_ISSUED_AT:-$(date -u +%s)}"
if [[ ! "$issued_at" =~ ^[0-9]+$ ]]; then
  echo "ошибка: RELEASE_MANIFEST_ISSUED_AT='$issued_at' -- ожидались только цифры (unix-секунды)" >&2
  exit 2
fi

sha256_hex() {
  if command -v sha256sum >/dev/null 2>&1; then
    sha256sum "$1" | cut -d' ' -f1
  else
    shasum -a 256 "$1" | cut -d' ' -f1
  fi
}

# Пары "имя hash" по алфавиту имён.
pairs="$(for f in "$@"; do printf '%s %s\n' "$(basename "$f")" "$(sha256_hex "$f")"; done | LC_ALL=C sort)"

assets=""
while read -r name hash; do
  [[ -n "$assets" ]] && assets+=","
  assets+="\"$name\":\"$hash\""
done <<< "$pairs"

manifest="$out_dir/rcdesk-update.json"
sig="$out_dir/rcdesk-update.json.sig"
printf '{"version":"%s","issued_at":%s,"assets":{%s}}\n' "$version" "$issued_at" "$assets" > "$manifest"

openssl pkeyutl -sign -inkey "$key" -rawin -in "$manifest" -out "$sig"
size="$(wc -c < "$sig" | tr -d ' ')"
if [[ "$size" -ne 64 ]]; then
  echo "ошибка: подпись $sig занимает $size байт, ожидалось 64 (нужен ключ ed25519)" >&2
  exit 1
fi
echo "ok: манифест $manifest подписан ($# ассетов)"
