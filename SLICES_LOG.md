# rcdesk — Журнал слайсов

Хронология работы по слайсам. Обновляется в конце каждого слайса/под-шага.
Архитектурные решения — в ARCHITECTURE.md, здесь статус, итоги, долги.

---

## 📍 Голова: состояние и открытые долги (живой индекс)

> **Этот блок — состояние, не история.** Перезаписывается при каждом изменении.
> Всё, что НИЖЕ разделителя `---`, — append-only история. Архитектор при старте
> чата читает сначала голову, в тело ныряет по ссылке на слайс.

**Состояние на 2026-09-11:**
- Фаза 0 закрыта, CI зелёный на ubuntu/macos/windows (run 34610667883).
- Слайс 1.1 (сигнальный сервер) готов локально; следующий — 1.2 (хост: захват → openh264
  → WebRTC). Разведка API для 1.2 зафиксирована в `docs/host-libs-api-notes.md`.
- Владелец 2026-09-11 разрешил архитектору коммитить самостоятельно на раннем этапе
  («мне пока нечего проверять»); push для проверки CI — в фазе 0 тоже архитектор.
- Решения владельца: аудитория — владелец и небольшая команда; тестовый Windows —
  физический ПК с Windows 10 x64; VPS — существующий FastVPS; домен `rcdesk.app`
  (по RDAP свободен на 2026-09-11, покупка за владельцем); репо публичное;
  приоритет после MVP — буфер обмена и передача файлов.
- Окружение архитектора: Apple M4 / 16 ГБ, macOS 26.6, Chrome 153, Safari 26.6,
  Rust 1.98 (rustup, `~/.cargo/bin`), Node 26, Go 1.27, Docker, UTM.

**Открытые долги:**
- ⚪ D1: LICENSE для публичного репо не выбран (развилка владельцу: MIT / Apache-2.0 / без лицензии).
- ⚪ D2: домен `rcdesk.app` не куплен; проверить цену в корзине регистратора (Google может ставить премиум).

---

## Слайс 0 — Бутстрап

### 0.1 Документы и репозиторий (2026-09-11)

- Изучена схема «архитектор + executor» из crewtally, перенесена в CLAUDE.md и
  `.claude/agents/executor.md` с поправкой на Rust/TS-гейты и платформенный код.
- Написан ARCHITECTURE.md: стек (Rust хост и сервер, TS клиент, webrtc-rs, scap,
  openh264 → аппаратные кодеры, enigo), транспорт (H.264 CBP + 4 data channels),
  сигналинг (PIN в MVP → устройства и PAKE в фазе 3), два режима хоста, дорожная
  карта на 4 фазы.
- Создан публичный репозиторий `github.com/1ax/rcdesk`.
- Проверены имена: ~80 кандидатов по RDAP в 6 зонах и по GitHub; выбран `rcdesk`
  под домен `rcdesk.app`.

### 0.2 Скелет монорепо и CI (2026-09-11)

- Executor (Sonnet, general-purpose с правилами executor.md — агент из `.claude/agents`
  подхватывается только при старте сессии). Скоуп выдержан, сверка diff чистая.
- Workspace `proto`/`host`/`server`; `proto::signal::{Role, SignalMessage}` с ts-rs
  экспортом в `web/src/generated/` (каталог задан в `.cargo/config.toml`), CI-джоб
  `proto-ts-sync` ловит рассинхрон. `host`: `--version`, tracing, платформенный модуль
  за `cfg` (macos/windows/other) с тестом. `server`: axum `/healthz`, порт `RCDESK_PORT`.
  `web`: Vite 8 + TS 7 strict + vitest 5, `__APP_VERSION__` через `define`.
- Гейты локально (архитектор перепроверил сам): fmt чисто; clippy `Finished`; тесты
  host **2 passed**, proto **3 passed**, server **1 passed**; web `Tests 1 passed (1)`,
  build ✓.
- Версии: ts-rs 12.0.1, axum 0.8.9, tower 0.5.3, http-body-util 0.1.5, vite 8.3.0,
  typescript 7.0.2, vitest 5.0.0, tokio 1.53.1.

## Слайс 1 — MVP по LAN

### 1.1 Сигнальный сервер (2026-09-11)

- `proto::signal`: полный набор MVP-сообщений (Hello, HostRegister/Registered, Join/Joined/
  PeerJoined, Offer/Answer/Ice/Bye, Error) + `IceCandidate`; TS-типы перегенерированы.
- `server`: `registry.rs` (in-memory, `std::sync::Mutex` — секции без `.await`), `ws.rs`
  (`GET /ws`: read-loop + writer-задача через unbounded mpsc; первое сообщение — Hello;
  host: register → forward; client: join → forward; Bye и обрывы освобождают хост, PIN
  живёт пока хост подключён; один хост — одна сессия). Крейт получил `lib.rs`, чтобы
  интеграционные тесты видели `app()`/`Registry`.
- Тесты: 4 unit в registry, 7 интеграционных в `server/tests/signaling.rs` через
  tokio-tungstenite (регистрация, join, пересылка offer/answer/ice, unknown pin, host busy,
  освобождение хоста после отключения клиента, «первое сообщение не Hello»).
- Гейты (архитектор перепроверил): fmt чисто; clippy `Finished`; тесты host 2, proto 5,
  server lib 5, signaling 7 — все `ok`; web `Tests 1 passed (1)`, build ✓.
- Архитектор ослабил пины версий до мажора (`rand = "0.10"` и т.п.) — конвенция workspace.
- Разведка перед 1.2 (архитектор, пробная сборка в песочнице): `webrtc` 0.20 — новая
  архитектура поверх sans-IO `rtc`; `scap` на macOS отдаёт NV12, на Windows BGRA; PLI
  приходит как `TrackLocalEvent::OnRtcpPacket`. Всё в `docs/host-libs-api-notes.md`.
