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
- **REMB и RR от браузера (слайс 2.3, проверено вживую с Chrome 153):** тем же путём
  (`RtcpForwarder` → `TrackLocalEvent::OnRtcpPacket`) приходят
  `rtc::rtcp::payload_feedbacks::receiver_estimated_maximum_bitrate::ReceiverEstimatedMaximumBitrate`
  (`bitrate: f32` бит/с, `ssrcs` — наш SSRC) каждые ~200 мс и `rtc::rtcp::receiver_report::ReceiverReport`
  (`reports: Vec<ReceptionReport { ssrc, fraction_lost: u8, total_lost, jitter, last_sender_report, delay }>`).
  Маршрутизация в `webrtc` `driver.rs`: RTCP про *наш* трек идёт в `TrackLocal` по `destination_ssrc()`
  пакета. **TWCC-фидбека нет:** `register_default_interceptors` зовёт `configure_twcc_receiver_only`
  (расширение `transport-cc` объявляется в SDP, но `TwccSender` не регистрируется — исходящие пакеты
  без transport-wide seq), поэтому Chrome считает receive-side оценку и шлёт REMB. Особенность REMB:
  оценка ограничена сверху ~1.5× фактически принятого битрейта (AIMD в libwebrtc), поэтому REMB меньше
  цели кодера — норма при статичном экране; сигнал перегрузки — только REMB ниже реально отправленного.
  RTT из RR: `now_ntp_mid32 − last_sender_report − delay` в 1/65536 с (SR шлёт report-интерцептор хоста
  раз в секунду; до первого SR `last_sender_report == 0`).
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
- **Битрейт/fps на лету (слайс 2.3):** `EncoderConfig` крейта применяется только при создании;
  на живом кодере — `unsafe { encoder.raw_api() }.set_option(ENCODER_OPTION_BITRATE, &mut SBitrateInfo
  { iLayer: SPATIAL_LAYER_ALL, iBitrate })` (плюс `ENCODER_OPTION_MAX_BITRATE` и
  `ENCODER_OPTION_FRAME_RATE` с `f32`). Типы/константы — из `openh264-sys2` (крейт `openh264`
  реэкспортирует только `DynamicAPI as OpenH264API`), отсюда прямая зависимость. Ключевой кадр
  не форсируется, SPS/PPS не меняются. Осторожно: `encode_at` при смене размера кадра делает
  `reinit` с исходным `EncoderConfig` — цель битрейта откатится (у нас размер фиксирован).

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

## VideoToolbox (objc2-video-toolbox 0.3.2)

Реализация: `host/src/encode/videotoolbox.rs` (весь файл за
`#[cfg(target_os = "macos")]` в `encode/mod.rs`, не поэлементно).

- **Крейты/фичи** (`host/Cargo.toml`, секция macOS): `objc2-core-foundation`
  (`CFBase, CFString, CFNumber, CFDictionary, CFArray`) для CF-контейнеров свойств/
  словарей; `objc2-core-video` (`CVBase, CVBuffer, CVImageBuffer, CVPixelBuffer,
  CVReturn`) для `CVPixelBuffer`, которым кодер кормится; `objc2-core-media`
  (`CMBase, CMTime, CMBlockBuffer, CMFormatDescription, CMSampleBuffer,
  objc2-core-video`) для входного/выходного контейнера сэмпла; `objc2-video-toolbox`
  (`VTBase, VTErrors, VTSession, VTCompressionSession, VTCompressionProperties,
  objc2-core-media, objc2-core-video`) — сама сессия. Версии совпадают с тем, что уже
  тянет `enigo`/`objc2-app-kit` для macOS (`cargo tree -p rcdesk-host -i objc2 -e
  features` подтверждает единственную версию `objc2`/`objc2-core-foundation` в графе).
  Никакой фичи `objc2` (классы Objective-C) у них не включено — всё через чистые
  CF-типы, `.deref()`-цепочка `VTCompressionSession -> CFType` встроена в сам
  `cf_type!`-макрос objc2-core-foundation (нет явного `AsRef`/каста нужно).
- **Порядок инициализации** (`VideoToolboxEncoder::new`): `VTCompressionSession::create`
  (unsafe, `encoder_specification` = `{EnableHardwareAcceleratedVideoEncoder: true}` —
  именно `Enable`, не `Require`: на CI/VM без аппаратного кодера VT должен молча выдать
  программный) → цепочка `VTSessionSetProperty` (RealTime, ProfileLevel=
  ConstrainedBaseline_AutoLevel, AverageBitRate, DataRateLimits, AllowFrameReordering=false,
  MaxKeyFrameInterval, ExpectedFrameRate, опционально MaxAllowedFrameQP) →
  `VTCompressionSessionPrepareToEncodeFrames` → диагностическое чтение
  `UsingHardwareAcceleratedVideoEncoder` через `VTSessionCopyProperty` для лог-строки
  `hardware=true/false`. Любая ошибка в этой цепочке зовёт `VTCompressionSessionInvalidate`
  перед возвратом `Err` — иначе течёт сама сессия (CFRetained сам по себе только
  release'ит объект, `Invalidate` — отдельный шаг по документации Apple).
- ⚠️ **Найдено на реальном железе (Apple M4, macOS 26.6):** `VTSessionSetProperty` для
  `kVTCompressionPropertyKey_MaxFrameDelayCount = 0` возвращает `kVTPropertyNotSupportedErr`
  (`OSStatus -12900`) несмотря на то, что промпт/документация предполагали её как
  обязательную. Обработано мягко (`soft_set_property`, как `MaxAllowedFrameQP`) —
  `tracing::warn!` и продолжение без неё; сессия по-прежнему кодирует нормально
  (`encode()`'s call-and-drain и так не предполагает ровно один выход на кадр).
- **Смена битрейта/fps на живой сессии (слайс 2.3):** `AverageBitRate`, `DataRateLimits`,
  `ExpectedFrameRate` принимаются `VTSessionSetProperty` после `PrepareToEncodeFrames`
  без пересоздания сессии и без принудительного ключевого кадра (проверено на M4 тестом
  `set_rate_mid_stream_keeps_encoding_without_new_keyframe`); `ProfileLevel` и размеры —
  нет. У Media Foundation на лету меняется только `CODECAPI_AVEncCommonMeanBitRate` через
  `ICodecAPI`; `MF_MT_FRAME_RATE` — часть media type, fps там не трогаем (пейсинг в конвейере).
- **Выход — AVCC, а не Annex-B.** Кодер эмитит `CMSampleBuffer` с `CMBlockBuffer`, где
  каждый NAL предварён 4-байтовой big-endian длиной вместо старт-кода (размер префикса —
  `nal_unit_header_length_out` из `CMVideoFormatDescriptionGetH264ParameterSetAtIndex`,
  на практике 4, но код его не захардкоживает). Конвертация в Annex-B — построчный проход
  по AVCC с заменой длины на `00 00 00 01` (`avcc_to_annexb`). Данные блока читаются
  копированием (`CMBlockBufferCopyDataBytes`) — `CMBlockBuffer` может быть несмежным,
  указателя напрямую в общем случае нет.
- **SPS/PPS** достаются не из самого сэмпла, а из его `CMFormatDescription`
  (`CMSampleBufferGetFormatDescription` → `CMVideoFormatDescriptionGetH264ParameterSetAtIndex`,
  индекс 0 = SPS, 1 = PPS) и приписываются перед кадром вручную (`00 00 00 01`+SPS,
  `00 00 00 01`+PPS, затем сам кадр) только когда сэмпл ключевой — на дельта-кадрах их нет.
- **Определение ключевого кадра:** `CMSampleBufferGetSampleAttachmentsArray(sbuf, false)`
  → элемент 0 (если массив вообще есть) — это `CFDictionary`, но приходит как непараметризованный
  `CFArray`/`Opaque`-элемент; переинтерпретация в `CFArray<CFDictionary<CFString, CFType>>` через
  `CFRetained::cast_unchecked` безопасна, т.к. представление `CFArray<T>`/`CFDictionary<K,V>` не
  зависит от параметров типа (только `Opaque`-заглушка для дефолтного параметра). Ключевой кадр —
  **отсутствие** ключа `kCMSampleAttachmentKey_NotSync` в словаре ИЛИ его значение `false`
  (значение всегда `CFBoolean`, тот же `cast_unchecked` трюк). Отсутствие самого массива
  attachments тоже трактуется как ключевой кадр (по документации Apple он есть только когда
  сэмпл несинхронный).
- **`VTCompressionSessionCompleteFrames(session, kCMTimeInvalid)`** вызывается сразу после
  каждого `EncodeFrame` — это и есть механизм, которым `encode()` вытягивает результат
  синхронно в рамках одного вызова вместо отдельной очереди/потока: при `RealTime=true` и
  `MaxFrameDelayCount=0` (когда он принят) VT почти всегда успевает вызвать колбэк с готовым
  сэмплом до возврата из `CompleteFrames`. Колбэк складывает результат в `Mutex<Vec<..>>`
  внутри `Box<CallbackState>` (стабильный адрес, на который указывает
  `output_callback_ref_con`), `encode()` сразу же забирает содержимое `mem::take`'ом.
- **Колбэк — `unsafe extern "C-unwind" fn`.** VT может звать его на своём внутреннем потоке;
  никакой `unwrap()`/`expect()`/паники внутри — размотка стека через границу C-колбэка не
  определена. Любой сбой конвертации (нулевой указатель, `OSStatus != 0`, `FrameDropped`)
  превращается в `None`/`Err(status)`, а не в панику.
- **CFDictionary для разнотипных значений:** ключ/значение словаря свойств должны быть одного
  статического типа (`CFDictionary<CFString, CFType>::from_slices`), поэтому каждое реальное
  значение (`CFBoolean`, `CFNumber`, `CFArray<CFNumber>`, `&'static CFString` для ProfileLevel)
  приводится к `&CFType` через `.as_ref()` — тип выбирается компилятором по ожидаемому типу
  аргумента (`Option<&CFType>`/`Option<&CFDictionary>`), без ручных каст-функций.
- **Владение (`CFRetained`)**: `VTCompressionSessionCreate`/`CVPixelBufferCreate`/
  `VTSessionCopyProperty` отдают ссылку с +1 (обёрнуто `CFRetained::from_raw`); держать
  их в `CFRetained` и просто ронять — этого достаточно для `CVPixelBuffer`, но для самой
  сессии Apple явно требует `Invalidate` до релиза (см. выше). `CFDictionary::from_slices`/
  `CFArray::from_objects` ретейнят переданные элементы сами — временные `CFNumber`/`CFBoolean`,
  созданные прямо в вызове, безопасно живут только до конца выражения (стандартное
  Rust-правило temporary lifetime extension на весь оператор).

## Media Foundation (windows 0.61)

Реализация: `host/src/encode/mediafoundation.rs` (весь файл за
`#[cfg(target_os = "windows")]` в `encode/mod.rs`, не поэлементно). Проверено
**только компиляцией** (`cargo check`/`clippy --target x86_64-pc-windows-msvc`
в отдельном scratch-крейте, сигнатуры сверены с исходником `windows` 0.61.3 из
локального кэша реестра) — на живом Windows это никогда не запускалось этим
исполнителем; поведение подтверждает только CI (`windows-latest`, там есть
программный "H264 Encoder MFT" Microsoft) и тестовый стенд владельца (Windows
10, без аппаратного H.264 — там `build_encoder(None, ..)`/"auto" штатно уходит
в openh264, что покрыто отдельным логированием на `info!`, не `warn!`).

- **Крейты/фичи** (`host/Cargo.toml`, секция Windows): к уже имевшимся
  Direct3D/Dxgi/Gdi-фичам добавлены `Win32_Media_MediaFoundation` (сам MFT-API),
  `Win32_System_Com` (`CoInitializeEx`/`CoTaskMemFree`, и один из трёх компонентов
  фичи-гейта на `ICodecAPI::SetValue`), `Win32_System_Ole` и `Win32_System_Variant`
  (два других компонента того же гейта — `windows` требует все три фичи сразу на
  этом одном методе, проверено прямо в исходнике крейта). Ни одна из них не тянет
  новых пакетов в граф (весь функционал — cfg-модули внутри уже имеющегося
  пакета `windows`), поэтому `Cargo.lock` не меняется.
- **MFT — не COM-класс напрямую, а через `IMFActivate`.** `MFTEnumEx(MFT_CATEGORY_VIDEO_ENCODER,
  flags, Some(&input_type_info), Some(&output_type_info), &mut activates_ptr, &mut count)`
  ищет encoder-MFT с входом NV12 и выходом H264; сначала с
  `MFT_ENUM_FLAG_HARDWARE | MFT_ENUM_FLAG_SORTANDFILTER` (только аппаратные), и
  только если пусто и вызывающий явно разрешил (`allow_software`) — второй проход с
  `MFT_ENUM_FLAG_SYNCMFT | MFT_ENUM_FLAG_ASYNCMFT | MFT_ENUM_FLAG_SORTANDFILTER`.
  `build_encoder`: `None`/"auto" вызывает `new(cfg, allow_software = false)` (аппаратный
  MFT предпочтительнее openh264, но *программный* MFT не имеет преимущества перед уже
  обкатанным openh264 — лишний риск без выгоды), явный `Some(MediaFoundation)` —
  `new(cfg, true)` (пользователь осознанно попросил именно этот бэкенд).
  Результат `MFTEnumEx` — массив `*mut Option<IMFActivate>`, выделенный ОС через
  `CoTaskMemAlloc`: каждый элемент читается `ptr::read` (забирает +1-ссылку, не
  роняя элемент второй раз), сам массив освобождается одним `CoTaskMemFree` после
  цикла. Первый найденный активатор активируется в `IMFTransform`
  (`IMFActivate::ActivateObject::<IMFTransform>()`); имя MFT для лог-строки —
  `GetAllocatedString(&MFT_FRIENDLY_NAME_Attribute, ..)` через `IMFActivate`'s
  `Deref<Target = IMFAttributes>` (эта строка тоже `CoTaskMemAlloc`'d, освобождается
  отдельным `CoTaskMemFree`).
- **Sync vs async MFT.** `transform.GetAttributes()` может целиком провалиться —
  тогда MFT синхронный (`is_async = false`), это штатный путь, не ошибка. Если
  атрибуты есть и `GetUINT32(&MF_TRANSFORM_ASYNC) == 1` — MFT асинхронный, и перед
  любым использованием обязателен `SetUINT32(&MF_TRANSFORM_ASYNC_UNLOCK, 1)` на тех
  же атрибутах (иначе `ProcessInput`/`ProcessOutput` отказывают). Асинхронный MFT
  дополнительно кастуется в `IMFMediaEventGenerator` (`transform.cast()` — safe
  метод, не `unsafe`, несмотря на то что почти все остальные вызовы в этом модуле
  `unsafe`) и гоняется через событийный цикл: `METransformNeedInput` (601) копит
  "кредиты" на `ProcessInput`, `METransformHaveOutput` (602) запускает
  `ProcessOutput`. `GetEvent(MF_EVENT_FLAG_NO_WAIT)` — неблокирующий слив всего, что
  накопилось; `GetEvent(MF_EVENT_FLAG_NONE)` — блокирующее ожидание ровно одного
  события, используется только когда `encode()` не имеет ни одного кредита и должен
  дождаться `METransformNeedInput`, прежде чем звать `ProcessInput`. Синхронный MFT
  устроен проще: `ProcessInput` сразу за `ProcessOutput` в цикле,
  `MF_E_TRANSFORM_NEED_MORE_INPUT` на выходе — не ошибка, просто "кадр ещё не готов";
  `MF_E_NOTACCEPTING` на входе — входная очередь MFT полна, `encode()` сливает то, что
  есть на выходе (кладёт в `pending`, а не роняет — принадлежит предыдущему кадру), и
  повторяет `ProcessInput` один раз.
- **Порядок настройки типов: выходной, затем входной** — большинство encoder-MFT
  выбирают набор допустимых входных типов (`GetInputAvailableType`) исходя из уже
  установленного выходного. Выходной тип строится с нуля через `MFCreateMediaType()`
  (не перебором `GetOutputAvailableType`): `MF_MT_MAJOR_TYPE=Video`,
  `MF_MT_SUBTYPE=H264`, `MF_MT_AVG_BITRATE`, `MF_MT_FRAME_SIZE`/`MF_MT_FRAME_RATE`
  (упакованы как `(hi << 32) | lo`, по документации `MFVideoFormat`-макросов),
  `MF_MT_INTERLACE_MODE=Progressive`, `MF_MT_MPEG2_PROFILE=eAVEncH264VProfile_ConstrainedBase`
  (256). Если `SetOutputType` с Constrained Baseline проваливается —
  `tracing::warn!` и повтор с `eAVEncH264VProfile_Base` (66, обычная Baseline):
  Microsoft документирует для своего программного кодера только Base/Main/High,
  не Constrained Baseline, на части версий Windows. Входной тип сначала ищется
  перебором `GetInputAvailableType(0, i)` по subtype NV12 (`GetGUID(&MF_MT_SUBTYPE)`);
  если у MFT нет доступных входных типов вообще (типичная ошибка до того, как выходной
  тип задан — здесь уже не должно происходить благодаря порядку выше, но обработано
  как fallback) — тип строится с нуля тем же `MFCreateMediaType()`. В обоих случаях
  дополняется `MF_MT_FRAME_SIZE`/`MF_MT_FRAME_RATE`/`MF_MT_INTERLACE_MODE` и
  (только на входе) `MF_MT_ALL_SAMPLES_INDEPENDENT=1`.
- **`ICodecAPI`/`VARIANT` — ручная сборка union'а.** `ICodecAPI::SetValue` принимает
  `*const VARIANT`; сам тип — вложенные `union`ы (`VARIANT.Anonymous: VARIANT_0`,
  `VARIANT_0.Anonymous: ManuallyDrop<VARIANT_0_0>`, `VARIANT_0_0.Anonymous:
  VARIANT_0_0_0`), собираемые вручную двумя хелперами (`variant_u32`/`variant_bool`) —
  инициализация union-поля безопасна в Rust (небезопасно только *чтение*), так что
  сборка идёт без `unsafe`. `vt` — не голый `u16`, а `VARENUM`
  (`VT_UI4 = VARENUM(19)`, `VT_BOOL = VARENUM(11)`); булево значение — не Rust
  `bool`, а `VARIANT_BOOL` (`VARIANT_TRUE = -1i16`, `VARIANT_FALSE = 0i16`, из
  `Win32::Foundation`, не `Win32::System::Variant`). Каст к `ICodecAPI`
  (`transform.cast::<ICodecAPI>()`) — мягкий: недоступность интерфейса на части
  MFT (замечено, что не каждый софтверный MFT его реализует) не фатальна,
  `tracing::warn!` и пропуск всей ICodecAPI-настройки. Из самих свойств
  фатальны только `CODECAPI_AVEncCommonRateControlMode` (CBR,
  `eAVEncCommonRateControlMode_CBR = 0`) и `CODECAPI_AVEncCommonMeanBitRate`
  (без них битрейт не управляется вообще); `CODECAPI_AVLowLatencyMode`,
  `CODECAPI_AVEncMPVGOPSize`, `CODECAPI_AVEncVideoMaxQP` (только при заданном
  `max_qp`) и `CODECAPI_AVEncVideoForceKeyFrame` (за кадр, при `force_keyframe`)
  — мягкие, `tracing::warn!` и продолжение без них.
- **Выход — уже Annex-B, не AVCC** (в отличие от VideoToolbox выше): и аппаратные, и
  программный MFT Microsoft отдают H.264 со старт-кодами и SPS/PPS перед каждым IDR
  сами, так что здесь нет своего `avcc_to_annexb`-конвертера — модуль просто копирует
  байты выходного буфера и проверяет через переиспользуемый `encode::nal_types`, что
  NAL 7/8 (SPS/PPS) действительно присутствуют в первом выходе; если нет —
  `Backend`-ошибка (значит, тип/профиль был настроен неправильно). Промпт для этого
  под-шага не был проверен на живом MFT, так что эта проверка — единственная защита
  от молча неправильно сконфигурированного кодера.
- **Определение ключевого кадра** — `IMFSample::GetUINT32(&MFSampleExtension_CleanPoint)`
  (наследуется через `Deref<Target = IMFAttributes>`, как и у `IMFMediaType`/`IMFActivate`)
  ИЛИ найденный NAL 5 (IDR) в уже скопированных байтах — оба пути ИЛИ'ятся вместе, а не
  берётся первый успешный: не каждый MFT документированно ставит атрибут, так что
  сканирование NAL — не запасной, а равноправный источник истины.
- **Владение `MFT_OUTPUT_DATA_BUFFER::pSample`/`pEvents`** (`ManuallyDrop<Option<T>>`,
  не сами `Option<T>`) — после каждого `ProcessOutput` оба поля обязательно
  выгребаются через `ManuallyDrop::take` ровно один раз, независимо от того,
  успешен вызов или нет: МFT либо заполняет/возвращает сэмпл, который сюда
  передали (когда `!provides_samples`, см. `GetOutputStreamInfo`'s
  `MFT_OUTPUT_STREAM_PROVIDES_SAMPLES`), либо оставляет слот как был (обычно
  `None`, когда MFT сам предоставляет сэмплы) — в обоих случаях слот несёт
  ссылку, которую нужно уронить самим, `ManuallyDrop` этого не делает
  автоматически.
- **`MFT_OUTPUT_DATA_BUFFER_FORMAT_CHANGE` (256) в `dwStatus`** после успешного
  `ProcessOutput` означает, что выходной тип нужно перечитать
  (`GetOutputAvailableType(0, 0)`) и переустановить (`SetOutputType`) прежде чем
  повторить `ProcessOutput` — обработано как `continue` внутри цикла `drain_output`,
  не как отдельная функция верхнего уровня.
- **Время сэмплов — 100-наносекундные единицы (`hns`), не микросекунды** (в отличие
  от VideoToolbox выше, где `CMTime` брал произвольный `timescale`). Входной
  `IMFSample::SetSampleTime` считается от `frame.ts()` (момент захвата, не момент
  вызова `encode()`) относительно `started: Instant`, зафиксированного в `new()` —
  тот же принцип, что у `openh264`/`videotoolbox` (см. их док-комментарии): RTP-время
  кадра должно идти по реальным разрывам захвата, а не по счётчику кадров.
  Симметрично на выходе `captured_at` восстанавливается из
  `IMFSample::GetSampleTime()` того же `started`, не из `Instant::now()`.
