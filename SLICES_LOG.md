# rcdesk — Журнал слайсов

Хронология работы по слайсам. Обновляется в конце каждого слайса/под-шага.
Архитектурные решения — в ARCHITECTURE.md, здесь статус, итоги, долги.

---

## 📍 Голова: состояние и открытые долги (живой индекс)

> **Этот блок — состояние, не история.** Перезаписывается при каждом изменении.
> Всё, что НИЖЕ разделителя `---`, — append-only история. Архитектор при старте
> чата читает сначала голову, в тело ныряет по ссылке на слайс.

**Состояние на 2026-09-14 (конец четвёртого чата, слайс 2.3 закрыт кодом, ждёт стенда):**
- ✅ **Фаза 1 в проде** (https://rcdesk.app), ✅ **2.1 Windows-хост** на стенде (GDI), ✅ **2.2 кодеры**
  (VT на macOS, MFT на Windows, openh264 QP ≤ 30 как откат; Deploy `34777760519` зелёный на трёх ОС).
- ✅ **Слайс 2.3 адаптивный битрейт/fps — в `main`, пять коммитов** (см. запись 2.3): `ff8a065`
  `Encoder::set_rate` у трёх кодеров + пейсинг fps в конвейере; `4662595` REMB/RR из RTCP как события
  сессии; `c2244eb` контроллер `host/src/adapt` (REMB ниже отправленного → срез, потери > 10 % → срез,
  < 2 % → пробинг +10 %/с, кодер не успевает → ступень fps вниз, бюджет кадра 0.02 бит/пиксель → fps
  раньше битрейта), `--no-adapt`, `target … (reason)` в оверлее; фикс нулевого тика и доставки `Quality`.
  **Решение слайса:** свой TWCC-BWE не пишем — Chrome шлёт REMB (оценка его GCC) каждые 200 мс, потому
  что хост не ставит расширение transport-cc в исходящие пакеты; включать `TwccSender` нельзя без своего
  GCC (REMB пропадёт). Смена разрешения — долг D20.
- ⏳ **Стенд владельца — не проверен.** Что смотреть: под видео на стенде (openh264 или MFT) должны
  появиться строки `adapt: new rate target … reason="encoder"` и/или `"remb"`, jb в оверлее не должен
  расти до 80 мс, как в 2.2f; для сравнения «до» — `serve --no-adapt`. `main` запушен, Deploy `34820127376`
  зелёный (артефакт `rcdesk-host-windows-x64` для стенда — из него).
- Локально проверено (Mac, синтетика 720p, Chrome 153): `--bitrate 300` → решение `bitrate`, fps 15,
  оверлей `target 0.3 Mbit/s @ 15 fps (bitrate)`; дефолт 6000 → 0 решений за 16 с при REMB 0.79–1.36 Мбит/с
  (поток 0.8), 116 RR. Реальный захват release-бинарником не проверен: у `target/release/rcdesk-host` нет
  разрешения «Запись экрана» (scap: «no capturable display found»).
- ▶️ **Следующий слайс — 2.4 мультимонитор** (D7), либо сначала живая проверка 2.3 на стенде и, если
  адаптация не спасает openh264 на 1080p, — даунскейл (D20). Новый чат — на слайс.
- Как запустить: `docs/dev-run.md` (локально), `docs/host-windows.md` (стенд), `infra/README.md` (прод).
- Окружение владельца: Apple M4 / macOS 26.6 / Chrome 153 / Safari 26.6, Rust 1.98 в `~/.cargo/bin`;
  Win10 Pro 22H2 x64 стенд (ATI Radeon HD 4600 → GDI, аппаратного H.264 нет), exe в `d:\rc\`.
- Правила процесса (в CLAUDE.md): архитектор коммитит сам после сверки, «коммит» от владельца — только
  перед живой проверкой/деплоем (решение 2026-09-13); живые проверки на стенде — только мышь и чтение
  экрана; `git add` только явными путями, пока работает исполнитель.

**За владельцем:**
- Проверить 2.3 на стенде (Mac Chrome → Win10): набор текста и видео при `serve` (адаптация включена)
  и при `serve --no-adapt`; сравнить fps/Мбит/с/jb в оверлее и строки `adapt:` в логе хоста.
- Если push `main` 2.2f ещё не сделан — сделать: на стенде обычный `serve` без флагов должен логировать
  `media foundation encoder name=H264 Encoder MFT hardware=false`.
- Развилка D14: монитор на встроенную графику Ivy Bridge (даст WGC + Quick Sync для `auto`).
- ✅ TLS 1.3 на nginx включён (2026-09-13, `/etc/nginx/conf.d/ssl.conf`, файл не принадлежит пакету).

**Открытые долги:**
- ⚪ D1: LICENSE для публичного репо не выбран (MIT / Apache-2.0 / без лицензии).
- ⚪ D3: `scap` 0.0.8: `Capturer` не `Send`, `unwrap()` на ошибке D3D, `DrawBorderSettings` не настраивается.
  Переход на прямой `windows-capture` 2.x / `screencapturekit` — вместе с D5.
- ⚪ D4: openh264 с `skip_frames(false)` не держит потолок битрейта на сложном контенте; контроллер 2.3
  режет цель по REMB/потерям, но сам кодер по-прежнему может перелетать — проверить на стенде.
- ⚪ D20: **смена разрешения (даунскейл) в адаптации не реализована** — контроллер 2.3 меняет только
  битрейт и fps; если на стенде openh264/GDI не вытягивают 1080p даже при 8–10 fps, нужен скейлер в
  конвейере и пересоздание кодера (VT/MF фиксируют размер при создании).
- ⚪ D21: TWCC-фидбек хостом не используется (см. ARCHITECTURE §5): при переходе на свой GCC или `str0m`
  включить `configure_twcc` и убрать зависимость от REMB. Safari шлёт REMB? — не проверено.
- ⚪ D5: `windows-capture` запинен на =1.4.4 из-за scap 0.0.8; снять пин при переходе на прямой 2.x (с D3).
- ⚪ D6: хост не переподключается к сигнальному серверу при обрыве WS — 3.5.
- ⚪ D7: захват берёт первый `Target::Display`, мышь масштабируется по `main_display()` — мультимонитор в 2.4.
- ⚪ D9: задержка glass-to-glass численно не измерена (цель ≤ 80 мс по LAN); методика ARCHITECTURE §12.
  Косвенно: захват→кодер VT 11.7 мс / openh264 18 мс на M4; `app 1–5 ms`, `jb 0–120 ms`.
- ⚪ D10: TURN без TLS (5349): сертификаты LE в `/var/www/httpd-cert/` пользователю `rcdesk` не читаются.
- ⚪ D11: визибилити пакета `ghcr.io/1ax/rcdesk-server` не проверена через API.
- ⚪ D12: шум логов хоста (`rtc_dtls` WARN, mDNS/STUN таймауты; теперь ещё `warn` про `MaxFrameDelayCount`
  от VT на каждом старте сессии) — уровни логов в 3.5.
- ⚪ D14: GDI-захват 12–14 fps при 1080p на стенде; варианты: даунскейл/dirty-rect, монитор на iGPU (WGC).
- ✅ D15 закрыт в 2.2f: openh264 с потолком QP 30 по умолчанию; на Windows дефолт — MFT (резкий без потолка).
  Остаток ушёл в 2.3: под видео openh264 на стенде копит jitter-буфер (кодер не успевает за 1080p).
- ⚪ D16: WGC на Windows 10 рисует жёлтую рамку; закрывается с D3/D5 на Windows 11.
- ⚪ D17: **MF async-MFT не проверен вживую** (аппаратных MFT нет ни в CI, ни на стенде): цикл событий
  NeedInput/HaveOutput написан по документации; при первом же аппаратном Windows проверить `bench
  --encoder mediafoundation` и `hardware=true async=true` в логе.
- ⚪ D19: **ICE иногда выбирает TURN-релей вместо прямого пути:** третий живой сеанс 2.2f шёл с rtt 100–185 мс
  против 1–9 мс в двух предыдущих при тех же машинах (Mac и стенд в одной сети). Хост (webrtc-rs) —
  controlling-сторона и номинирует первую сошедшуюся пару. Проверить через `chrome://webrtc-internals`
  (расширению архитектора внутренние страницы недоступны) и предпочесть host/srflx-пары — 3.5.
- ⚪ D18: VT-кодер копирует NV12 в `CVPixelBuffer` (~0.3 мс на 1080p); zero-copy через
  `CVPixelBufferCreateWithPlanarBytes` отложен, пока кадр живёт короче сессии.
- ✅ D13 закрыт в 2.1 (Windows-ветки ввода проверены вживую).

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

### 1.2a Видеоконвейер хоста без сети (2026-09-11)

- `capture/`: `RawFrame::{Nv12, Bgra}`, трейт `FrameSource`, `SyntheticSource` (движущийся
  градиент, пейсинг без дрейфа), `ScapSource` за cfg (проверка `has_permission()` до любого
  вызова scap — без разрешения возвращает ошибку, не паникует и не зовёт диалог).
- `encode/`: `I420Frame`, `nv12_to_i420` (деинтерливинг), `bgra_to_i420` через
  `openh264::formats` (SIMD, лимитированный диапазон), обрезка до чётных размеров;
  `OpenH264Encoder` (ScreenContentRealTime, Baseline, ConstantId SPS/PPS, force IDR по
  флагу), `nal_types()` для разбора Annex-B.
- `pipeline/`: поток захвата → слот «последний кадр» (Mutex+Condvar) → поток кодирования →
  bounded mpsc(4). **Архитектор поправил политику дропа:** при полном канале пропускаем
  кадр ДО кодирования (иначе выброшенный P-кадр рвёт цепочку ссылок декодера); если дроп
  всё же случился — принудительный ключевой кадр.
- CLI на clap: `list-displays`, `bench --synthetic|--display --fps --bitrate --seconds --dump`.
- Гейты (архитектор перепроверил): host **11 passed**, proto 5, server 5, signaling 7; web ✓.
  Bench синтетика 1280x720@30, 3 с: `captured=91 encoded=91 dropped=0 keyframes=2`,
  `avg_fps=30.09`, 583 кбит/с (контент простой). openh264 из исходников собирается ~11 с.
- Отклонения исполнителя приняты: главный дисплей = первый `Target::Display`; размер через
  `get_output_frame_size()`; новый lint 1.98 `chunks_exact_to_as_chunks` учтён.

### 1.2b WebRTC-транспорт и сигналинг хоста (2026-09-11)

- `host` стал lib+bin. `transport::PeerSession`: PC с одним кодеком H.264 pt 102 (`42e01f`,
  rtcp-fb nack/pli/fir/remb), видеотрек `TrackLocalStaticSample`, 4 data channel до оффера
  (`input`, `pointer` unordered/0 retransmits, `control`, `file`), события `SessionEvent`
  (LocalIce, ConnectionState, DataChannelMessage, KeyframeRequested). `start_video` ждёт
  `Connected` (иначе SRTP молча дропает), затем форсирует ключевой кадр и пишет сэмплы.
- **Находка исполнителя:** `NoopInterceptor` в `rtc` глотает входящие RTCP; добавлен
  `RtcpForwarder` (ручной интерцептор), иначе PLI никогда не доходил. Заметка в
  `docs/host-libs-api-notes.md` исправлена.
- `signaling::SignalingClient`: Hello → HostRegister → Registered(PIN); PeerJoined → конвейер +
  сессия + Offer; Answer/Ice → в сессию; Bye/Failed → закрыть. `serve` в CLI печатает `PIN:`.
- `host/tests/loopback.rs`: второй PeerConnection в процессе как «браузер»: on_track ≤10 с,
  ≥30 RTP-пакетов, SPS в первых payload'ах, PLI → `KeyframeRequested` + рост `stats.keyframes`.
  Стабильно ~1.25 с.
- CI после 1.2a: ubuntu падал на dead_code (бинарный крейт без scap) — снято переходом на lib;
  **Windows падал внутри scap 0.0.8** (`windows-capture` 1.5.0 сломала `Settings::new`) —
  архитектор запинил `windows-capture = "=1.4.4"` в манифесте хоста.
- Гейты (архитектор перепроверил): host lib 11, loopback 1, proto 5, server 5, signaling 7 —
  все `ok`; web ✓. Ручная проверка исполнителя: `serve --synthetic` печатает PIN, сервер
  логирует `host registered`.

### 1.3 Веб-клиент (2026-09-11)

- `web/src/`: `signaling.ts` (SignalingClient поверх EventTarget, инжектируемый WebSocket),
  `session.ts` (PeerSession: answer-сторона, маппинг ICE snake/camel, data channels по label),
  `stats.ts` (чистые функции из `getStats()`), `app.ts` (экран PIN → экран сессии, `video.play()`
  в жесте клика для Safari, оверлей раз в секунду), `style.css`; Vite proxy `/ws` → :8080.
  19 юнит-тестов с фейками WebSocket/RTCPeerConnection. `docs/dev-run.md`.
- **Живая проверка архитектора в Chrome 153** (сервер + `serve --synthetic` + Vite): нашёл и
  починил два дефекта, которые юнит-тесты не ловили:
  1. `join()` сразу после `connect()` → `WebSocket.send` бросал `InvalidStateError`
     (CONNECTING). Исправлено очередью исходящих до `open` (hello уходит первым), +1 тест.
  2. `display: flex` на `.pin-screen` перебивал атрибут `hidden` → после connected карточка
     PIN оставалась поверх видео. Исправлено `[hidden] { display: none !important }`.
  После фиксов: оверлей `30 fps · 1280x720 · 0.6 Mbit/s · rtt 1 ms · loss 0 · H264`, все
  четыре data channel открыты на хосте, Disconnect → сервер и хост закрыли сессию,
  повторный Connect по тому же PIN — connected за 334 мс.
- Safari не проверен (нет автоматизации из этой сессии) — за владельцем.
- В логе хоста шумят `rtc_dtls` WARN «Unsupported Extension Type» и ошибки mDNS/STUN-таймаутов
  (кандидаты `.local` от Chrome и STUN Google) — не влияют, занести в фазу 3.5 (уровни логов).
- Гейты (архитектор): Rust все `ok`, web `Tests 20 passed (20)`, build ✓.

### 1.4 Ввод мыши и клавиатуры (2026-09-11)

- `proto::input`: `InputMessage` (pointer_move / pointer_button / wheel / key / release_all),
  `PointerButton`; координаты нормализованы [0,1], клавиши — `KeyboardEvent.code`.
- `host/src/input/`: трейт `Injector`, `NoopInjector` (`serve --no-input` и не-macOS/Windows),
  `InputRouter` (выделенный поток, коалесцирование `PointerMove`, учёт нажатого, `ReleaseAll`
  и `Drop` отпускают всё), `keymap` (полные таблицы kVK_* и scan-кодов набора 1),
  `EnigoInjector` (`raw()`, `move_mouse(Abs)`, `scroll`, `main_display()` для масштаба).
  Клик/колесо сначала двигают курсор в свою позицию: каналы `pointer` и `input` независимы.
- `web/src/input.ts`: `mapPointer` с учётом letterbox `object-fit: contain`, `normalizeWheel`,
  `keyToMessage` (repeat пропускается), `attachInput` (rAF-коалесцирование движений, blur /
  visibilitychange → release_all); `app.ts` подключает ввод, когда получены оба канала.
- Гейты (архитектор): Rust все `ok` (host 19 + loopback 1, proto 10, server 5 + 7),
  web `Tests 30 passed (30)`, build ✓.
- Живой захват реального экрана (сборка 1.3 из worktree, порт 8090): Chrome показывает рабочий
  стол 1920x1080, connect 879 мс; bench 5 с на статичном экране — 104 кадра, 20 fps,
  491 кбит/с, p95 кадра 9 КБ.

### 1.5 Форма курсора и прикладной ping (2026-09-11)

- `proto::control::ControlMessage` (CursorShape / CursorHidden / Ping / Pong).
- `host/src/cursor/`: `CursorSource` + `watch()` на выделенном потоке (дедуп по `PartialEq`,
  лимит 128 px, первое состояние всегда «изменение»), `MacCursorSource` (NSCursor →
  перебор `representations()`; `TIFFRepresentation()` давал 280x400 scale=10 — кадр под
  accessibility-размер указателя), `WinCursorSource` (GetCursorInfo/GetIconInfo/GetDIBits,
  проверен только сборкой в CI). Транспорт хранит канал `control` и умеет `send_control`.
- `web/src/cursor.ts`: RGBA → canvas → PNG data URL → `cursor: url() hx hy, auto`
  (`image-set` для scale 2), кэш ≤ 32; `app.ts`: ping/pong раз в секунду → `app N ms`.
  Архитектор добавил старт пинга, если канал пришёл уже открытым.
- Гейты (архитектор): Rust все `ok` (proto 15, host 25 + 1 ignored, loopback 1, server 5+7),
  web `Tests 36 passed (36)`.
- ⚠️ **Процессный инцидент:** коммит `f2082f1` (docs про Safari/домен) сделан архитектором
  через `git add -A`, пока исполнитель писал 1.5, и унёс `proto/src/control.rs`, `Cargo.toml`,
  `Cargo.lock`, часть `host/Cargo.toml`. История не переписывалась (уже в origin); правило
  «явные пути, пока работает исполнитель» добавлено в CLAUDE.md.
- Живая проверка стека: сервер/хост/клиент на VPS-стенде владельца (Safari) и Chrome —
  курсор и `app N ms` проверить при следующем прогоне вместе с 1.6.

### 1.6 Размещение на VPS (2026-09-11)

- `proto::signal::IceServer` + `ice_servers` в `Registered`/`Joined`; `server/src/ice.rs` выдаёт
  STUN и TURN-креды по REST-схеме coturn (`use-auth-secret`, HMAC-SHA1, тест на RFC 2202);
  хост объединяет серверные ICE с `--stun`; клиент берёт из `joined`.
- `server/Dockerfile` (workspace-контекст, `host` заглушкой), `infra/docker-compose.prod.yml`
  (server на 127.0.0.1:8100, coturn в host-сети), `infra/.env.prod.example`, `infra/README.md`,
  `.github/workflows/deploy.yml` (ci.yml как reusable → build-push ghcr → build-web → scp +
  compose up + health).
- Подготовка сервера (архитектор + владелец): пользователь FastPanel `rcdesk`, ключи ops/deploy
  (домашний каталог оказался `/var/www/rcdesk/data`), группа docker, `~/app/.env` с секретом
  TURN (сгенерирован на сервере), nginx-директивы `/ws`, `/healthz`, security-заголовки;
  секреты CI `SSH_*`. Первый деплой прошёл с первого запуска workflow.
- Два дефекта после деплоя: (1) coturn 4.18 не принимает `--no-dtls` (перезапускался по кругу)
  — флаг убран; (2) хост получал `ProtocolVersion` по WSS: tungstenite подключает rustls без
  `tls12`, а nginx FastPanel отдаёт только TLS 1.2 — добавлена прямая зависимость rustls с
  `ring`+`tls12` (подтверждено экспериментом в песочнице: без фичи — алерт, с ней — 101).
- Проверка прода: `/healthz` ok, `/ws` 101, клиент с CDN-хешами; хост через
  `wss://rcdesk.app/ws` → Chrome на https://rcdesk.app: connected, 1920x1080, 29 fps, H264,
  rtt 1 мс, app 1 мс, форма курсора; trickle-ICE с TURN-кредами → 3 relay-кандидата
  46.36.221.168 (udp).
- Гейты (архитектор): Rust все `ok` (server 8 + 7, proto 16, host 25 + loopback), web `Tests 37 passed (37)`.

## Слайс 2 — Windows и производительность

### 2.1 Хост на Windows 10 (2026-09-11 … 2026-09-13)

Четыре под-шага плюс три fix-коммита по итогам CI; всё проверено вживую на стенде владельца
(Windows 10 Pro 22H2, ATI Radeon HD 4600, Ivy Bridge 4 потока, монитор 1920x1080 @ 100 %).

- **2.1a Артефакты и инструкция** (`7183008`): job `build-host` в `deploy.yml` (windows-latest +
  macos-latest, release, upload-artifact), `docs/host-windows.md`. Кросс-компиляции нет: openh264
  собирается из исходников под MSVC.
- **2.1b Клавиатура через SendInput** (`6141097`, фикс `7285cd1`): разведка нашла, что таблица скан-кодов
  хранила расширенные клавиши без префикса `E0` (`ArrowLeft` = `Numpad4` = 0x4B, `MetaLeft` 0x5B), а
  `enigo::raw` выводит `KEYEVENTF_EXTENDEDKEY` из своей неполной таблицы VK. Теперь коды `0xE0xx`,
  отправка своя (`platform/windows/keyboard.rs`), enigo остаётся для мыши/колеса. CI-падение: неиспользуемый
  на Windows импорт `Keyboard`.
- **Стенд:** `bench` и `serve` паниковали внутри scap: `windows-capture` требует Direct3D feature level
  11_0, у HD 4600 — 10.1 (`feature_level=41216`). Решение владельца: другой машины нет, нужен доступ к
  этой → запасной бэкенд.
- **2.1c GDI-захват** (`a8098be`): `capture/gdi.rs` (BitBlt в top-down DIB, `FramePacer`, дедуп одинаковых
  кадров), `platform/windows/d3d.rs` (`wgc_supported()` — пробное `D3D11CreateDevice`, зеркало
  `create_d3d_device` из windows-capture), `platform/windows/dpi.rs` (per-monitor-v2, иначе GDI отдаёт
  уменьшенную картинку при масштабе ≠ 100 %), флаг `--capture auto|wgc|gdi`. У `windows` фичи
  `Win32_UI_HiDpi`, `Win32_Graphics_Direct3D`, `Win32_Graphics_Direct3D11`, `Win32_Graphics_Dxgi`
  (последняя нужна из-за cfg-гейта на `D3D11CreateDevice`). Windows-код скомпилировался в CI с первого раза.
- **Живая проверка ввода (архитектор из Chrome + владелец):** мышь, клики, колесо, буквы/цифры, стрелки,
  Delete, Home, Ctrl+C/V — работают; форма курсора приходит. Клавиши Win на клавиатуре владельца нет.
- **2.1d RTP-время по захвату** (`8f049ae`, фиксы `8c972d8`, `60f1917`): владелец пожаловался на ~1 с
  задержки при наборе. getStats в Chrome: jitter-буфер 850 мс на кадр при целевой 83, 34 freeze.
  Причина — `TrackLocalStaticSample` ставит RTP-время как сумму `duration = 1/fps`, а кадры идут только
  при изменении экрана. Транспорт теперь пакетизирует сам (`TrackLocalStaticRTP` + `Packetizer`,
  `skip_samples` на реальный интервал `captured_at`, кап 10 с), кодер получает wall-clock отметки,
  оверлей показывает `jb N ms`. После фикса: Mac-хост — jb 13 мс, 0 freeze; стенд — jb 0 мс в покое,
  125–149 мс под видео (целевая Chrome 116 мс, т.е. буфер честный), 0 dropped; владелец: «задержка
  приемлемая». CI-падения: синтетика догоняла расписание пачкой кадров (проверка переписана на «ход
  RTP-часов ≈ wall-clock» при источнике 15 fps / сессии 30), затем `FramePacer` терял кадры при
  пересыпающем `sleep` в CI-виртуалке (снап только при отставании > 4 интервалов).
- **Цифры стенда (GDI, 1080p):** `bench --seconds 5` со статичной консолью — 12 кадров (дедуп);
  с видео на Rutube (параллельно с `serve`) — `captured=70 encoded=62 dropped=0`, `avg_fps=12.26`,
  `avg_bitrate_kbps=1097`, средний кадр 11.4 КБ, p95 24.9 КБ. Сессия под видео: 17–18 fps,
  0.5–0.9 Мбит/с, rtt 1–2 мс, app 2–5 мс.
- **Chore** (`2d03643`): CI не запускается отдельно на push в `main` (дубль с Deploy).
- Гейты на закрытии (Deploy `34759205080` зелёный): host lib 32 + 1 ignored, loopback 1, proto 16,
  server 8 + 7, web `Tests 39 passed (39)`.
- ⚠️ Инцидент: при живой проверке архитектор набрал текст в окно Notepad++ на стенде, где сессия
  восстановила `.env` с выделенным содержимым — файл затёрт в буфере (не сохранён), владелец
  восстановил отменой. Правило в голове журнала.

### 2.2 Аппаратные кодеры (2026-09-13)

Пять коммитов, все по схеме «executor → сверка → коммит архитектором» (с этого чата без команды
«коммит», см. CLAUDE.md `a793b88`).

- **2.2a Потолок QP** (`3d2ff33`): `EncoderConfig.max_qp`, `--max-qp`, bench печатает `avg_keyframe_bytes`/
  `avg_delta_bytes`. Исполнитель передал `QpRange::new(0, max)`, и потолок молча не действовал даже при 10:
  `ParamValidationExt` (`encoder_ext.cpp`) сбрасывает диапазон к дефолту screen-content, если любая граница
  ≤ 0. Архитектор поправил нижнюю границу на 1. Замер на Mac (1080p, реальный экран, 5 с): без потолка
  дельта 0.23 КБ / 0.26 Мбит/с; QP ≤ 34 и ≤ 30 — дельта 6.2 КБ / 0.99 Мбит/с, IDR 74 КБ без изменений;
  QP ≤ 24 — IDR 95 КБ. Вывод по D15: размытость при наборе дают дельта-кадры с высоким QP, а не IDR.
  RC openh264 считает бюджет кадра как `bitrate / fMaxFrameRate` (`ratectl.cpp:640`), при подаче 12–18 fps
  вместо 30 кодер тратит вдвое меньше цели.
- **2.2b Трейт на `RawFrame`** (`63dc4b9`): кодер сам конвертирует и ставит `captured_at` из
  `RawFrame::ts()`; `encode::build_encoder(Option<EncoderKind>)`, флаг `--encoder`; сигналинг не знает про
  openh264; bench печатает `encoder=…` и `avg/p95_capture_to_encoded_ms` (синтетика 720p30: 7.5 / 10.8 мс).
- **2.2c VideoToolbox** (`20f8287`): `encode/videotoolbox.rs` за `cfg(macos)`; крейты objc2-video-toolbox/
  core-media/core-video/core-foundation 0.3 (минимальные фичи, без objc2-классов, вторых версий objc2* в
  графе нет). Сессия: CBP AutoLevel, RealTime, без переупорядочивания, AverageBitRate + DataRateLimits,
  MaxAllowedFrameQP из `--max-qp`; `Enable`, не `Require` аппаратного кодера (на CI-виртуалке VT отдаёт
  программный, тесты идут). Вход `CVPixelBufferCreate` 420v + копия плоскостей (D18); выход синхронно через
  `CompleteFrames`, AVCC → Annex-B, SPS/PPS из format description перед IDR, ключевой кадр по отсутствию
  `NotSync`. Находка на M4: `MaxFrameDelayCount = 0` отвергается (`-12900`) — мягкое свойство. Замер
  (1080p, реальный экран, 5 с): VT hardware=true, захват→кодер 11.7 / 12.4 мс (avg / p95) против
  18.0 / 26.3 у openh264; дельта 1.8 КБ против 0.3, IDR ~83 КБ у обоих. Тесты: SPS `profile_idc 0x42` с
  `constraint_set1`, дельты, форс-ключевой, BGRA-вход, `auto` → VT.
- **2.2d Media Foundation** (`322605d`): `encode/mediafoundation.rs` за `cfg(windows)`; фичи `windows`
  `Win32_Media_MediaFoundation`, `Win32_System_{Com,Ole,Variant}` (граф не меняется). `MFTEnumEx` NV12 → H264,
  `auto` = только `MFT_ENUM_FLAG_HARDWARE`, явный флаг допускает программный MFT; async-MFT через
  `MF_TRANSFORM_ASYNC_UNLOCK` и события NeedInput/HaveOutput с кредитами, sync — ProcessInput/ProcessOutput с
  `NEED_MORE_INPUT`; `ICodecAPI` (CBR, MeanBitRate, LowLatency, GOP, MaxQP, ForceKeyFrame через `VARIANT`,
  собранный вручную); выход CBP с откатом на Base; вход NV12 из BGRA через `to_i420` + новый
  `convert::i420_to_nv12`; время захвата едет через `SampleTime` (выход может отставать на кадр);
  ключевой кадр по `CleanPoint` или NAL 5. Проверка: scratch-крейт `cargo check`/`clippy` под
  `x86_64-pc-windows-msvc` чисто; тесты на Windows компилируются только в CI (там программный MFT Microsoft).
- **2.2e Доки** (этот коммит): ARCHITECTURE §2/§4.1/§13/§14, `docs/dev-run.md`, `docs/host-windows.md`.
- **2.2f Дефолты по итогам стенда** (2026-09-14, архитектор сам, без исполнителя): три живых сеанса
  Mac (Chrome) → стенд (GDI 1080p), архитектор подключался по PIN из своей вкладки и читал оверлей раз в
  секунду скриптом; владелец набирал текст в Notepad++ на клавиатуре стенда, затем включал видео:

  | Кодер | Набор: fps / Мбит/с / jb | Видео: fps / Мбит/с / jb | Текст |
  |---|---|---|---|
  | openh264 без потолка | 1–10 / 0.0 / 43–101 мс | 13–20 / 0.4–2.1 / 23–59 мс | мягкий |
  | openh264 `--max-qp 30` | 5–10 / 0.1 / 3–12 мс | 13–17 / 2.1–3.5 / 2→80 мс растёт | чёткий |
  | mediafoundation (программный MS) | 5–11 / 0.1–0.3 / 2–10 мс | 20–23 / 2.7–4.9 / 27–41 мс | чёткий |

  `bench` на стенде: openh264 захват→кодер 67 / 101 мс (avg / p95), с потолком 78 / 103, MF 27 / 33;
  MF IDR 322 КБ против 126, дельта 11 КБ против 3.8 при 2.9 Мбит/с. `auto` (до 2.2f) корректно откатился
  на openh264 с `info`. Решение владельца по рекомендации: `auto` на Windows = любой MFT (аппаратный, затем
  программный Microsoft), openh264 только при ошибке; openh264 получает `DEFAULT_MAX_QP = 30`, когда
  `--max-qp` не задан (VT и MF без потолка). Тест на Windows теперь требует от `auto` `MediaFoundation`.
  Побочная находка — D19 (ICE выбрал TURN-релей в одном из сеансов).
- Гейты на закрытии (Mac): host lib **39 passed; 1 ignored**, loopback 1, proto 16, server 8 + 7,
  web `Tests 39 passed (39)`, build ✓. Deploy `34777760519` (после push владельца): все три ОС зелёные,
  на `windows-latest` host lib 39 passed, включая три теста Media Foundation на программном MFT.
- Попутно: TLS 1.3 на nginx VPS включён (`/etc/nginx/conf.d/ssl.conf`, панель этот параметр в UI не
  показывает; файл не принадлежит пакету fastpanel2), рукопожатие 1.3 и `/ws` 101 проверены с Mac.

### 2.3 Адаптивный битрейт/fps (2026-09-14)

Четвёртый чат. Владелец заранее одобрил все под-шаги («коммить сам, по итогам запуш»), поэтому
развилки решал архитектор; 2.3a и 2.3b исполнялись параллельно (2.3b — в отдельном worktree,
патч наложен после коммита 2.3a).

- **Разведка (архитектор):** живая сессия Mac (синтетика) → Chrome 153 с `rcdesk_host::transport=trace`:
  за 30 с **112 RR и ~100 REMB** (0.9–1.38 Мбит/с при потоке 0.7), TWCC-фидбека нет —
  `register_default_interceptors` ставит `configure_twcc_receiver_only`, расширение объявлено в SDP, но
  `TwccSender` не регистрируется, и Chrome считает receive-side оценку. Решение: строить 2.3 на REMB +
  RR + собственных метриках кодера, свой GCC не писать (ARCHITECTURE §5, долг D21). Особенность REMB:
  receive-side оценка ограничена ~1.5× принятого, поэтому это сигнал только «вниз» и только когда REMB
  ниже реально отправленного.
- **2.3a `set_rate` + пейсинг** (`ff8a065`): `RateTarget { bitrate_kbps, fps }`, `Encoder::set_rate` —
  openh264 через `raw_api().set_option(ENCODER_OPTION_BITRATE/MAX_BITRATE/FRAME_RATE)` (прямая зависимость
  `openh264-sys2` той же 0.9.8: `openh264` реэкспортирует только `DynamicAPI`), VT через
  `VTSessionSetProperty` (AverageBitRate/DataRateLimits/ExpectedFrameRate, вынесено в `apply_rate`),
  MF через `ICodecAPI` MeanBitRate (fps — часть media type, не меняется). Конвейер: `RateControl`
  (отложенная цель применяется в потоке кодирования) + пейсинг до целевого fps с допуском 25 % интервала,
  пропуск сырых кадров до кодера (`paced_out`, ключевой кадр не нужен). Тесты: openh264 4000→200 kbps
  уменьшает дельты без нового IDR; VT смена на лету без ключевого кадра; пейсинг 60 fps → 10.
- **2.3b RTCP-события** (`4662595`): `SessionEvent::Remb { bitrate_bps }`, `::ReceiverReport { fraction_lost,
  total_lost, jitter, rtt }` по SSRC трека; RTT = `now_ntp_mid32 − LSR − DLSR` (1/65536 с, > 10 с — мусор).
  Loopback: REMB 1.5 Мбит/с доходит (±10 kbps на мантиссу), RR с нулевыми потерями и RTT < 2 с приходят
  от report-интерцепторов. Loopback флаковал (~1 прогон из 8) на проверке RTT: `now` из `SystemTime`, а
  NTP в SR у rtc — из пары (SystemTime, Instant), снятой раз на поток; при истинном RTT ~0.1 мс разность
  уходила в минус и после wrapping отбрасывалась как мусор. Фикс: разность как i32, до −100 мс → RTT 0.
- **2.3c контроллер** (`c2244eb`): `host/src/adapt/mod.rs` — чистый, время аргументом, 10 тестов.
  Тик 1 с: битрейт — `loss > 10 %` → `b·(1−0.5·loss)`; иначе свежий REMB < 0.9·sent → `min(b, remb)`;
  иначе `loss < 2 %` и REMB > b (или нет REMB) → `b·1.10`; clamp `[300, --bitrate]`; гистерезис 5 %.
  fps — лестница `[max, 30, 24, 20, 15, 12, 10, 8]` до `min 5`: средняя задержка захват→кодер за тик
  > 1.2 интервала → ступень вниз, три тика < 0.5 интервала ступени выше → вверх; бюджет кадра
  `b/fps ≥ 0.02 бит/пиксель` (1080p: 41 кбит/кадр) режет fps раньше битрейта. Приоритет reason:
  loss > remb > encoder > bitrate > probe. Сигналинг: forward-таск шлёт `Feedback::Frame`, события RTCP →
  `Feedback`, адапт-таск применяет `RateControl::set` и шлёт `ControlMessage::Quality`; оверлей
  `target 3.2 Mbit/s @ 20 fps (remb)`; `serve --no-adapt`. Исполнитель честно доложил, что два числовых
  примера из промпта не проходят через сам алгоритм (REMB сравнивается с sent, а не с b; клэмп до 300
  застревает на 302 из-за гистерезиса) — тесты переписаны на корректную арифметику, алгоритм не менялся.
- **Фикс** (`d6e0a8d`, архитектор сам): `tokio::time::interval` стреляет сразу — решение на пустом окне
  до соединения; `send_control` выбрасывал `Quality` в закрытый канал. Первый тик через секунду,
  `send_control → Result<bool>`, недоставленное объявление повторяется на следующем тике.
- **Локальная проверка (release, синтетика 720p, VT, Chrome):** `--bitrate 300` → на первом тике
  `adapt: new rate target bitrate_kbps=300 fps=15 reason="bitrate"`, оверлей `15 fps · 0.3 Mbit/s · jb 7 ms ·
  target 0.3 Mbit/s @ 15 fps (bitrate)`; дефолт 6000 → 0 решений за 16 с, 28–31 fps, jb 6–9 мс,
  REMB 0.79–1.36 Мбит/с при 0.8, 116 RR. Реальный захват не проверен: release-бинарник без TCC-разрешения.
  Стенд Windows (главная цель слайса — jb до 80 мс под видео у openh264) — за владельцем.
- **Фиксы после Deploy на `macos-latest`** (тест конвейера получал 15 кадров вместо ≥ 20 за секунду при
  30 fps, а 60-fps синтетика — ~25 кадров/с): первая гипотеза — пейсер с жёстким дедлайном выбрасывает
  пачку, которой синтетика догоняет расписание (2.1d); заменён на token bucket с запасом 3 кадра. Падение
  повторилось — настоящая причина в том, что с 2.3a три теста конвейера (три источника + три кодера) идут
  параллельно на 3-vCPU ВМ и голодают. Пейсер вынесен в структуру `Pacer` с четырьмя детерминированными
  тестами на фиктивных таймстампах, интеграционные тесты конвейера сериализованы мьютексом, а проверка
  пейсинга ослаблена до инвариантов, которые медленная машина не ломает (не больше цели, что-то пропущено).
- **Живая проверка на стенде №1 (MFT, адаптация включена) — провал алгоритма:** через 10 с после
  подключения цель 0.7 Мбит/с @ 15 fps (`encoder`), при наборе 0.8 @ 15 (`bitrate`), под видео
  **0.3 Мбит/с @ 5 fps (`remb`)** — против 20–23 fps / 2.7–4.9 Мбит/с у того же кодера в 2.2f. Две
  ошибки модели: (1) REMB Chrome — receive-side оценка, ограничена ~1.5× принятого и растёт ~8 %/с;
  на статичном экране она заведомо ниже любой вспышки изменений, и правило «REMB < отправленного»
  срезало цель по спирали до пола; (2) абсолютная задержка захват→кодер (> 1.2 интервала кадра)
  посчитала MFT перегруженным при 30–50 мс на кадр, хотя он держал 20 fps. **Фикс (архитектор сам):**
  REMB — сигнал перегрузки только при нагруженном канале (отправлено ≥ 80 % цели) и два тика подряд;
  пробинг +10 %/с всегда, когда нет сигнала перегрузки и потерь (иначе цель после среза не восстановить
  на статичном экране); признак «кодер не успевает» — перезапись слота захвата (`PipelineStats::
  overwritten` ≥ 2 за тик), обратно на ступень — после пяти чистых тиков с кадрами. Тесты контроллера
  переписаны (14), host lib 61 passed.
- **Стенд №2 (MFT, адаптация, `972e576`+`f9f2979`):** набор 8–12 fps, 0.1–1.2 Мбит/с, jb 0–3 мс, решений
  нет; видео **20–23 fps, 2.0–5.2 Мбит/с, jb 12–43 мс** — уровень 2.2f. Один ложный срез: при 5.2 Мбит/с
  REMB отстал (растёт ~8 %/с) → `remb` 4.0→2.6, пробинг вернул 6.0 за 12 с, fps не изменился.
  **Фикс:** REMB — сигнал перегрузки только если он не растёт тик к тику (настоящий overuse у
  receive-side оценки — мультипликативное снижение); тест `rising_remb_below_sent_rate_is_ramping_not_congestion`.
- Гейты на закрытии (Mac): host lib **67 passed; 1 ignored**, loopback 1, proto 17, server 8 + 7,
  web `Tests 39 passed (39)`, build ✓. Первый Deploy `34819646587`: web/ubuntu/windows зелёные (на
  `windows-latest` собрался и прошёл MF `set_rate`), macOS красный из-за пейсера — см. фикс выше.
  Второй Deploy `34820127376` (`5554685`) — все три ОС, build-host и deploy зелёные; прод обновлён.
