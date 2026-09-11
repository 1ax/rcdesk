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
- 1.1–1.4 закрыты. **Живой захват реального экрана проверен в Chrome** (2026-09-11, после
  выдачи владельцем «Записи экрана»): 1920x1080, H264, подключение 0.9 с, bench на статичном
  экране 20 fps / 0.5 Мбит/с. Следующий — 1.5 (курсор) и приёмка фазы 1 (задержка ≤ 80 мс).
  Запуск стека — `docs/dev-run.md`. Живая инъекция ввода ждёт разрешения «Универсальный
  доступ» у владельца.
- ⚠️ **За владельцем:** Safari (архитектор не может автоматизировать) и «Универсальный
  доступ» для проверки ввода.
- ⚠️ Живой захват экрана ещё не проверялся: у терминала нет разрешения «Запись экрана».
  Владельцу при возвращении: Системные настройки → Конфиденциальность → Запись экрана →
  включить приложение терминала (или IDE), из которого запускается `cargo run`.
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
- ⚪ D3: `scap` 0.0.8 не экспортирует `get_main_display`, `BGRAFrame` без stride (принят packed), `Capturer` не `Send` (обёрнут `unsafe impl Send` с инвариантом «один поток»). Если упрёмся — переход на прямые `screencapturekit`/`windows-capture`.
- ⚪ D4: openh264 с `skip_frames(false)` не держит потолок битрейта на сложном контенте; адаптация — фаза 2.3.
- ⚪ D5: `windows-capture` запинен на =1.4.4 из-за scap 0.0.8; снять пин при переходе на прямой windows-capture 2.x (вместе с D3).
- ⚪ D6: хост не переподключается к сигнальному серверу при обрыве WS (процесс завершается) — фаза 3.5.
- ⚪ D7: захват берёт первый `Target::Display`, а координаты мыши масштабируются по `main_display()` enigo — при нескольких мониторах могут разойтись; решается в 2.4 (мультимонитор).
- ⚪ D8: ручные проверки исполнителя и архитектора делят порт 8080 и `pkill -f rcdesk-*` — исполнитель однажды убил живой стек архитектора. Правило: живые проверки архитектора — из worktree на другом порту с переименованными бинарниками.

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
