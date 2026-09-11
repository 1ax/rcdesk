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
