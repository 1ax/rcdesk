# Правка rcdesk в rtc-sctp 0.20.5

Копия крейта `rtc-sctp` 0.20.5 с двумя правками:

1. `src/config.rs`: `INITIAL_MTU` = 1188 (было 1228). Тест `rcdesk_patch_tests::outgoing_packets_fit_chrome_sized_mtu`.
2. `src/association/timer.rs`: `TimerConfig::default()` ставит `max_t3_rtx_retrans: NO_MAX_RETRANS`
   (было `PATH_MAX_RETRANS` = 5). Тест `rcdesk_patch_tests::t3_rtx_is_unlimited_by_default`.

## Зачем: MTU (главная причина, 2026-10-12)

Пакет SCTP 1228 байт + DTLS (~37 байт) на реальном пути Mac-хост → Windows-клиент (Chrome) отбрасывался
всегда: первый же кусок данных хоста не доходил, повторы тоже, и канал хост → клиент вставал с первых
секунд сеанса (файлы «1/6», буфер обмена не доходил). Видео (RTP ≤ 1200) и пакеты Chrome (SCTP ≤ 1188)
проходили. Проверено живьём: тестовый хост с 1188 — 6 файлов дошли, `T3-rtx` ни разу. В 0.21.1 то же 1228,
наружу (`EndpointConfig` создаётся в `rtc` через `default()`) не выставлено.

## Зачем: лимит T3-rtx

Документация поля говорит «Defaults to unlimited», но по умолчанию стояло 5. После 5 повторов
`handle_timeout` вызывает `on_retransmission_failure`, который лишь логирует
`retransmission failure: T3-rtx (DATA)` и не перезапускает таймер. Потерянный кусок данных
больше не уходит заново. Симптом: хост -> клиент по каналам данных встаёт примерно через минуту
после потери пакета. В `webrtc`/`rtc` 0.20.5 `TimerConfig` наружу не выставлен.

В 0.21.1 то же самое (проверено архитектором).

## Подключение

Корневой `Cargo.toml`: `[patch.crates-io] rtc-sctp = { path = "vendor/rtc-sctp" }`.

## Когда убрать

Когда upstream снизит MTU и снимет лимит T3-rtx или даст задать оба через SettingEngine.
Тогда удалить `vendor/rtc-sctp`, секцию `[patch.crates-io]` и `exclude = ["vendor"]`.
