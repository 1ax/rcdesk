# Правка rcdesk в scap 0.0.8

Копия крейта `scap` 0.0.8 (из crates.io) с одной правкой:

1. `src/capturer/mod.rs`: добавлен `Capturer::get_next_frame_timeout(timeout)` — как `get_next_frame`,
   но возвращает `RecvTimeoutError::Timeout` по истечении таймаута. Кадры, которые движок отбрасывает
   (`process_channel_item` -> `None`), дедлайн не продлевают.

## Зачем

Без таймаута `rx.recv()` в ScreenCaptureKit/WGC не разбудить: `tx` живёт внутри стрима до drop, а drop
происходит только после выхода потока захвата. ScreenCaptureKit отдаёт кадры лишь при изменении экрана,
поэтому на статичном экране `PipelineHandle::stop()` висел до изменения экрана, утекал поток
`spawn_blocking` и живой SCK-стрим (аудит 2026-10-13). Теперь `ScapSource` вызывает метод с таймаутом
250 мс и между ожиданиями проверяет флаг остановки.

## Подключение

Корневой `Cargo.toml`: `[patch.crates-io] scap = { path = "vendor/scap" }`.

## Когда убрать

Когда upstream даст таймаут или неблокирующее чтение кадра. Тогда удалить `vendor/scap` и строку
`scap` из `[patch.crates-io]` (`exclude = ["vendor"]` оставить, пока жив `vendor/rtc-sctp`).
