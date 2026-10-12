# Правка rcdesk в rtc-sctp 0.20.5

Копия крейта `rtc-sctp` 0.20.5 с одной правкой в `src/association/timer.rs`:
`TimerConfig::default()` ставит `max_t3_rtx_retrans: NO_MAX_RETRANS` (было `PATH_MAX_RETRANS` = 5).
Рядом добавлен юнит-тест `rcdesk_patch_tests::t3_rtx_is_unlimited_by_default`.

## Зачем

Документация поля говорит «Defaults to unlimited», но по умолчанию стояло 5. После 5 повторов
`handle_timeout` вызывает `on_retransmission_failure`, который лишь логирует
`retransmission failure: T3-rtx (DATA)` и не перезапускает таймер. Потерянный кусок данных
больше не уходит заново. Симптом: хост -> клиент по каналам данных встаёт примерно через минуту
после потери пакета. В `webrtc`/`rtc` 0.20.5 `TimerConfig` наружу не выставлен.

В 0.21.1 то же самое (проверено архитектором).

## Подключение

Корневой `Cargo.toml`: `[patch.crates-io] rtc-sctp = { path = "vendor/rtc-sctp" }`.

## Когда убрать

Когда это исправят в upstream или когда `TimerConfig` можно будет задать через SettingEngine.
Тогда удалить `vendor/rtc-sctp`, секцию `[patch.crates-io]` и `exclude = ["vendor"]`.
