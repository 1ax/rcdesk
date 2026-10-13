#!/usr/bin/env bash
# Ежедневный шифрованный бэкап SQLite-базы rcdesk-server (том rcdesk_db-data).
# Что делает: консистентная копия через `sqlite3 .backup` (внутри образа
# сервера), проверка `PRAGMA integrity_check`, шифрование GPG на открытый ключ
# владельца (открытая копия БД на диск вне временного каталога не попадает и
# удаляется при выходе), локальная ротация, отгрузка в Box через rclone,
# ротация в Box и heartbeat Better Stack. Любая ошибка -- уведомление в Telegram.
#
# Cron пользователя rcdesk (ставится один раз вручную, infra/README.md,
# раздел «Бэкап БД»):
#   30 4 * * * $HOME/app/backup-db.sh >> $HOME/backups/backup.log 2>&1
# Ручной прогон:
#   bash ~/app/backup-db.sh
#
# Зависимости: bash, docker, gpg, rclone (~/bin), curl, flock, find, date --
# без jq (на сервере его нет).
set -euo pipefail

PATH="$HOME/bin:$PATH"

BACKUP_DIR="${BACKUP_DIR:-$HOME/backups}"
BACKUP_ENV_FILE="${BACKUP_ENV_FILE:-$HOME/app/.env}"
BACKUP_VOLUME="${BACKUP_VOLUME:-rcdesk_db-data}"
BACKUP_DB_NAME="${BACKUP_DB_NAME:-rcdesk.db}"
BACKUP_IMAGE="${BACKUP_IMAGE:-}"
BACKUP_RCLONE_REMOTE="${BACKUP_RCLONE_REMOTE:-box:rcdesk-backups}"
BACKUP_LOCAL_RETENTION_DAYS="${BACKUP_LOCAL_RETENTION_DAYS:-14}"
BACKUP_REMOTE_RETENTION_DAYS="${BACKUP_REMOTE_RETENTION_DAYS:-90}"
BACKUP_GPG_RECIPIENT="${BACKUP_GPG_RECIPIENT:-}"
BACKUP_HEARTBEAT_URL="${BACKUP_HEARTBEAT_URL:-}"
BACKUP_NOTIFY_FILE="${BACKUP_NOTIFY_FILE:-}"
TRAFFIC_TG_BOT_TOKEN="${TRAFFIC_TG_BOT_TOKEN:-}"
TRAFFIC_TG_CHAT_ID="${TRAFFIC_TG_CHAT_ID:-}"

step="старт"
work=""

# Читает значение KEY=VALUE из файла через grep/cut (не source -- файл это .env
# с секретами, читаем только конкретный ключ, не выполняем его как shell).
read_kv() {
  local key="$1" file="$2" val=""
  if [[ -f "$file" ]]; then
    val="$(grep -E "^${key}=" "$file" | tail -n1 | cut -d= -f2- || true)"
  fi
  printf '%s' "$val"
}

log() {
  printf '%s backup-db: %s\n' "$(date -u +%Y-%m-%dT%H:%M:%SZ)" "$1"
}

send_notify() {
  local text="$1"
  if [[ -n "$BACKUP_NOTIFY_FILE" ]]; then
    printf '%s\n' "$text" >> "$BACKUP_NOTIFY_FILE"
    return 0
  fi
  if [[ -z "$TRAFFIC_TG_BOT_TOKEN" || -z "$TRAFFIC_TG_CHAT_ID" ]]; then
    echo "backup-db: notify skipped: TRAFFIC_TG_BOT_TOKEN/TRAFFIC_TG_CHAT_ID not set" >&2
    return 1
  fi
  if curl -fsS -m 20 -X POST "https://api.telegram.org/bot${TRAFFIC_TG_BOT_TOKEN}/sendMessage" \
      --data-urlencode "chat_id=${TRAFFIC_TG_CHAT_ID}" \
      --data-urlencode "text=${text}" >/dev/null; then
    echo "backup-db: notified: $text" >&2
    return 0
  fi
  echo "backup-db: warning: telegram send failed" >&2
  return 1
}

# Общий обработчик ошибок: сообщение в Telegram и stderr, exit 1.
on_error() {
  local line="$1" msg
  trap - ERR
  msg="rcdesk бэкап БД УПАЛ: ${step} (строка ${line})"
  echo "backup-db: $msg" >&2
  send_notify "$msg" || true
  exit 1
}

cleanup() {
  # В $work лежит открытая копия БД -- не должна пережить запуск.
  if [[ -n "$work" && -d "$work" ]]; then
    rm -rf "$work"
  fi
}

# Одновременный запуск (cron + ручной прогон) пропускаем молча.
step="создание каталога бэкапов"
mkdir -p "$BACKUP_DIR"
chmod 700 "$BACKUP_DIR"
exec 9>"$BACKUP_DIR/.lock"
if ! flock -n 9; then
  echo "backup-db: lock busy, skip" >&2
  exit 0
fi

trap 'on_error $LINENO' ERR
trap cleanup EXIT

step="чтение настроек"
for key in BACKUP_GPG_RECIPIENT BACKUP_HEARTBEAT_URL TRAFFIC_TG_BOT_TOKEN TRAFFIC_TG_CHAT_ID; do
  if [[ -z "${!key}" ]]; then
    printf -v "$key" '%s' "$(read_kv "$key" "$BACKUP_ENV_FILE")"
  fi
done
if [[ -z "$BACKUP_IMAGE" ]]; then
  image_tag="$(read_kv IMAGE_TAG "$BACKUP_ENV_FILE")"
  BACKUP_IMAGE="ghcr.io/1ax/rcdesk-server:${image_tag:-latest}"
fi

step="проверка BACKUP_GPG_RECIPIENT"
if [[ -z "$BACKUP_GPG_RECIPIENT" ]]; then
  echo "backup-db: BACKUP_GPG_RECIPIENT не задан (env или $BACKUP_ENV_FILE)" >&2
  on_error "$LINENO"
fi

step="временный каталог"
work="$(mktemp -d "$BACKUP_DIR/.work.XXXXXX")"
log "старт, образ $BACKUP_IMAGE, том $BACKUP_VOLUME"

# `.backup` корректно читает WAL и даёт согласованную копию; простое копирование
# файла БД -- нет. `--user 0`: том принадлежит nobody, а каталог вывода --
# rcdesk, обычный пользователь контейнера не смог бы писать в один из них.
step="sqlite3 .backup"
docker run --rm --user 0 -v "$BACKUP_VOLUME:/data" -v "$work:/out" \
  --entrypoint sqlite3 "$BACKUP_IMAGE" \
  "/data/$BACKUP_DB_NAME" ".backup /out/$BACKUP_DB_NAME"

step="PRAGMA integrity_check"
integrity="$(docker run --rm --user 0 -v "$work:/out" \
  --entrypoint sqlite3 "$BACKUP_IMAGE" \
  "/out/$BACKUP_DB_NAME" "PRAGMA integrity_check")"
if [[ "$integrity" != "ok" ]]; then
  echo "backup-db: integrity_check вернул: $integrity" >&2
  on_error "$LINENO"
fi

# gpg сжимает сам, gzip не нужен. Пишем в .part и переименовываем -- cron-ротация
# и rclone не должны увидеть недописанный файл.
step="gpg шифрование"
ts="$(date -u +%Y%m%d-%H%M%S)"
out="$BACKUP_DIR/rcdesk-$ts.db.gpg"
gpg --batch --yes --trust-model always --encrypt --recipient "$BACKUP_GPG_RECIPIENT" \
  --output "$out.part" "$work/$BACKUP_DB_NAME"
mv "$out.part" "$out"
size="$(wc -c < "$out" | tr -d ' ')"
log "зашифровано: $out ($size байт)"

step="локальная ротация"
find "$BACKUP_DIR" -maxdepth 1 -name 'rcdesk-*.db.gpg' -type f \
  -mtime "+$BACKUP_LOCAL_RETENTION_DAYS" -delete

step="rclone copy"
rclone copy "$out" "$BACKUP_RCLONE_REMOTE/"
step="rclone delete (ротация в Box)"
rclone delete --min-age "${BACKUP_REMOTE_RETENTION_DAYS}d" "$BACKUP_RCLONE_REMOTE/"

step="heartbeat"
if [[ -n "$BACKUP_HEARTBEAT_URL" ]]; then
  if ! curl -fsS -m 20 "$BACKUP_HEARTBEAT_URL" >/dev/null; then
    echo "backup-db: warning: heartbeat failed" >&2
  fi
fi

log "финиш: $out ($size байт)"
