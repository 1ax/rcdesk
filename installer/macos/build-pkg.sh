#!/usr/bin/env bash
# Собирает rcdesk.app и установщик rcdesk-macos.pkg (только macOS, arm64).
#
# Использование (из корня репозитория, после `cargo build --release -p rcdesk-host`):
#   bash installer/macos/build-pkg.sh target/release 0.1.0 pkg-out
#
# Подпись. Если заданы MACOS_SIGN_P12 (base64 от .p12) и MACOS_SIGN_P12_PASSWORD,
# бандл подписывается самоподписанным сертификатом из временной связки. DR
# подписи -- `identifier "app.rcdesk.agent" and certificate leaf = H"<sha1>"`,
# он не зависит от cdhash, поэтому TCC-разрешения («Запись экрана»,
# «Универсальный доступ») переживают обновление (долг D28). Без секретов --
# ad-hoc подпись (DR = cdhash) и предупреждение: форки и PR не падают.
#
# Оба бинарника (rcdesk-host и rcdesk-agent) подписываются одним идентификатором
# app.rcdesk.agent: DR `identifier "app.rcdesk.agent" and certificate leaf =
# H"<sha1>"` общий, поэтому гранты TCC («Универсальный доступ», «Запись экрана»)
# действуют для любого из них, в том числе на экране входа. Там же нужна секция
# __CGPreLoginApp,__cgpreloginapp (её кладёт host/build.rs) -- проверяем ниже.
#
# Hardened runtime НЕ включаем: нотаризации нет, а entitlements под захват
# экрана и ввод не продуманы. .pkg не подписываем: Developer ID Installer нет.
set -euo pipefail

if [ "$#" -ne 3 ]; then
  echo "использование: $0 <release_dir> <version> <out_dir>" >&2
  exit 2
fi

release_dir="$1"
version="$2"
out_dir="$3"
here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

for f in rcdesk-agent rcdesk-host; do
  [ -f "$release_dir/$f" ] || { echo "нет $release_dir/$f" >&2; exit 1; }
  # Страховка: секцию кладёт host/build.rs; без неё ввод на экране входа молча
  # перестанет работать.
  if ! otool -l "$release_dir/$f" | grep -q cgpreloginapp; then
    echo "в $release_dir/$f нет секции __CGPreLoginApp,__cgpreloginapp (см. host/build.rs)" >&2
    exit 1
  fi
  echo "секция __cgpreloginapp на месте: $f"
done

work="$(mktemp -d "${TMPDIR:-/tmp}/rcdesk-pkg.XXXXXX")"
keychain=""
orig_keychains=()
# Список поиска связок пользователя (по одной на строку, без кавычек).
read_keychains() {
  security list-keychains -d user | sed -e 's/^[[:space:]]*"//' -e 's/"$//'
}
cleanup() {
  if [ -n "$keychain" ]; then
    # Возвращаем исходный список поиска и удаляем связку вместе с ключом.
    if [ "${#orig_keychains[@]}" -gt 0 ]; then
      security list-keychains -d user -s "${orig_keychains[@]}" 2>/dev/null || true
    fi
    security delete-keychain "$keychain" 2>/dev/null || true
  fi
  rm -rf "$work"
}
trap cleanup EXIT

# 1. Бандл во временном staging (корень pkg -- только rcdesk.app).
root="$work/root"
app="$root/rcdesk.app"
mkdir -p "$app/Contents/MacOS" "$app/Contents/Resources"
cp "$release_dir/rcdesk-agent" "$release_dir/rcdesk-host" "$app/Contents/MacOS/"
chmod 755 "$app/Contents/MacOS/rcdesk-agent" "$app/Contents/MacOS/rcdesk-host"
cp "$here/rcdesk.icns" "$app/Contents/Resources/rcdesk.icns"
sed "s/__VERSION__/$version/g" "$here/Info.plist" >"$app/Contents/Info.plist"
plutil -lint "$app/Contents/Info.plist"
# Расширенные атрибуты (provenance и т.п.) pkgbuild упаковал бы как
# AppleDouble-файлы `._*` внутри бандла -- убираем до подписи.
xattr -cr "$app"

# 2. Подпись: сначала вложенный rcdesk-host, затем бандл целиком (без --deep).
if [ -n "${MACOS_SIGN_P12:-}" ] && [ -n "${MACOS_SIGN_P12_PASSWORD:-}" ]; then
  keychain="$work/sign.keychain-db"
  kc_pass="$(uuidgen)"
  printf '%s' "$MACOS_SIGN_P12" | base64 -d >"$work/sign.p12"
  security create-keychain -p "$kc_pass" "$keychain"
  security set-keychain-settings -lut 3600 "$keychain"
  security unlock-keychain -p "$kc_pass" "$keychain"
  security import "$work/sign.p12" -k "$keychain" -P "$MACOS_SIGN_P12_PASSWORD" -T /usr/bin/codesign
  rm -f "$work/sign.p12"
  security set-key-partition-list -S apple-tool:,apple: -s -k "$kc_pass" "$keychain" >/dev/null
  # Связку нужно видеть codesign'у: добавляем в список поиска (cleanup уберёт).
  while IFS= read -r line; do
    [ -n "$line" ] && orig_keychains+=("$line")
  done < <(read_keychains)
  security list-keychains -d user -s "$keychain" "${orig_keychains[@]}"
  sha1="$(security find-identity -p codesigning "$keychain" | awk '/^ *1\)/ {print $2; exit}')"
  [ -n "$sha1" ] || { echo "в связке нет идентичности для подписи" >&2; exit 1; }
  echo "подпись сертификатом $sha1"
  sign=(codesign --force --keychain "$keychain" -s "$sha1")
else
  echo "ПРЕДУПРЕЖДЕНИЕ: MACOS_SIGN_P12 не заданы -- ad-hoc подпись; TCC-разрешения сбросятся при обновлении" >&2
  sign=(codesign --force -s -)
fi
"${sign[@]}" --identifier app.rcdesk.agent "$app/Contents/MacOS/rcdesk-host"
"${sign[@]}" --identifier app.rcdesk.agent "$app"

# 3. Проверка и DR в лог.
codesign --verify --strict --verbose=2 "$app"
codesign -d -r- "$app" 2>&1
codesign -d -r- "$app/Contents/MacOS/rcdesk-host" 2>&1

# 4. pkg. Component plist генерируем pkgbuild --analyze (набор ключей и
# формат определяет сам pkgbuild, в репо нечему устаревать) и ставим
# BundleIsRelocatable=false: иначе Installer найдёт старую копию rcdesk.app
# где угодно на диске и обновит её вместо /Applications.
component="$work/component.plist"
pkgbuild --analyze --root "$root" "$component"
plutil -replace 0.BundleIsRelocatable -bool false "$component"

mkdir -p "$out_dir"
pkgbuild \
  --root "$root" \
  --component-plist "$component" \
  --install-location /Applications \
  --identifier app.rcdesk.pkg \
  --version "$version" \
  --scripts "$here/scripts" \
  "$out_dir/rcdesk-macos.pkg"
echo "готово: $out_dir/rcdesk-macos.pkg"
