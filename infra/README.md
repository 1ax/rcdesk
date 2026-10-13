# Инфраструктура

Сервер: FastVPS (Debian 12), пользователь `rcdesk` (в группе `docker`), его
домашний каталог `/var/www/rcdesk/data`. Внешний FastPanel-Nginx уже отдаёт
статику `https://rcdesk.app` из `/var/www/rcdesk/data/www/rcdesk.app` и
проксирует `/ws` и `/healthz` на `127.0.0.1:8100` (порты 8080/8090 заняты
другими проектами на этом сервере). Сертификаты пользователю `rcdesk` не
читаются, поэтому coturn работает без TLS (только `3478` udp/tcp) -- TURN-TLS
на `5349` планируется позже. Файрвола на сервере нет.

Состав:

- `docker-compose.prod.yml` -- `server` (сигнальный сервер, `127.0.0.1:8100`)
  и `coturn` (host-сеть, `3478` + relay-диапазон `49152-65535`).
  coturn ограничен: `denied-peer-ip` на внутренние адреса, квоты,
  без TCP-relay; контейнеры идут с `cap_drop`, логи ротируются. Откат при
  проблеме с привязкой 3478 -- убрать `cap_drop`/`cap_add` у coturn и
  `docker compose up -d`.
- `.env.prod.example` -- шаблон секретов; на сервере копируется в `.env`
  рядом с compose-файлом (`chmod 600`, владелец `rcdesk`).

Образ публикуется `.github/workflows/deploy.yml` в
`ghcr.io/1ax/rcdesk-server`, деплой идёт при push в `main` после зелёных
гейтов CI (`.github/workflows/ci.yml`, вызывается как reusable workflow).

## Первый запуск на сервере (владелец/архитектор)

```bash
# под пользователем rcdesk
mkdir -p ~/app
cp infra/.env.prod.example ~/app/.env
chmod 600 ~/app/.env
```

Заполнить `~/app/.env`:

- `TURN_SECRET` и `RCDESK_TURN_SECRET` -- **один и тот же** секрет
  (`openssl rand -hex 32`): coturn проверяет TURN-креды тем же ключом,
  которым `rcdesk-server` их подписывает (`server/src/ice.rs`, REST API
  `use-auth-secret`). Разные значения -- TURN не работает молча (клиент
  получит креды, coturn их отклонит).
- `TURN_EXTERNAL_IP` -- внешний IP сервера (не зашивается в репозиторий).
- `RCDESK_STUN_URLS`/`RCDESK_TURN_URLS`/`RCDESK_TURN_TTL_SECS` -- обычно
  можно оставить значения из `.env.prod.example`.

Если образ `ghcr.io/1ax/rcdesk-server` приватный, авторизоваться перед первым
`pull` (PAT с `read:packages`): `docker login ghcr.io`.

```bash
cd ~/app
docker compose -f docker-compose.prod.yml pull
docker compose -f docker-compose.prod.yml up -d
```

Дальнейшие деплои автоматизирует workflow `deploy` (см. выше).

### Проверка

```bash
curl -s http://127.0.0.1:8100/healthz   # ok
```

Снаружи: `https://rcdesk.app/healthz` должен вернуть `ok`, а открытие `wss`
на `https://rcdesk.app/ws` (например, через DevTools -> Network -> WS при
подключении веб-клиента) -- код `101 Switching Protocols`.

Проверка TURN: собрать креды из `Registered`/`Joined` (например,
`RUST_LOG=debug cargo run -p rcdesk-host -- serve --server wss://rcdesk.app/ws`
и посмотреть лог, либо открыть DevTools на веб-клиенте и залогировать
`msg.ice_servers` из сигналинга) и подставить `urls`/`username`/`credential`
в https://webrtc.github.io/samples/src/content/peerconnection/trickle-ice/ --
относительно TURN-сервера должны появиться `relay`-кандидаты.

### Откат

Деплой выставляет `IMAGE_TAG=sha-<7 символов коммита>`, поэтому каждый
выкаченный образ остаётся на сервере и в ghcr.io под своим тегом. Откат на
предыдущий образ (тег -- со страницы Deploy в Actions или из
`docker images ghcr.io/1ax/rcdesk-server`):

```bash
cd ~/app && IMAGE_TAG=sha-<предыдущий> docker compose -f docker-compose.prod.yml up -d server
```

Следующий деплой вернёт свежий sha.

## nginx (FastPanel)

Конфиг сайта `rcdesk.app` лежит на VPS в `/etc/nginx/fastpanel2-sites/rcdesk/rcdesk.app.conf`
(root). Источник правды -- `infra/nginx/rcdesk.app.conf`: полный `server {}` с нашими
добавками (security-заголовки, `Cache-Control` на `index.html` и `/assets/`, CSP).
Строки панели (`listen`, `ssl_certificate*`, `root`, `include`, логи) в нём как у панели;
при перевыпуске сертификата пути к `.crt`/`.key` брать из живого файла.

Применение (root): заменить файл целиком, затем

```bash
nginx -t && systemctl reload nginx
```

Проверка:

```bash
curl -sI https://rcdesk.app/ | grep -i -E "cache-control|content-security|strict-transport"
curl -sI https://rcdesk.app/assets/<файл> | grep -i cache-control
```

Для `/` ожидается `Cache-Control: no-cache` и
`Content-Security-Policy-Report-Only`, для ассета -- `public, max-age=31536000, immutable`.

Порядок включения CSP: Report-Only -> живой сеанс (вход, видео, буфер обмена,
передача файла, passkey) без сообщений `Content Security Policy` в консоли ->
переименовать заголовок в `Content-Security-Policy` (в обоих `location`).

## Монитор трафика

`traffic-monitor.sh` считает исходящий трафик VPS за месяц и уведомляет в
Telegram при пересечении порогов бюджета (ARCHITECTURE.md §11, D22):

- **TURN-релей** -- сумма метрик `turn_total_traffic_sentb` +
  `turn_total_traffic_peer_sentb` из Prometheus-эндпойнта coturn
  (`127.0.0.1:9641/metrics`, включается `--prometheus` в
  `docker-compose.prod.yml`).
- **Исходящий VPS целиком** -- `tx_bytes` внешнего интерфейса (`ens3`,
  `/sys/class/net/ens3/statistics/tx_bytes`).

Состояние -- помесячный накопитель `~/app/traffic/<YYYY-MM>.state`
(счётчики coturn/интерфейса сбрасываются при рестарте, скрипт хранит
последнее снятое значение и накопленную сумму). Пороги: 50/80/100 % от
`TRAFFIC_RELAY_BUDGET` (5 ТБ по умолчанию) для релея и 90 % от
`TRAFFIC_IFACE_BUDGET` (10 ТБ) для интерфейса; уведомление на порог уходит
один раз (метка пишется в state только при успешной отправке).

Деплой (`.github/workflows/deploy.yml`) копирует скрипт в `~/app/` и делает
его исполняемым при каждом деплое. Crontab деплой не трогает -- ставится
один раз вручную под пользователем `rcdesk`:

```bash
mkdir -p ~/app/traffic
crontab -e
# добавить строку:
*/10 * * * * $HOME/app/traffic-monitor.sh >> $HOME/app/traffic/monitor.log 2>&1
```

Ручная проверка:

```bash
bash ~/app/traffic-monitor.sh
cat ~/app/traffic/$(date -u +%Y-%m).state
```

Проверка отправки в Telegram -- на копии state во временном каталоге (в
пустом каталоге первый запуск лишь фиксирует счётчики, итог равен нулю и
порог не сработает; на боевом state метка `relay50` осталась бы навсегда):

```bash
mkdir -p /tmp/t && cp ~/app/traffic/$(date -u +%Y-%m).state /tmp/t/
TRAFFIC_DIR=/tmp/t TRAFFIC_RELAY_BUDGET=1 bash ~/app/traffic-monitor.sh
```

## Бэкап БД

`backup-db.sh` раз в сутки делает консистентную копию SQLite (`sqlite3 .backup`
внутри образа сервера, через `docker run --entrypoint sqlite3`), проверяет
`PRAGMA integrity_check` и шифрует GPG на ключ владельца
`CE0B 723C BB61 6652 6661 7F6A B9B1 E3BB 79CE 3DA7` («CrewTally Backup», общий
для проектов владельца). Открытая копия БД на диск вне временного каталога не
пишется и удаляется при выходе. Дальше: `~/backups` (14 дней) -> Box через
rclone (90 дней) -> heartbeat Better Stack. Любая ошибка шлёт «УПАЛ» в
Telegram (тот же бот, что у монитора трафика).

Разовая настройка под пользователем `rcdesk` на VPS:

1. rclone без root: скачать `rclone-current-linux-amd64.zip` с rclone.org,
   распаковать бинарник `rclone` в `~/bin` (скрипт добавляет `~/bin` в `PATH`).
2. Box. На Mac: `brew install rclone`, `rclone authorize "box"` -- получить
   токен. На VPS: `rclone config` -> `n`, имя `box`, тип `box`, client id и
   secret пустые, `box_sub_type=user`, на вопрос про браузер ответить `n` и
   вставить токен. Проверка: `rclone lsd box:`, затем
   `rclone mkdir box:rcdesk-backups`. Конфиг `~/.config/rclone/rclone.conf`
   должен быть писаемым: refresh-токен обновляется при запусках (без запусков
   он живёт 60 дней).
3. Импортировать открытый ключ: `gpg --import pub.asc`.
4. В `~/app/.env` заполнить `BACKUP_GPG_RECIPIENT` (отпечаток ключа) и
   `BACKUP_HEARTBEAT_URL` (Better Stack heartbeat).
5. Crontab (`crontab -e`):

```bash
mkdir -p ~/backups
30 4 * * * $HOME/app/backup-db.sh >> $HOME/backups/backup.log 2>&1
```

Деплой копирует скрипт в `~/app/` и делает его исполняемым; crontab ставится
один раз вручную.

Ручной прогон:

```bash
bash ~/app/backup-db.sh
ls -l ~/backups && rclone ls box:rcdesk-backups
```

Восстановление. На Mac расшифровать и проверить:

```bash
gpg --decrypt rcdesk-<дата>.db.gpg > rcdesk.db
sqlite3 rcdesk.db 'PRAGMA integrity_check'
```

На VPS (файл `rcdesk.db` в текущем каталоге, `<образ>` -- образ сервера):

```bash
docker compose -f docker-compose.prod.yml stop server
docker run --rm --user 0 -v rcdesk_db-data:/data -v "$PWD:/in" --entrypoint sh <образ> \
  -c 'cp /in/rcdesk.db /data/rcdesk.db && rm -f /data/rcdesk.db-wal /data/rcdesk.db-shm && chown nobody:nogroup /data/rcdesk.db'
docker compose -f docker-compose.prod.yml start server
```

## Внешний мониторинг (Better Stack)

Аккаунт владельца, настройка в дашборде, в репо не хранится. Три монитора:

1. Keyword-монитор `https://rcdesk.app/healthz`, интервал 3 мин, алерт «тело не
   содержит `ok`» -- ловит падение и деградацию (healthz делает `SELECT 1` и
   берёт мьютекс реестра).
2. TCP-монитор `turn.rcdesk.app:3478` -- coturn лёг.
3. Heartbeat «rcdesk backup», период 1 день, grace 2 ч; URL лежит в
   `~/app/.env` как `BACKUP_HEARTBEAT_URL` (см. «Бэкап БД»).

Канал -- email владельца (free-tier).

Не покрыто и остаётся долгом: диск; сертификат (можно добавить SSL-монитор
Better Stack на `rcdesk.app`).

## Релиз

Релизы собирает `.github/workflows/release.yml`. Тег ставить руками не нужно:

1. Поднять `version` в `[workspace.package]` корневого `Cargo.toml` и в `web/package.json`
   (тест `web/src/version.test.ts` сверяет их), обновить `Cargo.lock`/`package-lock.json`, коммит.
2. `git push origin main`. Job `version` читает версию (`infra/cargo-version.sh`); если тега
   `v<version>` на GitHub ещё нет, он сам ставит его на этот коммит, пушит и в том же прогоне
   собирает релиз. Push без смены версии релиз не создаёт — только деплой (`deploy.yml`).

Резервный путь — тег руками: `git tag vX.Y.Z && git push origin vX.Y.Z`.

При ручном теге workflow сначала проверяет (`infra/release-version-check.sh`), что тег совпадает
с `version` (при несовпадении релиз не создаётся); затем прогоняет гейты (CI) и
собирает хост под Windows и macOS. В GitHub Releases попадают установщик
`rcdesk-setup.exe` (Windows) и zip без установки: `rcdesk-windows-x64.zip`,
`rcdesk-macos-arm64.zip`.

### Подпись манифеста обновления (D50b)

Job `release` публикует `rcdesk-update.json` (версия и sha256 установщиков) и
`rcdesk-update.json.sig` — подпись ed25519 (`infra/release-manifest.sh`). Хост
обновляется только по манифесту с подписью ключа, открытая часть которого зашита в
`host/src/update_verify.rs` (`RELEASE_PUBLIC_KEY`). Закрытый ключ (PEM) — в секрете
репозитория `RELEASE_SIGN_KEY` и в Bitwarden владельца; без секрета релиз падает на
шаге подписи. Смена ключа = новая версия хоста с новым открытым ключом; хосты со
старым ключом обновятся только вручную.

