# Хост на Windows 10 (тестовый стенд)

## Требования

Windows 10 версии 1903 (сборка 18362) или новее, x64 (захват — Windows
Graphics Capture, ввод — SendInput). Права администратора не нужны.

## Где взять бинарник

GitHub → репозиторий `1ax/rcdesk` → Actions → workflow «Deploy» → последний
зелёный запуск на `main` → раздел Artifacts → `rcdesk-host-windows-x64` (zip,
внутри `rcdesk-host.exe`). Нужен вход в GitHub. Распаковать, например, в
`C:\rcdesk\`.

## SmartScreen

exe не подписан; при первом запуске из проводника Windows покажет «Windows
защитила ваш компьютер» → «Подробнее» → «Выполнить в любом случае». При
запуске из консоли предупреждения обычно нет.

## Запуск

Всё из консоли (cmd или PowerShell), из каталога с exe.

```
rcdesk-host.exe --version
```

```
rcdesk-host.exe list-displays
```

Список мониторов (id и имя).

```
rcdesk-host.exe bench --seconds 5
```

Захват+кодирование без сети; напечатает `captured/encoded/dropped/keyframes`,
`avg_fps`, `avg_bitrate_kbps`. Эти цифры нужны для журнала.

```
rcdesk-host.exe bench --seconds 5 --capture gdi
```

То же самое, но принудительно через GDI-бэкенд (см. «Бэкенды захвата» ниже) —
замер на железе/ВМ, где WGC недоступен.

```
rcdesk-host.exe bench --seconds 5 --encoder mediafoundation
rcdesk-host.exe bench --seconds 5 --encoder openh264
rcdesk-host.exe bench --seconds 5 --max-qp 30
```

Сравнение кодеров (см. «Кодеры» ниже). Первая строка вывода — `encoder=…`,
последняя — `avg_capture_to_encoded_ms`/`p95_…` (задержка от захвата кадра до
готового H.264). Отдельно печатаются средние размеры ключевых и дельта-кадров
(`avg_keyframe_bytes`/`avg_delta_bytes`).

```
rcdesk-host.exe serve --server wss://rcdesk.app/ws --no-input
```

Первый сеанс без ввода: напечатает `PIN: NNNNNN`, дальше открыть
https://rcdesk.app в браузере на другом устройстве, ввести PIN.

```
rcdesk-host.exe serve --server wss://rcdesk.app/ws
```

Полный режим с мышью и клавиатурой.

**⚠️ Подключившийся клиент реально управляет этим ПК** — как только сессия
установлена, движения курсора, клики и нажатия клавиш в браузере клиента
реально выполняются на машине, где запущен `rcdesk-host.exe`.

Подробные логи:

PowerShell:

```
$env:RUST_LOG="debug"; .\rcdesk-host.exe serve ...
```

cmd:

```
set RUST_LOG=debug
rcdesk-host.exe serve ...
```

## Бэкенды захвата

`--capture auto|wgc|gdi` у `bench` и `serve` (по умолчанию `auto`):

- **WGC** (Windows Graphics Capture, через `scap`) — нужен видеодрайвер с
  поддержкой Direct3D 11 (feature level ≥ 11_0). `auto` выбирает его, когда
  пробное создание Direct3D-устройства (`platform::windows::d3d::wgc_supported`)
  это подтверждает.
- **GDI** (`BitBlt`) — работает на любом железе и в ВМ, включая легаси-драйверы
  ниже Direct3D 11 (обнаружено на тестовом стенде: ATI Radeon HD 4600, только
  Direct3D 10.1, драйвер WDDM 1.1 — с этим драйвером `windows-capture` 1.4.4
  падает с паникой, поэтому `auto` заранее проверяет уровень поддержки и уходит
  на GDI сам). Выше нагрузка на CPU, курсор не попадает в кадр так же, как и у
  WGC, без «жёлтой рамки» (её рисует только WGC).

`--capture wgc`/`--capture gdi` форсируют бэкенд вне зависимости от того, что
показала проверка.

## Кодеры

`--encoder auto|openh264|videotoolbox|mediafoundation` у `bench` и `serve`
(по умолчанию `auto`):

- **mediafoundation** — H.264 MFT Media Foundation; **это и есть `auto` на
  Windows**: аппаратный MFT (Quick Sync, NVENC, AMF), если найден, иначе
  программный «H264 Encoder MFT» Microsoft. На стенде (ATI Radeon HD 4600,
  аппаратного MFT нет) программный Microsoft обходит openh264 по всем
  измерениям: `bench` захват→кодер 27 мс против 67–78, под видео в сеансе
  20–23 fps против 13–17 при устойчивом jitter-буфере 27–41 мс, текст при
  наборе чёткий. В логе: `media foundation encoder name=… hardware=false`.
- **openh264** — программный запасной кодер, есть везде; на Windows берётся,
  только если MFT не создался, или по явному `--encoder openh264`.
- **videotoolbox** — только macOS, на Windows отвечает ошибкой «not available».

`--max-qp N` (0..=51) — потолок QP кодера: ограничивает худшее качество кадра,
битрейт при этом может превышать целевой. Без флага у openh264 потолок 30
(без него дельта-кадры идут при QP > 34 и текст при наборе размыт), у
Media Foundation и VideoToolbox потолка нет. Замер на стенде для openh264:
без потолка под видео 0.4–2.1 Мбит/с и мягкий текст; с потолком 30 текст
чёткий, под видео 2.1–3.5 Мбит/с, но jitter-буфер растёт до 80 мс за
полминуты — кодер на этом CPU не успевает за 1080p, адаптация — слайс 2.3.

## Брандмауэр Windows

При первом сеансе появится запрос «Брандмауэр Защитника Windows заблокировал
некоторые функции этого приложения» — разрешить для частных сетей (WebRTC
слушает UDP на случайном порту для ICE). Без разрешения соединение может
пойти только через TURN или не установиться.

## Что известно и не считается ошибкой

- Жёлтая рамка вокруг экрана на время сеанса при бэкенде **WGC**: её рисует
  Windows Graphics Capture; на Windows 10 отключить нельзя (API
  `IsBorderRequired` появился в сборке 20348, то есть в Windows 11), к тому же
  scap 0.0.8 не даёт настроить. Долг в SLICES_LOG. При бэкенде **GDI** рамки
  нет вовсе (см. «Бэкенды захвата»).
- Курсор в видео не попадает — его форму хост шлёт отдельно, браузер рисует
  локально.
- Захватывается первый монитор из списка (мультимонитор — слайс 2.4).

## Что смотреть при проблемах

- `serve` завершился сразу с ошибкой про WebSocket/TLS → проверить доступ к
  https://rcdesk.app из браузера на этом же ПК.
- В браузере «connecting» дольше 10 с → брандмауэр.
- Видео есть, ввод не работает → запущено с `--no-input`.
- В логах `rtc_dtls` WARN «Unsupported Extension» и mDNS-таймауты — шум, не
  ошибка (долг D12).

## Остановить

Ctrl+C в консоли.
