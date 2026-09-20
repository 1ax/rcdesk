# rcdesk — Журнал слайсов

Хронология работы по слайсам. Обновляется в конце каждого слайса/под-шага.
Архитектурные решения — в ARCHITECTURE.md, здесь статус, итоги, долги.

---

## 📍 Голова: состояние и открытые долги (живой индекс)

> **Этот блок — состояние, не история.** Перезаписывается при каждом изменении.
> Всё, что НИЖЕ разделителя `---`, — append-only история. Архитектор при старте
> чата читает сначала голову, в тело ныряет по ссылке на слайс.

**Состояние на 2026-09-20 (седьмой чат: слайс 2.6 закрыт):**
- ✅ **Слайс 2.6 автозапуск и трей — в `main`, проверен вживую на стенде** (см. запись 2.6): `dbd3d77` ядро
  агента + переподключение (backoff 1→30 с, WS keepalive 20/45 с, `AgentStatus`); `f480468` свежие ICE-креды
  в `PeerJoined`; `ba76691` агент `rcdesk-agent` (трей/меню-бар, `tray-icon` 0.25); `aac4f3b` автозапуск
  (LaunchAgent / `HKCU\…\Run`, меню и `rcdesk-host autostart`); `fffeda6` предупреждение об окне администратора
  (UIPI); `bf57e5e` повтор ключевого кадра на статичном экране + бирка `build <id>`; `5a839a6` первый кадр не
  теряется при согласовании видео; `0db44fa` русский интерфейс. Фиксы по ходу: `3973340` (clippy Windows),
  `4d80198` (значок трея на Windows), `59aa489` (импорт `OpenProcessToken`), `fffe75e` (флаки-тест).
  Последний зелёный Deploy — `35528104924`, прод обновлён.
- **Проверка владельца (2026-09-20, Win10-стенд ↔ Mac-клиент через прод):** агент стартует без консоли, значок
  зеленеет в сеансе, PIN и «Копировать PIN» работают, автозапуск появляется в Диспетчере задач и поднимает агент
  после перелогина, плашка «Окно администратора» появляется и исчезает, интерфейс русский, картинка приходит
  за пару секунд (время установления ICE/DTLS, см. D32).
- ▶️ **Следующее: 3.1 постоянные устройства и список «мои компьютеры»** (SQLite, без PIN при каждом подключении).
  Новый чат — на слайс.
- ⚠️ Один docs-коммит может лежать локально незапушенным: по решению владельца docs-only коммиты уезжают
  со следующим кодовым push (каждый push крутит Deploy целиком).
- ✅ **2.4e монитор трафика — закрыт полностью** (`a7b5ec0`, Deploy `34889246636`, см. запись 2.4e):
  coturn отдаёт метрики на `127.0.0.1:9641`, `~/app/traffic-monitor.sh` в crontab `rcdesk` раз в 10 минут,
  уведомления в Telegram проверены сквозняком 2026-09-15 (бот `@Rcdesk_bot`, chat_id владельца).
  Боевой счётчик пошёл с реальными цифрами: `relay_total` 41.8 МБ, `iface_total` 620 МБ, `notified` пуст,
  `monitor.log` без предупреждений. **Грабли:** Telegram-бот не может написать первым — пока владелец не
  нажал «Start», любой chat_id даёт `chat not found` (400); токен при этом валиден, `getMe` отвечает.
  Попутно: 41.8 МБ релея за утро — это сеансы rcdesk Mac↔Mac, ушедшие через TURN, лишнее подтверждение D19.
- ✅ **Слайс 2.4 мультимонитор — в `main`, четыре коммита** (см. запись 2.4): `9f37676` контракт
  `Displays`/`SelectDisplay` + список дисплеев с координатами и primary (Windows — свой
  `EnumDisplayMonitors`), синтетика с двумя дисплеями; `c3d7b5a` `VideoPipeline` и переключение в живой
  сессии без пересоздания PeerConnection; `576ee71` селектор в клиенте; `6cfc491` ввод по прямоугольнику
  дисплея + `SendInput`/`VIRTUALDESK` на Windows. Локально и на стенде Win10 проверено (2026-09-14, Deploy
  `34876302689`): селектор скрыт при одном дисплее, мышь через `SendInput`+`VIRTUALDESK` попадает точно;
  задержка ~2 с до меню — видеопуть (GDI 1–2 fps на статике + релейный rtt ~200 мс, D19/D14), не ввод.
  Реальный захват на Mac после 2.4 не проверялся.
- ✅ **D25 закрыт (2026-09-14):** `name: rcdesk` в compose, три проекта на VPS: `app` (crewtally),
  `glitchtip`, `rcdesk`; попутно починен redis glitchtip (битый incr-AOF).
- (Ниже — состояние по 2.3, актуально.)
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
- ✅ **Проверено на стенде (2026-09-14, четыре сеанса Mac Chrome → Win10 GDI 1080p):** первая версия
  контроллера срезала цель по REMB до пола на пустом LAN (0.3 Мбит/с @ 5 fps) — два фикса `972e576`,
  `9d9f392` (REMB только при нагруженном канале, два тика подряд, и только когда оценка не растёт;
  перегрузка кодера — по перезаписи слота захвата). После фиксов: **MFT** — набор 8–12 fps, видео
  20–24 fps / 2–5.8 Мбит/с / jb 25–43 мс, решений нет; **openh264** — видео fps ступенями 30→8, затем
  8↔12 вокруг реальной ёмкости кодера, jb 35–60 мс и **не растёт** (в 2.2f — 13–17 fps с ростом jb до
  80+). Последний зелёный Deploy — `34838034261` (`9d9f392`).
- Локально проверено (Mac, синтетика 720p, Chrome 153): `--bitrate 300` → решение `bitrate`, fps 15,
  оверлей `target 0.3 Mbit/s @ 15 fps (bitrate)`; дефолт 6000 → 0 решений за 16 с при REMB 0.79–1.36 Мбит/с
  (поток 0.8), 116 RR. Реальный захват release-бинарником не проверен: у `target/release/rcdesk-host` нет
  разрешения «Запись экрана» (scap: «no capturable display found»).
- 2.3 на стенде проверен (см. выше); даунскейл (D20) остаётся долгом.
- Как запустить: `docs/dev-run.md` (локально), `docs/host-windows.md` (стенд), `infra/README.md` (прод).
- Окружение владельца: Apple M4 / macOS 26.6 / Chrome 153 / Safari 26.6, Rust 1.98 в `~/.cargo/bin`;
  Win10 Pro 22H2 x64 стенд (ATI Radeon HD 4600 → GDI, аппаратного H.264 нет), exe в `d:\rc\`.
- Правила процесса (в CLAUDE.md): архитектор коммитит сам после сверки, «коммит» от владельца — только
  перед живой проверкой/деплоем (решение 2026-09-13); живые проверки на стенде — только мышь и чтение
  экрана; `git add` только явными путями, пока работает исполнитель.

**За владельцем:**
- **Push `main` и живая проверка 2.6** (после зелёного Deploy, артефакт с `rcdesk-host` + `rcdesk-agent`):
  1. Стенд Win10: двойной клик `rcdesk-agent.exe` → без консоли, значок в трее (серый монитор), меню: `PIN …`,
     Copy PIN (вставить в Блокнот), Open log. Подключиться с Mac → значок зелёный, `Session active`; End session
     рвёт сеанс у клиента. Если значка нет/агент не стартует — `%LOCALAPPDATA%\rcdesk\logs\agent.log`.
  2. Там же «Start at login» → галочка; Диспетчер задач → Автозагрузка: `rcdesk`; перелогиниться → агент поднялся
     сам. Снять галочку → запись пропала.
  3. Переподключение: при следующем деплое (или перезапуске сервера) агент уходит в `Offline — retrying…` и
     возвращается с НОВЫМ PIN (ожидаемо до 3.1).
  4. Mac (необязательно, из артефакта или `cargo build`): `rcdesk-agent` в меню-баре, пункты разрешений
     `Grant … permission…` открывают нужные панели Настроек; разрешения выдаются самому бинарнику (D28).
- ✅ Mac с реальным захватом после 2.4 — проверен владельцем 2026-09-18 (см. запись 2.5a): картинка
  1920×1080 28 fps VT, плашка `View only` без «Универсального доступа»; с ним — клиент Chrome на стенде Win10
  через прод, клики по Dock Mac попадают. Остаток — второй монитор (D23).
- Необязательно: буфер обмена на **Mac-хосте** (NSPasteboard) вживую не проверен — только Windows-хост;
  проверка: хост из PyCharm, клиент Chrome на стенде Win10, копирование в обе стороны.
- Необязательно: `serve --encoder openh264 --no-adapt` на том же контенте — честное «до» для openh264
  (в 2.2f цифры снимались на другом видео и, возможно, через TURN-релей, см. D19).
- Если push `main` 2.2f ещё не сделан — сделать: на стенде обычный `serve` без флагов должен логировать
  `media foundation encoder name=H264 Encoder MFT hardware=false`.
- Развилка D14: монитор на встроенную графику Ivy Bridge (даст WGC + Quick Sync для `auto`).
- ✅ TLS 1.3 на nginx включён (2026-09-13, `/etc/nginx/conf.d/ssl.conf`, файл не принадлежит пакету).

**Открытые долги:**
- 🟡 D6 частично закрыт в 2.6a: переподключение к сигнальному серверу с backoff и keepalive есть; PIN при этом
  меняется (стабильный — с постоянными устройствами, 3.1). Переподключение WebRTC-сеанса — по-прежнему 3.5.
- ⚪ D28: **TCC на macOS сбрасывается при обновлении бинарника `rcdesk-agent`** — ad-hoc подпись, разрешения
  привязаны к cdhash; при запуске из launchd разрешения выдаются самому бинарнику. Лечится подписью с постоянным
  идентификатором и `.app`-бандлом (4.3).
- ⚪ D29: **Windows-код 2.6c/2.6d не компилировался локально** (ring/assert.h): цикл сообщений, `SetThreadExecutionState`,
  реестр `Run` сверены с исходниками `windows` 0.61.3 построчно, одна ошибка (`BOOL == 0`) поймана на сверке.
  Первая сборка — Deploy после push; живое поведение — на стенде.
  **Решение владельца (2026-09-20): локальную кросс-сборку не заводим** — не захламлять диск (`brew llvm`
  ~2,5 ГБ + кэш MSVC CRT/Windows SDK у `cargo-xwin` ~1,5 ГБ; вариант с `mingw-w64` тоже отклонён).
  Windows-код продолжаем ловить в CI, по одной ошибке за прогон; цена известна и принята. Разведка
  зафиксировала упоры: только `build.rs` крейтов с C — `ring` (нет заголовков MS CRT) и `openh264-sys2`
  (C++ + nasm при фиче `source`); чистый Rust (`windows` 0.61) чекается и без MSVC (scratch-крейт, как в 2.2).
  Вернуться к вопросу — перед 4.1, где Windows-кода будет много; тогда же альтернатива — toolchain прямо
  на стенде Win10 (там всё равно нужны живые итерации со службой).
- ⚪ D32: **чёрный экран первые 2–4 с сеанса** — это время установления ICE/DTLS, показывать в этот момент
  нечего. Лечится косметически: надпись «Подключение к хосту…» поверх видео до первого кадра (правка только
  в клиенте). Владелец решил не делать сейчас (2026-09-20).
- ⚪ D31: **окна с правами администратора (UIPI)** — в лёгком режиме ввод в них не проходит (подтверждено на стенде
  2026-09-20: Диспетчер задач, пришлось идти к машине физически). 2.6e показывает плашку в клиенте; реально лечится
  полным режимом со службой (4.1).
- ⚪ D30: **иконка агента простая** (монитор, состояния: macOS — заливка template-иконки, Windows — цвет экрана);
  на тёмной панели Windows серый может быть малозаметен — оценить на стенде.
- ✅ D26 закрыт в 2.5a (2026-09-18): без разрешения на ввод сеанс деградирует в «только просмотр»
  (`NoopInjector` + `InputStatus` клиенту), а не гаснет; при провале старта — `Bye` клиенту. Для сессий из
  терминала TCC выдаётся родительскому приложению (PyCharm), «Универсальный доступ» у него по-прежнему нет.
- ⚪ D27: **`scap` печатает `Screen capture error occurred.`** (eprintln из `StreamErrorHandler::on_error`,
  т.е. SCStream `didStopWithError`) на старте сеанса с реальным захватом на Mac (2026-09-18, хост из PyCharm,
  клиент на стенде). Картинка при этом шла без замираний; триггер не выяснен (возможно, поток от предыдущей
  сессии или сосуществование двух SCStream). Флаг ошибки scap мы не читаем. Разобрать вместе с D3/D5
  (переход с scap), если начнёт замирать картинка — раньше.
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
- ✅ D25 (закрыт 2026-09-14): crewtally.app лежала (502) с 17:20 UTC 2026-09-11 — первый деплой rcdesk
  `docker compose up -d --remove-orphans` в проекте `app` (имя каталога) удалил контейнеры crewtally, чей проект
  тоже звался `app` (лог Deploy `34626666816`). Фикс: `name: rcdesk` в compose, rcdesk пересоздан под своим
  проектом, crewtally поднята из своего каталога с теми же томами `app_pg_data`/`app_redis_data`.
  **Правило:** compose-проект на общем VPS всегда с явным `name:`; `down -v` в rcdesk не делать никогда.
- ✅ D22 закрыт в 2.4e (2026-09-14): монитор трафика на VPS (coturn `--prometheus` + `infra/traffic-monitor.sh`
  по cron, пороги 50/80/100 % от 5 ТБ релея и 90 % от 10 ТБ по `ens3`, уведомление в Telegram). Остаток:
  отсечка на 100 % (флаг «не выдавать TURN-креды» в сигнальном сервере) — фаза 3, по решению владельца
  пока только уведомление. Ограничение: coturn инкрементирует `turn_total_traffic_*` только по завершении
  сессии — трафик сессии, живой в момент рестарта coturn, не учитывается.
- ⚪ D5: `windows-capture` запинен на =1.4.4 из-за scap 0.0.8; снять пин при переходе на прямой 2.x (с D3).
- ⚪ D6: хост не переподключается к сигнальному серверу при обрыве WS — 3.5.
- ✅ D7 закрыт в 2.4 (список дисплеев, переключение, ввод по прямоугольнику дисплея).
- ⚪ D23: **мультимонитор не проверен на двух физических мониторах** (у владельца один): переключение и
  попадание мыши на втором мониторе проверены только синтетикой и unit-тестами; Windows-путь
  `MOUSEEVENTF_VIRTUALDESK` на стенде проверяется лишь как «мышь на одном мониторе не сломалась».
  На macOS `CaptureRect` — точки `CGDisplayBounds`, на Retina масштаб к пикселям захвата не нужен (CGEvent
  тоже в точках); при первом же втором мониторе проверить оба.
- ⚪ D24: при переключении дисплея кадры старого pipeline (до 4 в канале) могут уйти после IDR нового —
  декодер шлёт PLI и восстанавливается (локально 2 PLI за 3 переключения); если на стенде будет
  заметный «глитч» — гасить старый pipeline до старта нового или дренировать канал.
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
  **Вероятная причина найдена 2026-09-18:** Chrome прячет свой host-кандидат за mDNS-именем `<uuid>.local`,
  а хост его не разрешает (`ERROR rtc_ice::agent::agent_proto: mDNS Query 1 timed out for ….local`) — прямой
  LAN-пары нет, остаются srflx (хэйрпин через роутер) или релей; сеанс Win10 Chrome → Mac-хост через прод
  шёл с rtt > 100 мс. Направление фикса: режим mDNS в `SettingEngine` webrtc-rs (query) — проверить в 3.5.
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
- **Стенд №3 (MFT, `9d9f392`):** ~2 мин, набор 6–11 fps, видео 20–24 fps / 2.5–5.8 Мбит/с / jb 25–37 мс,
  **решений нет** — ложный срез из №2 закрыт.
- **Стенд №4 (openh264, `9d9f392`):** набор 6–11 fps, 0.1 Мбит/с, jb 8–15 мс без решений; с началом видео
  `encoder` ступенями 30→24→20→15→12→10→8 за первые секунды (перезаписи слота: openh264 ~100 мс на кадр
  1080p против 14–20 fps от GDI), затем 8→10→12 после чистых тиков и обратно — контроллер ходит вокруг
  реальной ёмкости кодера; видео 8–12 fps, 1–5.4 Мбит/с, **jb 35–60 мс и не растёт** (2.2f: 13–17 fps,
  jb 2→80 мс). Вывод: адаптация делает то, что должна, а openh264 на этом CPU для 1080p непригоден —
  реальный выход для такого железа даунскейл (D20); дефолт на Windows остаётся MFT.
- Гейты на закрытии (Mac): host lib **67 passed; 1 ignored**, loopback 1, proto 17, server 8 + 7,
  web `Tests 39 passed (39)`, build ✓. Первый Deploy `34819646587`: web/ubuntu/windows зелёные (на
  `windows-latest` собрался и прошёл MF `set_rate`), macOS красный из-за пейсера — см. фикс выше.
  Второй Deploy `34820127376` (`5554685`) — все три ОС, build-host и deploy зелёные; прод обновлён.

### 2.4 Мультимонитор (2026-09-14)

Пятый чат. Владелец: монитор один (Mac и стенд подключаются к нему по очереди), развилки — селектор в
панели сессии без диалога при подключении, свой `SendInput` только для `pointer_move`; «коммить все этапы
сам, подключусь на тестировании». Четыре под-шага, 2.4c и 2.4d исполнялись параллельно (непересекающиеся
файлы: host/input+platform+signaling vs web+docs).

- **Разведка:** `ScapSource`/`GdiSource` уже принимали id дисплея, но список (`DisplayInfo`) был без
  координат и primary, дефолт у scap — «первый», у GDI — primary; мышь масштабировалась по
  `enigo.main_display()` (главный дисплей от (0,0)); `enigo` на Windows шлёт `MOUSEEVENTF_ABSOLUTE` без
  `VIRTUALDESK` (в исходнике TODO) — второй монитор недостижим; keyframe-флаг рождался внутри
  `Pipeline::start`, а PLI-задача транспорта держала свой `Arc`; `OnOpen` data channel только логировался.
- **2.4a** (`9f37676`): `proto::control::{DisplayEntry, Displays, SelectDisplay}`; `DisplayInfo`
  + `x/y/width/height/primary` (единицы ввода ОС: точки на macOS через `CGDisplay::bounds()/is_main()` из
  `raw_handle` scap, без новых зависимостей; физические пиксели на Windows через свой
  `platform::windows::monitors` на `EnumDisplayMonitors`/`GetMonitorInfoW`, id = HMONITOR как u32, как у
  scap и `GdiSource`); `capture::default_display` (primary, иначе первый); синтетика `list_displays()` —
  два фейковых дисплея (1280×720 горизонтальный градиент, 1024×768 вертикальный с обратным тинтом),
  `for_display(id)`; `list-displays` печатает `id title WxH@x,y primary|-`.
- **2.4b** (`c3d7b5a`): `SessionEvent::DataChannelOpen`; `Pipeline::start_with_keyframe_flag`;
  `VideoPipeline` в сигналинге (источник, кодер, forward-задача с `oneshot`-стопом и `spawn_blocking` на
  `handle.stop()`, контроллер адаптации под новый размер); `switch_display`: перечитать список, новый
  pipeline раньше остановки старого, `Displays` в ответ всегда (и при отказе); `HostContext::{build_source(id),
  list_displays, display}`. Архитектор снял лишнее поле `forward_task` (drop tokio `JoinHandle` не абортит
  задачу) и добавил ответ `Displays` при ошибке листинга. Тест: два pipeline на синтетике, смена, keyframe
  и кадры после смены — 0.37 с.
- **2.4c** (`6cfc491`): `input::CaptureRect` (`From<&DisplayInfo>`), `to_pixel` = origin + scale,
  `InputRouter::set_capture_rect` (Arc<Mutex>, читается на каждое сообщение), вызовы в `start_session` и
  `switch_display`; `platform::windows::mouse::move_to` — `SendInput` `MOVE|ABSOLUTE|VIRTUALDESK` с
  нормализацией по `SM_{X,Y,CX,CY}VIRTUALSCREEN` (чистая `normalize` с тестами, выполняются только в CI
  Windows); `EnigoInjector::pointer_move` на Windows через него.
- **2.4d** (`576ee71`): `web/src/displays.ts` (`displayOptions`, `shouldShowPicker`, `parseDisplayId`,
  10 тестов), `<select>` в `.controls` (скрыт при < 2 дисплеях, блокируется до ответа, фокус обратно на
  видео), стиль как у статуса; `docs/dev-run.md` раздел «Мультимонитор», `docs/host-windows.md` формат
  `list-displays` и предупреждение про HMONITOR в автозапуске.
- **Гейты на `6cfc491`:** fmt чисто; clippy `Finished`; host lib **77 passed, 1 ignored**, loopback 1,
  proto 20, server lib 8, signaling 7; web **Tests 49 passed (49)**, 7 файлов, build ✓.
- **Локальная проверка** (debug, `serve --synthetic --no-input`, VT, Chrome 153): селектор
  `1: Synthetic 1 1280×720 (primary)` / `2: Synthetic 2 1024×768`; переключения 1→2→1→2 — в логе
  `starting video pipeline … switched display from=1 to=2`, в клиенте `videoWidth×Height` меняется
  < 1 с, селектор `disabled` до `Displays`, оверлей 29–31 fps / 0.6–0.8 Мбит/с / jb 8–10 мс, `loss 0`,
  два PLI за сессию (D24), `failed` нет. Скриншот второго дисплея с вертикальным градиентом снят.
- **CI/Deploy:** первый прогон упал на Windows (`BOOL` в `windows` 0.61 живёт в `windows::core`, фикс
  `4582c3e`), второй — на флаки-тесте конвейера на `macos-latest` (18 кадров вместо ≥ 20; порог ослаблен до
  12, пейсинг покрыт `pacer_*`); перезапуск `34876302689` зелёный.
- **Стенд Win10:** `list-displays` печатает HMONITOR-id с размером и primary, сессия с вводом — селектор
  скрыт, мышь попадает точно, клики владельца работают; задержка ~2 с до появления меню — видеопуть
  (GDI 1–2 fps на статичном экране + релейный rtt ~200 мс, D19/D14), не ввод (`app 1–3 ms`).
- Попутно в этом чате: бюджет трафика 5 ТБ/месяц и долг D22 (`8a02062`), план монитора трафика 2.4e
  (`095a04a`).

### 2.4e Инфра: монитор исходящего трафика (2026-09-14)

- Решения владельца: канал уведомлений — Telegram-бот (токен и chat_id в `~/app/.env`); на 100 % бюджета —
  только уведомить, отсечка выдачи TURN-кредов — фаза 3. Разведка: образ `coturn/coturn:4` на VPS —
  4.18.0 с `--prometheus`; на VPS нет `jq` (формат состояния — плоский `key=value`), нет `sendmail`/`msmtp`
  (почта отпала), `flock`/`curl`/`awk` есть, crontab `rcdesk` был пуст.
- **`a7b5ec0`** (executor, сверка чистая): coturn `--prometheus --prometheus-address=127.0.0.1`;
  `infra/traffic-monitor.sh` (bash, `flock -n`, метрики по `curl` + awk-сумма
  `^turn_total_traffic(_peer)?_sentb` с labels/экспонентой, `tx_bytes` ens3, состояние
  `~/app/traffic/<YYYY-MM>.state` через tmp+`mv`, дельта = `current - last` либо `current` при сбросе
  счётчика, пороги relay 50/80/100 % и iface 90 %, метка порога только при успешной отправке, токен из
  `.env` через grep/cut, а не `source`); `infra/traffic-monitor.test.sh` — 9 сценариев на `file://`-моке
  метрик; `.env.prod.example` + две переменные; deploy копирует скрипт в `~/app/` и делает `chmod +x`; CI —
  job `infra-shell` (`shellcheck infra/*.sh` + тесты); README раздел «Монитор трафика»; ARCHITECTURE §11.
  Архитектор поправил README: проверка Telegram — только на копии state (в пустом каталоге первый запуск
  даёт итог 0 и порог не срабатывает). Исполнитель поставил `flock` через brew на Mac (системная утилита
  для локального прогона тестов, не зависимость репо).
- **Гейты:** `shellcheck infra/*.sh` — пусто, exit 0; `bash infra/traffic-monitor.test.sh` → `ok 9`.
  Rust/web не затронуты. Deploy `34889246636` зелёный на всех джобах, включая новый `infra-shell`.
- **Прод (архитектор по одобрению владельца):** после деплоя `127.0.0.1:9641/metrics` отвечает, в `# HELP`
  есть `turn_total_traffic_sentb` и `turn_total_traffic_peer_sentb` (строки со значениями появятся после
  первой завершённой сессии — до этого сумма 0). Поставлен crontab
  `*/10 * * * * $HOME/app/traffic-monitor.sh >> $HOME/app/traffic/monitor.log 2>&1`; два ручных прогона:
  `relay_total=0`, `iface_last=5806768702` → второй прогон `iface_total=450`, exit 0.
- **Уведомления проверены сквозняком (2026-09-15):** токен и chat_id владельца в `~/app/.env`, тест на копии
  состояния в `/tmp/t` с `TRAFFIC_IFACE_BUDGET=1` → `traffic-monitor: notified: …`, сообщение пришло в
  Telegram; боевое состояние и метки порогов не затронуты, временный каталог удалён. Первая попытка дала
  `curl: (22) … 400` — Telegram отвечает `chat not found`, пока пользователь не начал диалог с ботом
  (`getUpdates` был пуст). Токен при этом исправен, проверяется через `getMe`. Диагностика делается
  `curl` без `-f`: боевой скрипт прячет тело ответа, а описание ошибки именно там.

### 2.5 Буфер обмена (2026-09-18)

#### 2.5a Режим просмотра без разрешения на ввод, D26

- Решения владельца (по рекомендациям): D26 отдельным под-шагом перед буфером обмена; при провале старта
  сессии — `Bye` клиенту (поле `reason` в `Bye` не вводили: правка сигнального контракта и сервера);
  `--no-input` идёт тем же путём и тоже показывает `View only`; повторно поднимать ввод посреди сессии не
  делаем — инжектор создаётся на каждую сессию, после выдачи разрешения хватает переподключения
  (enigo 0.6.1 проверяет `AXIsProcessTrusted` при каждом `Enigo::new`).
- **`509cb6b`** (executor, сверка чистая): `ControlMessage::InputStatus { available, reason }` (host → client,
  при открытии `control` сразу после `Displays`, `reason: None` → `null`); `start_session` при ошибке
  `build_injector` берёт `NoopInjector` и хранит статус в `ActiveSession`; `EnigoInjector::new` различает
  `NewConError::NoPermission` (текст с путём в Системных настройках; вариант объявлен в enigo без cfg);
  `--no-input` — замыкание с `Err("disabled by --no-input")`; `Bye` при провале `start_session` и
  `create_offer`. Клиент: `inputStatus.ts` (`viewOnlyLabel`), флаг `inputBlocked` в `app.ts` (ввод не
  цепляется, уже прицепленный снимается), плашка `#view-only` в панели.
- **Гейты:** `cargo fmt --all --check` чисто; clippy `-D warnings` чисто; `cargo test --workspace` —
  host `80 passed; 0 failed; 1 ignored` (было 77), proto `22 passed` (было 20), server `8` + `7`, loopback `1`;
  web — `Test Files 8 passed (8)`, `Tests 52 passed (52)` (было 49), build `✓ built`.
- **Живая проверка (архитектор, локально, Chrome 153):** `serve --synthetic` БЕЗ `--no-input` debug-бинарником
  без «Универсального доступа» — в логе `WARN input unavailable, session is view-only error=no Accessibility
  permission (…)`, сеанс `connected`, видео 1280×720 24 fps, плашка `View only: no Accessibility permission
  (System Settings → Privacy & Security → Accessibility)` в панели. До фикса этот запуск давал чёрный экран.
- **Живая проверка владельца (2026-09-18, реальный захват, хост из терминала PyCharm):** без «Универсального
  доступа» — картинка 1920×1080 28 fps VT + плашка `View only`; после выдачи PyCharm разрешения плашки нет.
  Проверка мыши на одном Mac невозможна: хост двигает тот же системный курсор, и он «выбрасывается» из
  уменьшенной картинки (петля). Мышь проверена с клиента Chrome на стенде Win10 через прод: клики по Dock
  Mac попадают. Попутно: rtt > 100 мс и mDNS-таймаут на кандидат Chrome (→ D19), `Screen capture error
  occurred.` от scap без замирания картинки (→ D27).

#### 2.5b Протокол и хост

- Решения владельца (по рекомендациям): клиент→хост по `input` (порядок с Cmd+V гарантирован), хост→клиент по
  `control`; изменения — опрос счётчика ОС раз в 250 мс (`NSPasteboard.changeCount` / `GetClipboardSequenceNumber`),
  текст через `arboard` 3.6 (`default-features = false`); лимит 200 000 байт UTF-8 в обе стороны; по умолчанию
  включено, `--no-clipboard`; в view-only буфер работает. Стартовое содержимое буфера хоста клиенту не уходит.
- **`536318a`** (executor + две правки архитектора при сверке): модуль `host/src/clipboard` по образцу `cursor`
  (`ClipboardBackend`, чистая `ClipboardSync` с защитой от эха, поток-наблюдатель); `ClipboardText` с `input`
  применяется синхронно в `handle_session_event` до следующей клавиши. Содержимое буфера в логи не попадает —
  только длина (явные ветки до `trace!(?other)`). Правки архитектора: `apply_remote` не запоминает текст при
  ошибке записи (иначе повтор отбрасывался как дубль); импорт `ClipboardBackend` в `main.rs` за `cfg` (иначе
  clippy на Ubuntu). На macOS `arboard` безусловно тянет `objc2-core-graphics`/`objc2-io-surface`, на Windows —
  `clipboard-win` 5 и вторую копию `windows-sys` (0.52).
- **Гейты:** host `91 passed; 0 failed; 2 ignored` (было 80), proto `24 passed` (было 22); web без изменений
  `52 passed`. Кросс-проверка Windows локально невозможна (`ring` не находит `assert.h`, так же на чистом `main`) —
  Windows проверил Deploy.

#### 2.5c Клиент

- Решение владельца: буфер клиента уходит только на Cmd/Ctrl+V и по `clipboardchange` (Chrome); при подключении
  и возврате фокуса — нет.
- **`450da18`** (executor, сверка чистая): `clipboard.ts` — `ClipboardBridge` на внедряемых зависимостях (запись от
  хоста: сразу / отложенно до фокуса или жеста / в `ClipboardItem`-промис в Safari на Ctrl/Cmd+C/X с таймаутом
  3 с; чтение перед вставкой с таймаутом 10 с), `isSafari` по UA; `input.ts` — `KeyMessageGate`: на Cmd/Ctrl+V
  клавиши (и `release_all`) копятся, пока буфер не уйдёт. Заметка `Clipboard too large to sync (N KB)` в панели.
- **Гейты:** web `Test Files 9 passed (9)`, `Tests 90 passed (90)` (было 52); Rust без изменений.

#### 2.5d Живая проверка (владелец, 2026-09-18, Deploy `35380995575`)

- Win10-хост (exe из артефакта) ↔ Mac-клиент через прод: Chrome — Windows→Mac (Ctrl+C в Блокноте → Cmd+V в
  Заметках) и Mac→Windows (Ctrl+V после разрешения на чтение буфера) работают; Safari — оба направления
  работают, Mac→Windows требует нажать системную кнопку «Вставить» (ограничение Safari, §10; обойти через
  нативный `paste` нельзя — с Windows-хостом вставка идёт по Control+V, для macOS это не команда вставки).
- С Windows-хостом копировать/вставлять Control, не Cmd: Cmd уходит клавишей Win.
- Не проверено: Mac-хост (NSPasteboard) — в «За владельцем».

### 2.6 Автозапуск и трей/меню-бар (2026-09-18)

- Разведка: `serve` одноразовый (сборка в `main.rs::run_serve`, при обрыве WS — выход, D6); PIN живёт, пока
  хост на связи (`server/src/registry.rs`), новый — на каждой регистрации; ICE/TURN-креды выдаются один раз в
  `Registered` при TTL 24 ч (`RCDESK_TURN_TTL_SECS=86400`) — хост, живущий дольше суток, теряет релей; имя
  хоста из `HOSTNAME` (нет в Windows и под launchd); `rcdesk-host.exe` — консольный.
- **Решения владельца (все по рекомендациям, «go» на все под-шаги, коммиты архитектору, push и проверка —
  владелец):** трей на `tray-icon` 0.25 (+`muda`) без Linux-фич; отдельный бинарник `rcdesk-agent` с
  `windows_subsystem = "windows"`, `rcdesk-host` остаётся CLI; свежие ICE-креды на каждый сеанс сейчас;
  `.app`-бандл и подпись macOS — в 4.3 (долг: TCC сбрасывается при обновлении бинарника); смена PIN при
  переподключении допустима до 3.1; автозапуск по умолчанию выключен.
- План: **2.6a** ядро агента в библиотеке (`rcdesk_host::app`) + переподключение с backoff 1→30 с + статус
  наружу + имя компьютера; **2.6b** `PeerJoined.ice_servers` (свежие креды на сеанс); **2.6c** бинарник
  `rcdesk-agent`: трей/меню-бар (статус, PIN, «Скопировать PIN», «Завершить сеанс», «Открыть лог», «Выход»),
  лог в файл, один экземпляр, дефолт `wss://rcdesk.app/ws`; **2.6d** автозапуск (LaunchAgent / `HKCU\…\Run`),
  пункт меню и `rcdesk-agent autostart on|off|status`, артефакты CI, docs.

#### 2.6a Ядро агента и переподключение

- **`dbd3d77`** (executor + фикс-промпт + правка архитектора): `rcdesk_host::app` — `ServeOptions`,
  `build_host_context`, бэкенды и сборщики из `main.rs`; `run_agent` — бесконечный цикл connect → `Registered` →
  `run` с backoff 1→30 с (сброс после регистрации); `AgentStatus` (`Connecting`/`Registered`/`InSession`/`Reconnecting`)
  через `watch`; `InSession` по `RTCPeerConnectionState::Connected`. `HostContext.session.ice_servers` — только
  `--stun`, креды регистрации мёржатся на сеанс (`session_ice_servers`). Имя хоста: `--name` → имя компьютера
  (`NSHost.localizedName`, `COMPUTERNAME`) → `HOSTNAME` → `rcdesk-host`; фичи `NSHost` + `NSString` у objc2-foundation
  (без `NSString` тип `localizedName` не существует — принято). Сверка нашла отсутствие keepalive (ни хост, ни сервер
  не пинговали — «тихий» обрыв висел бы вечно) → фикс: ping 20 с, тишина 45 с → обрыв; `serve` печатает `PIN:` только
  при смене. Архитектор: ожидание writer-задачи после обрыва ограничено 2 с (close в мёртвый сокет мог висеть).
- Гейты: host `98 passed; 0 failed; 2 ignored` (было 91), остальное без изменений. Живая проверка локально: убийство
  сервера → `retry_in=1s,2s,4s,8s` → новый PIN после подъёма.

#### 2.6b Свежие ICE-креды на сеанс

- **`f480468`** (executor, сверка чистая): `PeerJoined { session_id, #[serde(default)] ice_servers }`, сервер кладёт
  `ice.ice_servers(now)` (как в `Joined`), хост берёт их, пустые → креды регистрации (`peer_ice_servers`). Старые хосты
  с новым сервером совместимы (в proto нет `deny_unknown_fields`). Гейты: host `100`, proto `26`, web `90 passed` без
  изменений, generated TS — одно поле.

#### 2.6c Агент в трее

- **`ba76691`** (executor + правки архитектора): бинарник `rcdesk-agent` (`windows_subsystem = "windows"`), `default-run
  = "rcdesk-host"` (иначе `cargo run -p rcdesk-host` в CI и docs сломался бы). Свой цикл событий без winit
  (`NSApplication` Accessory + `nextEventMatchingMask` / `MsgWaitForMultipleObjects` + `PeekMessageW`, 100 мс), tokio —
  отдельный runtime. Модуль `agent`: `menu_model`, `render_icon`, `probe_permissions` (`scap::has_permission` +
  `EnigoInjector::new`), пути, лог в файл с ротацией 5 МБ и panic hook, один экземпляр через `File::try_lock`.
  `AgentCommand::EndSession` → `Bye`. На время сеанса — `NSProcessInfo` activity (App Nap) / `SetThreadExecutionState`.
  Новые крейты: `tray-icon`, `muda`, `png` и их зависимости; новый дубль — только `miniz_oxide` 0.8/0.9.
  Правки архитектора: `PeekMessageW` возвращает структуру `BOOL` — `== 0` не скомпилировался бы на Windows →
  `.as_bool()`; иконка 44 px на macOS (tray-icon рисует 18 pt — 22 px мылились бы на Retina) и 32 px на Windows;
  фильтр лога `info,enigo=warn` (enigo писал info-строку на каждый опрос разрешений, ~8600 строк/сутки).
- Гейты: host `119 passed; 0 failed; 2 ignored`. Живая проверка (Mac): регистрация и PIN в `agent.log`, второй
  экземпляр выходит с кодом 0, переподключение; иконка-монитор видна в меню-баре и исчезает вместе с процессом
  (скриншот архитектора).

#### 2.6d Автозапуск

- **`aac4f3b`** (executor, сверка чистая): `agent::autostart` (plist-XML с экранированием и round-trip разбором,
  значение `Run` в кавычках, сравнение путей без учёта регистра на Windows, снятие `\\?\`), `platform::autostart`
  macOS/Windows/other. LaunchAgent `app.rcdesk.agent` (`RunAtLoad`, `KeepAlive.SuccessfulExit=false`,
  `LimitLoadToSessionType=Aqua`), `launchctl` не вызывается (bootstrap дал бы второй экземпляр, bootout убил бы агент
  из его же меню). Windows: `RegOpenKeyExW` вместо `RegCreateKeyExW` (последний требует фичу `Win32_Security`; ключ
  `Run` всегда существует). Меню «Start at login» (muda сам переключает галочку — проверено по исходникам, затем
  `set_checked(факт)`), `rcdesk-host autostart on|off|status`. Deploy: оба бинарника в артефакте, дымовая
  `autostart status`.
- Гейты: host `133 passed; 0 failed; 2 ignored`, loopback `1`, proto `26`, server `8` + `7`; web не затронут.
  Живая проверка (Mac, CLI): off → on (plist, `plutil -lint` OK) → off, plist удалён.

#### 2.6 Живая проверка на стенде и доводка (2026-09-20)

- Deploy `35454931984` зелёный на всех джобах (перед ним `35454697310` упал: clippy на windows-latest отклонил
  `chunks_exact(2)` с константой в чтении реестра — `3973340`). Windows-сборка агента прошла впервые; дымовая
  проверка на windows-latest: `rcdesk-host 0.1.0`, `autostart: off`.
- **Проверка владельца (Win10-стенд, агент из артефакта):** агент стартует без консоли, значок в трее есть, PIN
  виден, сеанс с Mac устанавливается, автозапуск в Диспетчере задач появился. Две находки:
  1. **Значок не зеленел в сеансе.** Причина: `TrayIcon::set_icon_with_as_template` в `tray-icon` 0.25 — macOS-only,
     на Windows молча возвращает `Ok(())`. Фикс `4d80198`: на Windows `set_icon`, ошибки в лог.
  2. **«Мышь двигается, клики не срабатывают».** Диагноз (архитектор): **UIPI** — клики в Диспетчер задач (высокий
     уровень целостности) отбрасываются, `SendInput` при этом возвращает успех, в логе хоста пусто; курсор двигается,
     потому что он общий для системы. Ввод исправен, это ограничение лёгкого режима (ARCHITECTURE §8), не дефект 2.6.
     Проверены и исключены: шлюз буфера обмена в клиенте (клики уходят мимо очереди), устаревшая позиция (координаты
     едут в самом сообщении о клике, хост ставит курсор перед нажатием).
- Deploy: `35497933583` упал (Windows: `OpenProcessToken` импортирован из `Security`, а в крейте `windows` он
  в `System::Threading` — там же, где `OpenProcess`; в `Security` только типы токена), фикс `59aa489` →
  `35498082898` зелёный, прод обновлён. **Урок:** Windows-ветки ловят по одной опечатке за прогон CI, пока нет
  локальной кросс-сборки (ring/assert.h) — оценить, чинится ли она, перед следующим Windows-слайсом.
- **Флаки-тест (`fffe75e`):** docs-коммит `73eae91` упал на macOS в `run_agent_reconnects_after_the_server_drops_the_connection`
  («timed out waiting for the first registration»). Причина — не продукт: `watch` хранит только последнее значение,
  и на загруженном раннере хост проскакивал `Registered{111111}` → обрыв → `Reconnecting` → `Registered{222222}`
  до первого чтения канала тестом. Фикс: фейковый сервер рвёт первое соединение только по сигналу теста (oneshot),
  промежуточный `Reconnecting` в этом тесте не утверждается (его детерминированно покрывает тест про «тихое»
  соединение с паузой 1 с). Пять прогонов подряд зелёные; Deploy `35500106875` зелёный, прод обновлён.
  **Решение владельца (2026-09-20):** docs-only коммиты отдельно не пушить — уезжают со следующим кодовым push
  (каждый push крутит Deploy целиком).
- **`fffeda6` (2.6e, решение владельца — вариант «предупреждать клиента»):** `ControlMessage::InputBlocked
  { blocked, reason }` (отдельно от `InputStatus`: клиент НЕ отцепляет ввод, иначе из окна администратора не выбраться);
  `platform::windows::elevation::foreground_input_blocked()` — уровень целостности активного окна против своего
  (`GetTokenInformation(TokenIntegrityLevel)`, отказ доступа = «выше»); наблюдатель раз в 1 с в сессии с доступным
  вводом, сообщение только при смене состояния (`InputBlockedWatcher`, тесты на любой ОС); клиент — плашка
  `Admin window: input is blocked by Windows`. Гейты: host `137 passed; 0 failed; 2 ignored`, proto `28`, web
  `Test Files 10 passed (10)`, `Tests 94 passed (94)`.

#### 2.6 Вторая проверка на стенде (2026-09-20)

- ✅ Значок трея зеленеет в сеансе (фикс `4d80198` подтверждён вживую).
- ✅ **Хост распознаёт окно с повышенными правами**: в `agent.log` `foreground window elevation state changed
  blocked=true` (08:44:41) и `blocked=false` (08:51:06), предупреждений об отправке нет — `InputBlocked` ушёл.
  **Плашки владелец не увидел, потому что вкладка клиента была открыта с 07:12, а прод обновился в 08:41:54:**
  в открытой вкладке работал старый бандл без обработчика. Выложенный бандл проверен архитектором — строка
  `Admin window` в нём есть. Вывод: перед проверкой клиента — жёсткая перезагрузка страницы; и нужен видимый
  идентификатор сборки (делается в 2.6f).
- ⛔ **Чёрный экран в начале сеанса** (повторилось дважды): источники отдают кадр только при изменении картинки
  (GDI — `frame_unchanged`, SCK — по природе), конвейер стартует до установления соединения, первые кадры уходят
  в никуда, а на статичном рабочем столе новых кадров нет. `request_keyframe` при connect и PLI от браузера лишь
  поднимают флаг — кодировать нечего. Картинка появляется, когда на хосте что-то изменится (или когда Chrome
  перерисовывает вкладку). Фикс — 2.6f: повтор последнего кадра как ключевого.
- Учётная запись на стенде — администратор (значит Диспетчер задач идёт с высоким уровнем целостности, как и
  предполагалось в 2.6e).

#### 2.6f Чёрный экран на статичном экране и бирка сборки (2026-09-20)

- **`bf57e5e`** (executor, сверка чистая): encode-поток конвейера хранит последний `RawFrame` (без клонирования —
  `encoder.encode(&raw)` берёт ссылку, кадр остаётся у нас) и на таймауте ожидания слота (`SLOT_WAIT_TIMEOUT`
  100 мс) при взведённом `request_keyframe` перештамповывает его текущим временем и кодирует как ключевой;
  флаг снимается тем же `swap`, при занятом канале (`tx.capacity() == 0`) остаётся взведённым; счётчик
  `stats.keyframe_repeats` и `debug!`. Ожидание переписано на `SlotWait::{Frame,Timeout,Stopped}` —
  `wait_timeout` теперь различает «кадр пришёл» и «истёк таймаут». `RawFrame::set_ts`. Тесты: источник отдаёт
  один кадр и замолкает — с `request_keyframe()` повтор приходит за ≤ 2 с, без запроса повторов нет.
- Клиент: бирка `build <id>` (`__BUILD_ID__` из `GITHUB_SHA` через `define` в vite.config, `dev` локально),
  один элемент вне переключаемых экранов. В проде проверено: в бандле `textContent=re(`bf57e5e`)` — хеш
  сходится с запушенным коммитом, правок в workflow не потребовалось.
- **Гейты:** host `140 passed; 0 failed; 2 ignored` (было 137), proto `28`, server `8` + `7`, loopback `1`;
  web `Test Files 11 passed (11)`, `Tests 96 passed (96)` (было 10/94). Deploy `35501516006` зелёный, прод обновлён.
- **Ждёт проверки владельца:** подключение при неподвижном рабочем столе даёт картинку сразу; плашка
  `Admin window` при Диспетчере задач — ПОСЛЕ жёсткой перезагрузки клиента (Cmd+Shift+R, сверить `build`).

#### 2.6g Чёрный экран: диагностика живой сессии и фикс (2026-09-20)

- 2.6f не помог: владелец предложил смотреть самому, архитектор открыл вкладку в Chrome владельца и снял
  статистику изнутри страницы (PIN со стенда). **Что увидели:** соединение `connected`, каналы данных живые
  (rtt ~300 мс), но `inbound-rtp` видео отсутствует — за 16 с ноль пакетов; затем поток оживает, и за 3 с
  приходит 15 кадров, из них **все 15 ключевые при 15 PLI** — то есть картинка идёт только в ответ на запросы
  браузера, обычных кадров нет вовсе (рабочий стол статичен, курсор в захват не входит).
- **Причина (`5a839a6`):** в `PeerSession::start_video` кадр, пришедший до готовности SSRC/payload type
  (DTLS/SRTP ещё поднимается), отбрасывался молча. Первый и единственный кадр попадал ровно в это окно, после
  чего задача парковалась на `frames.recv()` — на статичном экране следующего кадра нет никогда. Картинка
  появлялась только по таймеру PLI браузера (15–17 с). Фикс: при таком сбросе заново взводить
  `request_keyframe` (не чаще раза в 250 мс, `KEYFRAME_REARM_INTERVAL`), повтор из 2.6f отвечает за ~100 мс.
- Урок: диагностика живой сессии из вкладки владельца (`getStats` + патч `RTCPeerConnection`) дала ответ за
  десять минут там, где две итерации «фикс → артефакт → проверка на стенде» промахнулись.

#### 2.6h Русский интерфейс (2026-09-20)

- Решение владельца: весь пользовательский интерфейс по-русски (клиент и меню агента); логи, CLI и `--help`
  остаются английскими.
- **`0db44fa`** (executor + доводка архитектора): клиент — кнопки, статусы (`data-status` не тронут, переведены
  только подписи), плашки «Только просмотр: …» и «Окно администратора: Windows не пропускает ввод», заметка про
  буфер, `(основной)` в селекторе, причины адаптации в оверлее; `lang="ru"`. Меню агента — все пункты и подписи
  разрешений macOS. Причины, уходящие клиенту (`ELEVATED_INPUT_BLOCKED_REASON`, `InputStatus` из `enigo.rs`
  и `--no-input`). Бирка `build <id>` и технические сокращения оверлея (`fps`, `rtt`, `jb`) не переводятся.
  Доводка архитектора: ошибки сигнального сервера (`unknown pin`, `host busy`, …) остаются стабильными строками
  протокола, клиент переводит их таблицей `SIGNAL_ERROR_LABELS` (неизвестный код показывается как есть) — сервер
  не трогали. Остаток: `display.title` приходит от ОС (`\\.\DISPLAY1` на Windows) и не переводится.
- **Гейты:** host `140 passed; 0 failed; 2 ignored`, proto `28`, server `8` + `7`, loopback `1`;
  web `Test Files 11 passed (11)`, `Tests 98 passed (98)` (было 96 — плюс два теста на перевод ошибок сервера).

#### 2.6 Итог слайса (2026-09-20)

Восемь под-шагов (2.6a–2.6h) и четыре фикса по ходу живых проверок. Лёгкий режим стал продуктом: агент живёт
в трее/меню-баре, переживает обрывы связи и деплои сервера, запускается при входе, честно говорит про окна
администратора и разрешения, интерфейс по-русски. Проверено владельцем на стенде Win10 ↔ Mac через прод.

**Чему научил слайс:**
- Windows-код ловится только в CI, по одной ошибке за прогон (три раза за слайс): локальная кросс-сборка
  (`cargo-xwin` или контейнер) окупится уже на 4.1, где Windows-кода будет много.
- Живой сеанс быстрее разбирать изнутри вкладки владельца (`getStats`), чем по логам хоста — см. 2.6g.
- Вкладка клиента переживает деплой и молча работает на старом коде; отсюда бирка `build <id>`.
- Правки API чужих крейтов сверять по исходникам в `~/.cargo/registry`, а не по памяти: `set_icon_with_as_template`
  оказался macOS-only заглушкой на Windows, `OpenProcessToken` лежит не в том модуле, где ожидалось.
