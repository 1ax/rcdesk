# Заметки по API библиотек хоста (проверено по исходникам 2026-09-11)

Версии: `webrtc` 0.20.5 (поверх sans-IO `rtc` 0.20.5), `scap` 0.0.8, `openh264` 0.9.8,
`enigo` 0.6.1. Пробная сборка всех четырёх вместе на macOS arm64 — 30 с, чисто.
**API `webrtc` 0.20 — новая архитектура, НЕ совпадает с 0.11–0.13 из памяти моделей.**

## webrtc 0.20

- Фича по умолчанию `runtime-tokio`. Рантайм передаётся явно: `webrtc::runtime::tokio`
  (см. `examples/common/mod.rs`: `runtime() -> Arc<dyn Runtime>`, `interval()`).
- Соединение:
  ```rust
  use webrtc::peer_connection::{PeerConnection, PeerConnectionBuilder, PeerConnectionEventHandler,
      RTCConfigurationBuilder, RTCIceServer, RTCPeerConnectionIceEvent};
  use rtc::peer_connection::configuration::media_engine::{MediaEngine, MIME_TYPE_H264};
  use rtc::peer_connection::configuration::interceptor_registry::register_default_interceptors;
  use rtc::interceptor::Registry;
  use rtc::rtp_transceiver::rtp_sender::{RTCRtpCodec, RTCRtpCodecParameters, RtpCodecKind,
      RTCRtpEncodingParameters, RTCRtpCodingParameters};

  let mut me = MediaEngine::default();
  let video_codec = RTCRtpCodecParameters { rtp_codec: RTCRtpCodec {
      mime_type: MIME_TYPE_H264.to_owned(), clock_rate: 90000, channels: 0,
      sdp_fmtp_line: "level-asymmetry-allowed=1;packetization-mode=1;profile-level-id=42e01f".to_owned(),
      rtcp_feedback: vec![] }, payload_type: 102, ..Default::default() };
  me.register_codec(video_codec.clone(), RtpCodecKind::Video)?;
  let registry = register_default_interceptors(Registry::new(), &mut me)?;
  let config = RTCConfigurationBuilder::new().with_ice_servers(vec![RTCIceServer{ urls: vec!["stun:stun.l.google.com:19302".into()], ..Default::default() }]).build();
  let pc = PeerConnectionBuilder::new().with_configuration(config).with_media_engine(me)
      .with_interceptor_registry(registry).with_handler(handler).with_runtime(runtime.clone())
      .with_udp_addrs(vec!["0.0.0.0:0"]).build().await?;
  ```
  `with_udp_addrs(["0.0.0.0:0"])` — wildcard биндит по сокету на каждый интерфейс
  (без loopback/link-local) → нормальные host-кандидаты.
- События — трейт `PeerConnectionEventHandler` (`#[async_trait]`), все методы с
  дефолтами: `on_negotiation_needed`, `on_ice_candidate(RTCPeerConnectionIceEvent)`,
  `on_ice_candidate_error`, `on_signaling_state_change`, `on_ice_connection_state_change`,
  `on_ice_gathering_state_change`, `on_connection_state_change(RTCPeerConnectionState)`,
  `on_data_channel(Arc<dyn DataChannel>)`, `on_track(Arc<dyn TrackRemote>)`.
  Trickle ICE: в `on_ice_candidate` брать `event.candidate` и слать через сигналинг.
- SDP: `pc.create_offer(None).await?`, `pc.set_local_description(offer).await?`,
  `pc.set_remote_description(RTCSessionDescription).await?`, `pc.local_description().await`,
  `pc.add_ice_candidate(...)`, `pc.close().await?`. `RTCSessionDescription` — serde.
- Видеотрек (сэмплы Annex-B H.264, один access unit на sample):
  ```rust
  use webrtc::media_stream::{MediaStreamTrack, track_local::{TrackLocal, static_sample::TrackLocalStaticSample}};
  use rtc::media::Sample;
  let ssrc = rand::random::<u32>();
  let track = Arc::new(TrackLocalStaticSample::new(MediaStreamTrack::new(
      "rcdesk-stream".into(), "rcdesk-video".into(), "screen".into(), RtpCodecKind::Video,
      vec![RTCRtpEncodingParameters { rtp_coding_parameters: RTCRtpCodingParameters { ssrc: Some(ssrc), ..Default::default() },
           codec: video_codec.rtp_codec.clone(), ..Default::default() }]))?);
  let sender = pc.add_track(Arc::clone(&track) as Arc<dyn TrackLocal>).await?;
  // после переговоров:
  let pt = sender.get_parameters().await?.rtp_parameters.codecs.first().unwrap().payload_type;
  let ssrc = *track.ssrcs().await.first().unwrap();
  track.sample_writer(ssrc, pt).write_sample(&Sample { data: bytes, duration, ..Default::default() }).await?;
  ```
- **`TrackLocalStaticSample::write_sample`'s `duration` is not "how long this frame
  took", it's "how long until the next one"** (найдено при разборе слайса 2.1d: Chrome
  держал ~850 мс в jitter-буфере при цели 83 мс). Внутри `write_sample` вызывает
  `packetizer.packetize(data, samples)` (`samples = duration * clock_rate`,
  `static_sample.rs:147-159`), а `PacketizerImpl::packetize` (`rtc-rtp-0.20.5/src/
  packetizer/mod.rs:152-160`) сначала штампует пакеты ТЕКУЩИМ `self.timestamp`, и
  только ПОСЛЕ прибавляет `samples`. То есть RTP-время кадра k = сумма `duration`
  всех предыдущих кадров, а не момент захвата этого кадра. Для источника с
  постоянным fps это неотличимо от wall-clock; для экрана, где кадры идут только
  при изменении содержимого (SCK/GDI-дедуп), `duration = 1/fps` даёт часы потока,
  систематически отстающие от реального времени — и клиентский jitter-buffer
  (Chrome) расписывает показ по этим часам, а не по фактическому приходу пакетов.
  **Фикс:** пакетизировать самому через низкоуровневый API вместо `TrackLocalStaticSample`:
  ```rust
  use webrtc::media_stream::track_local::static_rtp::TrackLocalStaticRTP; // не *Sample
  use rtc::rtp::codec::h264::H264Payloader;
  use rtc::rtp::packetizer::{new_packetizer, Packetizer};
  use rtc::rtp::sequence::new_random_sequencer;

  let track = Arc::new(TrackLocalStaticRTP::new(media_stream_track)); // не Result, в отличие от *Sample::new
  // ssrc/pt уже известны (после негоциации) -> создаём пакетизатор сразу с ними,
  // не с payload_type=0 как это делает *Sample::new для всех ssrc заранее.
  let mut packetizer = new_packetizer(1200 /* MTU, `RTP_OUTBOUND_MTU` в static_sample.rs не pub */,
      pt, ssrc, Box::new(H264Payloader::default()), Box::new(new_random_sequencer()), 90_000);
  packetizer.skip_samples(((captured_at - prev_captured_at).as_secs_f64() * 90_000.0) as u32); // 0 на первом кадре
  for pkt in packetizer.packetize(&payload, 0)? { // samples=0: время уже сдвинуто выше
      track.write_rtp_with_extensions(pkt, &[]).await?;
  }
  ```
  `skip_samples` (`packetizer/mod.rs:184`) двигает только `timestamp`, sequencer не
  трогает — корректно для гэпа между кадрами. `Packetizer`/`PacketizerImpl` не дают
  сменить `payload_type` после создания, поэтому пакетизатор создаётся не сразу
  (как для `*Sample`), а в момент, когда ssrc/pt уже известны из
  `RtpSender::get_parameters()`. `TrackLocalStaticRTP` реализует тот же `Track`/
  `TrackLocal` (`poll()` → `TrackLocalEvent::OnRtcpPacket`, `ssrcs()`, `codec()`) —
  PLI/FIR-приём ниже не меняется.
- **PLI/FIR от браузера** приходят на трек: `while let Some(ev) = track.poll().await {
  match ev { TrackLocalEvent::OnRtcpPacket(pkts) => … } }` — пакеты `rtc::rtcp`,
  проверять `pkt.as_any().downcast_ref::<PictureLossIndication>()` / `FullIntraRequest`.
  **НО (найдено в 1.2b):** `NoopInterceptor` — внутреннее звено любой цепочки, включая
  `register_default_interceptors` — намеренно дропает входящие RTCP при чтении; дефолтные
  интерцепторы (NACK, reports, TWCC) их не пробрасывают. Нужен свой интерцептор, который
  копирует RTCP в свою очередь `poll_read` — см. `RtcpForwarder` в `host/src/transport/mod.rs`
  (`registry.with(|next| RtcpForwarder { next, .. })`). Макросы `#[derive(Interceptor)]`
  требуют прямых зависимостей `sansio`/`shared`/`interceptor` — написан руками через
  `rtc::sansio::Protocol` + `rtc::interceptor::Interceptor`.
- Реэкспорты: `webrtc::peer_connection::*` отдаёт `MediaEngine`, `Registry`,
  `register_default_interceptors`, `RTCConfigurationBuilder`, `RTCIceServer`,
  `RTCSessionDescription`, `RTCIceCandidateInit`, `RTCPeerConnectionState`. Через `rtc::` нужны
  `MIME_TYPE_H264`, `RTCRtpCodec*`, `RtpCodecKind`, `RTCPFeedback`, `Sample`, rtcp-пакеты.
- Отправлять сэмплы можно только после `RTCPeerConnectionState::Connected`: до готовности
  DTLS/SRTP `write_sample` молча дропает данные (`local_srtp_context is not set yet`),
  и форсированный стартовый ключевой кадр теряется.
- Data channel: `pc.create_data_channel(label, Option<RTCDataChannelInit>)` →
  `Arc<dyn DataChannel>`; `RTCDataChannelInit { ordered: bool, max_retransmits: Option<u16>,
  max_packet_life_time, protocol, negotiated, .. }` из `rtc::data_channel::init`.
  Приём: `while let Some(ev) = dc.poll().await { match ev { DataChannelEvent::OnOpen |
  OnClose | OnError | OnMessage(msg /* msg.data: Bytes, msg.is_string */) } }`.
  Отправка: `dc.send_text(&str)`, `dc.send(BytesMut)`, `dc.try_send*`, backpressure
  через `buffered_amount_low_threshold` / `writable().await`.

## scap 0.0.8

- ⚠️ На Windows `scap` 0.0.8 не собирается с `windows-capture` 1.5+ (сигнатура `Settings::new`
  выросла с 5 до 8 аргументов). В `host/Cargo.toml` прямой пин `windows-capture = "=1.4.4"`.
  В 1.5+/2.x есть `DirtyRegionSettings` и `MinimumUpdateIntervalSettings` — аргумент за прямой
  крейт в фазе 2.
- ⚠️ Найдено на тестовом стенде (Windows 10, ATI Radeon HD 4600, Direct3D 10.1, легаси-драйвер
  WDDM 1.1): `windows-capture` 1.4.4's `create_d3d_device` (`d3d11.rs:55`) требует у
  `D3D11CreateDevice` уровень поддержки ≥ `D3D_FEATURE_LEVEL_11_0` и превращает более низкий
  уровень в `Error::FeatureLevelNotSatisfied` — корректно, `Result`. Но `scap` 0.0.8 это
  значение разворачивает через `.unwrap()`
  (`scap-0.0.8/src/capturer/engine/win/mod.rs:120`, `WCStream::start_capture`:
  `Capturer::start_free_threaded(st.to_owned()).unwrap()`), поэтому на таком железе `bench`/
  `serve` не возвращают ошибку, а паникуют. Решение: пробное `D3D11CreateDevice` до вызова
  scap — `platform::windows::d3d::wgc_supported()` (host/src/platform/windows/d3d.rs), с тем же
  набором feature levels/флагов, что и `create_d3d_device`; `main.rs` вызывает его для
  `--capture auto` и при `false` уходит на запасной захват через GDI
  (`host/src/capture/gdi.rs`, `BitBlt`).

- Разрешение: `scap::has_permission()`, `scap::request_permission()`, `scap::is_supported()`.
- Цели: `scap::get_all_targets() -> Vec<Target>` (`Target::Display(Display{id,title,raw_handle})`
  / `Target::Window`), `scap::get_main_display()`.
- `Capturer::build(Options { fps, show_cursor, show_highlight, target, crop_area,
  output_type: FrameType, output_resolution: Resolution::Captured, excluded_targets })`
  → `Result<Capturer, CapturerBuildError{NotSupported|PermissionNotGranted}>`;
  `start_capture()`, `stop_capture()`, **блокирующий** `get_next_frame() -> Result<Frame, RecvError>`
  (std mpsc) — вызывать из выделенного потока, не из tokio-таска.
- **macOS:** `FrameType::YUVFrame` → `Frame::YUVFrame(YUVFrame { display_time, width, height,
  luminance_bytes, luminance_stride, chrominance_bytes, chrominance_stride })` — это
  **NV12** (`YCbCr420v`, биплан: Y + перемешанный CbCr). Для openh264 нужно I420 —
  деинтерливинг CbCr в два плана (дёшево, один проход).
  Кадры приходят только при изменении экрана (плюс `SCFrameStatus::Idle` даёт пустой
  BGRA-кадр width=0 только в режиме BGRAFrame — в YUV-режиме просто нет кадра).
- **Windows:** только `Frame::BGRA(BGRAFrame { display_time, width, height, data })`
  (WGC, BGRA8). Конверсия в I420 — `openh264::formats::{YUVBuffer::from_rgb8_source,
  BgraSliceU8}` (SIMD-ускорена в крейте).
- `show_cursor: false` поддержан на обеих платформах.
- Предупреждение future-incompat от транзитивного `block 0.1.6` (objc-стек) — не наш код.

## openh264 0.9.8

- `Encoder::with_api_config(OpenH264API::from_source(), EncoderConfig::new()
  .bitrate(BitRate::from_bps(..)).max_frame_rate(FrameRate::from_hz(30.0))
  .usage_type(UsageType::ScreenContentRealTime).rate_control_mode(RateControlMode::Bitrate)
  .profile(Profile::Baseline).level(Level::Level_3_1).sps_pps_strategy(SpsPpsStrategy::ConstantId)
  .intra_frame_period(IntraFramePeriod::from_num_frames(..)).skip_frames(false)
  .num_threads(n).complexity(Complexity::Low))`.
  (Проверить точные имена конструкторов `BitRate/FrameRate/IntraFramePeriod` по `encoder.rs`
  строки ~190–410 перед использованием.)
- `encoder.encode(&yuv)` / `encode_at(&yuv, Timestamp::from_millis(ms))` →
  `EncodedBitStream`: `.to_vec()` — Annex-B со start-кодами, `frame_type()`;
  `encoder.force_intra_frame()` — ключевой кадр по PLI.
- Источники YUV: `YUVBuffer::from_vec(i420, w, h)`, `YUVBuffer::new(w,h)`,
  `YUVSlices::new((y,u,v), (w,h), (sy,su,sv))` — без копии, реализует `YUVSource`.
  Ширина/высота должны быть чётными.

## enigo 0.6.1

- `Enigo::new(&Settings::default())`, трейты `Keyboard` (`key(Key, Direction)`, `text(&str)`,
  `raw(u16, Direction)` — платформенный код клавиши) и `Mouse` (`move_mouse(x, y, Coordinate::Abs)`,
  `button(Button, Direction)`, `scroll(len, Axis)`), `Direction::{Press, Release, Click}`.
- `Key` — часть вариантов за `cfg(target_os)`; для маппинга `event.code` → клавиша
  использовать `Key::Unicode(char)` для символов и `Key::{Shift, Control, Alt, Meta, …}`
  для модификаторов, либо `raw(vk)` с платформенной таблицей (решение слайса 1.4).
- macOS: нужно разрешение «Универсальный доступ», иначе события молча не доставляются.
- Windows: `raw()` сам транслирует скан-код в VK через `MapVirtualKeyW(scan, MAPVK_VSC_TO_VK_EX)`
  и выставляет `KEYEVENTF_EXTENDEDKEY` только если VK есть в своей неполной таблице
  (`is_extended_key` в `win_impl.rs`); префикс `E0` в самом скан-коде не принимает и не понимает.
  Поэтому клавиши на Windows шлём напрямую через `SendInput`
  (`host/src/platform/windows/keyboard.rs`), `enigo` остаётся только для мыши и колеса.
