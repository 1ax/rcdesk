//! Media Foundation-backed `Encoder`: Windows H.264 encode via an
//! `IMFTransform` MFT (`MFT_CATEGORY_VIDEO_ENCODER`). This whole module is
//! Windows-only -- gated at `mod.rs` (`#[cfg(target_os = "windows")]`), not
//! per-item here.
//!
//! Compiles only on `windows-latest` in CI: there is no Windows machine
//! available to this executor run, so every signature/type below comes from
//! reading the `windows` 0.61.3 source fetched into the local registry
//! cache, not from memory -- see the same approach `capture::gdi`,
//! `platform::windows::d3d`, `cursor::windows` and
//! `platform::windows::keyboard` document and use. See
//! `docs/host-libs-api-notes.md`'s Media Foundation section for the API
//! notes this module was written against.

use std::collections::VecDeque;
use std::mem::ManuallyDrop;
use std::time::{Duration, Instant};

use windows::core::{Interface, GUID, PWSTR};
use windows::Win32::Foundation::{RPC_E_CHANGED_MODE, VARIANT_FALSE, VARIANT_TRUE};
use windows::Win32::Media::MediaFoundation::{
    eAVEncCommonRateControlMode_CBR, eAVEncH264VProfile_Base, eAVEncH264VProfile_ConstrainedBase,
    CODECAPI_AVEncCommonBufferSize, CODECAPI_AVEncCommonMeanBitRate,
    CODECAPI_AVEncCommonRateControlMode, CODECAPI_AVEncMPVGOPSize,
    CODECAPI_AVEncVideoForceKeyFrame, CODECAPI_AVEncVideoMaxQP, CODECAPI_AVLowLatencyMode,
    ICodecAPI, IMFActivate, IMFMediaBuffer, IMFMediaEvent, IMFMediaEventGenerator, IMFMediaType,
    IMFSample, IMFTransform, METransformHaveOutput, METransformNeedInput, MFCreateMediaType,
    MFCreateMemoryBuffer, MFCreateSample, MFMediaType_Video, MFSampleExtension_CleanPoint,
    MFShutdown, MFStartup, MFTEnumEx, MFT_FRIENDLY_NAME_Attribute, MFVideoFormat_H264,
    MFVideoFormat_NV12, MFVideoInterlace_Progressive, MFSTARTUP_NOSOCKET,
    MFT_CATEGORY_VIDEO_ENCODER, MFT_ENUM_FLAG, MFT_ENUM_FLAG_ASYNCMFT, MFT_ENUM_FLAG_HARDWARE,
    MFT_ENUM_FLAG_SORTANDFILTER, MFT_ENUM_FLAG_SYNCMFT, MFT_MESSAGE_COMMAND_FLUSH,
    MFT_MESSAGE_NOTIFY_BEGIN_STREAMING, MFT_MESSAGE_NOTIFY_END_OF_STREAM,
    MFT_MESSAGE_NOTIFY_START_OF_STREAM, MFT_OUTPUT_DATA_BUFFER,
    MFT_OUTPUT_DATA_BUFFER_FORMAT_CHANGE, MFT_OUTPUT_STREAM_INFO,
    MFT_OUTPUT_STREAM_PROVIDES_SAMPLES, MFT_REGISTER_TYPE_INFO, MF_EVENT_FLAG_NO_WAIT,
    MF_E_NOTACCEPTING, MF_E_NO_EVENTS_AVAILABLE, MF_E_TRANSFORM_NEED_MORE_INPUT,
    MF_MT_ALL_SAMPLES_INDEPENDENT, MF_MT_AVG_BITRATE, MF_MT_FRAME_RATE, MF_MT_FRAME_SIZE,
    MF_MT_INTERLACE_MODE, MF_MT_MAJOR_TYPE, MF_MT_MPEG2_PROFILE, MF_MT_SUBTYPE, MF_TRANSFORM_ASYNC,
    MF_TRANSFORM_ASYNC_UNLOCK, MF_VERSION,
};
use windows::Win32::System::Com::{
    CoDecrementMTAUsage, CoIncrementMTAUsage, CoInitializeEx, CoTaskMemFree, CoUninitialize,
    COINIT_MULTITHREADED, CO_MTA_USAGE_COOKIE,
};
use windows::Win32::System::Variant::{
    VARIANT, VARIANT_0, VARIANT_0_0, VARIANT_0_0_0, VT_BOOL, VT_I4, VT_UI4, VT_UI8,
};

use crate::capture::RawFrame;
use crate::encode::convert::{nv12_len, Nv12Writer};
use crate::encode::{fps_compensated_bitrate_bps, nal_types};

use super::{EncodeError, EncodedFrame, Encoder, EncoderConfig, RateTarget};

/// Maximum time `encode()` waits for a `METransformNeedInput` credit. A
/// hardware MFT returns `NeedInput` within milliseconds; 2 s means the encoder
/// is stuck, and an honest `Err` (the D58 path) beats waiting forever.
const CREDIT_WAIT_TIMEOUT: Duration = Duration::from_secs(2);

/// Maximum time `encode()` waits for the output of the frame it just
/// submitted, so the returned output belongs to this call and not to the
/// previous input. Hardware NVENC at 2560x1440 encodes in ~6-15 ms; expiry is
/// not an error -- the frame surfaces with the next call, as it did before.
const OUTPUT_WAIT_TIMEOUT: Duration = Duration::from_millis(50);

/// Minimum interval between `MeanBitRate` readbacks of the drift watchdog.
const DRIFT_CHECK_INTERVAL: Duration = Duration::from_secs(5);

pub struct MediaFoundationEncoder {
    transform: IMFTransform,
    is_async: bool,
    /// Only `Some` when `is_async` -- the event-driven interface async MFTs
    /// signal `METransformNeedInput`/`METransformHaveOutput` through.
    events: Option<IMFMediaEventGenerator>,
    /// Kept alive so `encode()` can set `CODECAPI_AVEncVideoForceKeyFrame`
    /// per frame; `None` when the MFT has no `ICodecAPI` at all (soft
    /// failure at construction, see `configure_codec_api`).
    codec_api: Option<ICodecAPI>,
    cfg: EncoderConfig,
    /// Wall-clock reference for the sample times handed to the MFT (100ns
    /// units) -- same reasoning as `openh264::OpenH264Encoder::started` /
    /// `videotoolbox::VideoToolboxEncoder::started`.
    started: Instant,
    /// Whether the MFT fills a sample buffer we allocate (`false`) or hands
    /// back its own sample (`true`) -- see `GetOutputStreamInfo`'s
    /// `MFT_OUTPUT_STREAM_PROVIDES_SAMPLES` flag.
    provides_samples: bool,
    /// Minimum output buffer size to allocate when `!provides_samples`.
    output_buffer_size: u32,
    /// Reused output sample + its buffer for the `!provides_samples` case,
    /// created lazily by `drain_output` and dropped (to be recreated) on a
    /// format change or a `ProcessOutput` error.
    output_cache: Option<(IMFSample, IMFMediaBuffer)>,
    /// Reusable BGRA->I420 scratch for `build_input_sample`.
    nv12_writer: Nv12Writer,
    /// Async MFTs only: how many `ProcessInput` calls the MFT has signalled
    /// it can currently accept (one `METransformNeedInput` event = one
    /// credit).
    credits: u32,
    /// Encoded frames produced but not yet returned from `encode()` -- an
    /// async MFT can signal `METransformHaveOutput` for a frame submitted
    /// several `encode()` calls ago, and a sync MFT's `MF_E_NOTACCEPTING`
    /// recovery (see `encode_sync`) can likewise produce an output that
    /// belongs to a previous call.
    pending: VecDeque<EncodedFrame>,
    /// Set once `set_rate` has warned that this MFT has no `ICodecAPI` (see
    /// `configure_codec_api`), so repeated calls don't spam the log.
    rate_warned: bool,
    /// Async MFTs only: how many inputs handed to `ProcessInput` have not yet
    /// produced an output (`METransformHaveOutput` drained into a frame).
    in_flight: u32,
    /// Set once the async output backlog warning has fired, so a persistent lag
    /// doesn't spam the log.
    backlog_warned: bool,
    /// Last `MeanBitRate` (bps) we wrote ourselves (construction or `set_rate`).
    last_set_bitrate_bps: u32,
    /// Last `MeanBitRate` read back by the drift watchdog, so a persistent
    /// drifted value is logged once (`None` until the first check).
    last_seen_bitrate_bps: Option<u32>,
    /// When the drift watchdog last read `MeanBitRate` back.
    drift_checked_at: Instant,
    /// Balances the successful `MFStartup` with exactly one `MFShutdown`,
    /// whichever path (build error, drain error, normal drop) ends the
    /// encoder. Declared after `transform`/`codec_api` so they are released
    /// first, before `MFShutdown`; and before `_mta`.
    _mf: MfSession,
    /// MUST stay the last field: fields drop in declaration order, so the MTA
    /// is released only after `transform`/`codec_api` are freed and after
    /// `Drop::drop` and `_mf` (`MFShutdown`).
    _mta: MtaUsage,
}

/// One successful `MFStartup`; calls `MFShutdown` exactly once when dropped.
/// Created right after `MFStartup` in `new` and moved into `build` and then
/// into the encoder, so no error path can shut Media Foundation down twice.
struct MfSession;

impl MfSession {
    fn startup() -> Result<Self, EncodeError> {
        // SAFETY: the matching `MFShutdown` is `MfSession::drop`, run exactly
        // once for the value returned here.
        unsafe { MFStartup(MF_VERSION, MFSTARTUP_NOSOCKET) }
            .map_err(|err| EncodeError::Backend(format!("MediaFoundation MFStartup: {err}")))?;
        Ok(Self)
    }
}

impl Drop for MfSession {
    fn drop(&mut self) {
        // SAFETY: an `MfSession` exists only after a successful `MFStartup`
        // and is dropped once.
        unsafe {
            let _ = MFShutdown();
        }
    }
}

/// Scoped `CoInitializeEx`: calls `CoUninitialize` when dropped, on the same
/// thread that initialized COM (it never leaves `new`).
struct ComGuard;

impl Drop for ComGuard {
    fn drop(&mut self) {
        // SAFETY: a `ComGuard` is only created after `CoInitializeEx`
        // returned success (`S_OK`/`S_FALSE`) on the current thread, and it
        // is a local of `new`, dropped on that same thread: one
        // `CoUninitialize` per successful `CoInitializeEx`.
        unsafe { CoUninitialize() };
    }
}

/// Keeps the process-wide multi-threaded COM apartment alive for the whole
/// lifetime of the encoder (`CoIncrementMTAUsage`, Win8+).
///
/// The encoder is built on a tokio blocking-pool thread (V-1) but driven on
/// the pipeline's encode thread, which relies on the implicit MTA. The cookie
/// keeps that MTA alive regardless of which threads come and go, while the
/// `CoInitializeEx` in `new` is balanced on its own thread, with no
/// thread-local destructors (they would run under the loader lock, where
/// `CoUninitialize` must not be called).
struct MtaUsage(CO_MTA_USAGE_COOKIE);

// SAFETY: the cookie is an opaque handle; per the `CoDecrementMTAUsage`
// documentation it may be released from any thread, and nothing else is done
// with it.
unsafe impl Send for MtaUsage {}

impl MtaUsage {
    fn acquire() -> Result<Self, EncodeError> {
        // SAFETY: no preconditions; the returned cookie is released exactly
        // once by `Drop`.
        unsafe { CoIncrementMTAUsage() }.map(Self).map_err(|err| {
            EncodeError::Backend(format!("MediaFoundation CoIncrementMTAUsage: {err}"))
        })
    }
}

impl Drop for MtaUsage {
    fn drop(&mut self) {
        // SAFETY: `self.0` came from a successful `CoIncrementMTAUsage` and
        // is released only here.
        let _ = unsafe { CoDecrementMTAUsage(self.0) };
    }
}

// SAFETY: like `VideoToolboxEncoder`/`OpenH264Encoder`, `MediaFoundationEncoder`
// is only ever driven from the single dedicated encode thread that owns it
// for its entire lifetime (created, used and dropped there); we only need
// `Send` to hand the freshly-built value to that thread once. The COM
// apartment this module initializes (`COINIT_MULTITHREADED`, i.e. the
// multi-threaded apartment) is documented by Microsoft as allowing any
// thread in the process to call directly into an MTA object's interfaces
// (no marshaling needed) as long as calls aren't made concurrently from
// multiple threads at once, which single-threaded ownership here guarantees.
unsafe impl Send for MediaFoundationEncoder {}

impl MediaFoundationEncoder {
    /// `allow_software` gates whether a software (non-hardware) MFT is
    /// acceptable: `false` when the caller asked for "auto" (the fallback to
    /// openh264 is preferred over Media Foundation's own software encoder,
    /// see `build_encoder`'s doc comment), `true` when the caller explicitly
    /// requested `EncoderKind::MediaFoundation`.
    pub fn new(cfg: EncoderConfig, allow_software: bool) -> Result<Self, EncodeError> {
        // SAFETY: `None` for `pvReserved` is the documented "reserved, must
        // be null" value; `COINIT_MULTITHREADED` is the apartment model this
        // module's `Send` impl above relies on. On success the local guard
        // balances the call with `CoUninitialize` when `new` returns.
        // `RPC_E_CHANGED_MODE` means COM was already initialized on this
        // thread with another model: usable, but that call did not bump the
        // thread's COM count, so no guard (and no `CoUninitialize`).
        let co_status = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) };
        let _com_guard = if co_status.is_ok() {
            Some(ComGuard)
        } else if co_status == RPC_E_CHANGED_MODE {
            None
        } else {
            return Err(EncodeError::Backend(format!(
                "MediaFoundation CoInitializeEx: HRESULT {:#x}",
                co_status.0
            )));
        };
        // Taken while the guard above is alive; owned by the encoder (or
        // dropped with `build`'s error).
        let mta = MtaUsage::acquire()?;

        // The session owns the single `MFShutdown`; `build` hands it to the
        // encoder, and any error path drops it (once) on the way out.
        let mf = MfSession::startup()?;

        Self::build(cfg, allow_software, mta, mf)
    }

    fn build(
        cfg: EncoderConfig,
        allow_software: bool,
        mta: MtaUsage,
        mf: MfSession,
    ) -> Result<Self, EncodeError> {
        let input_type_info = MFT_REGISTER_TYPE_INFO {
            guidMajorType: MFMediaType_Video,
            guidSubtype: MFVideoFormat_NV12,
        };
        let output_type_info = MFT_REGISTER_TYPE_INFO {
            guidMajorType: MFMediaType_Video,
            guidSubtype: MFVideoFormat_H264,
        };

        let mut hardware = true;
        let mut activates = enumerate_mfts(
            MFT_ENUM_FLAG_HARDWARE | MFT_ENUM_FLAG_SORTANDFILTER,
            &input_type_info,
            &output_type_info,
        )?;
        if activates.is_empty() && allow_software {
            hardware = false;
            activates = enumerate_mfts(
                MFT_ENUM_FLAG_SYNCMFT | MFT_ENUM_FLAG_ASYNCMFT | MFT_ENUM_FLAG_SORTANDFILTER,
                &input_type_info,
                &output_type_info,
            )?;
        }
        let Some(activate) = activates.into_iter().next() else {
            return Err(EncodeError::Backend(
                "no H.264 encoder MFT found".to_string(),
            ));
        };

        let friendly_name = read_friendly_name(&activate);
        tracing::info!(name = %friendly_name, hardware, "media foundation encoder");

        // SAFETY: `activate` was just returned by a successful `MFTEnumEx`
        // call above.
        let transform: IMFTransform = unsafe { activate.ActivateObject() }.map_err(|err| {
            EncodeError::Backend(format!("MediaFoundation ActivateObject: {err}"))
        })?;

        let is_async = unlock_async(&transform)?;

        let codec_api = configure_codec_api(&transform, &cfg)?;
        set_output_type(&transform, &cfg)?;
        set_input_type(&transform, &cfg)?;

        // SAFETY: `transform` has both its input and output types set.
        let stream_info: MFT_OUTPUT_STREAM_INFO = unsafe { transform.GetOutputStreamInfo(0) }
            .map_err(|err| {
                EncodeError::Backend(format!("MediaFoundation GetOutputStreamInfo: {err}"))
            })?;
        let provides_samples =
            stream_info.dwFlags & MFT_OUTPUT_STREAM_PROVIDES_SAMPLES.0 as u32 != 0;
        let output_buffer_size = stream_info.cbSize.max(cfg.width * cfg.height);

        // SAFETY: `transform` is fully configured (input/output types set).
        unsafe { transform.ProcessMessage(MFT_MESSAGE_NOTIFY_BEGIN_STREAMING, 0) }.map_err(
            |err| EncodeError::Backend(format!("MediaFoundation NOTIFY_BEGIN_STREAMING: {err}")),
        )?;
        // SAFETY: same as above.
        unsafe { transform.ProcessMessage(MFT_MESSAGE_NOTIFY_START_OF_STREAM, 0) }.map_err(
            |err| EncodeError::Backend(format!("MediaFoundation NOTIFY_START_OF_STREAM: {err}")),
        )?;

        let events = if is_async {
            Some(transform.cast::<IMFMediaEventGenerator>().map_err(|err| {
                EncodeError::Backend(format!(
                    "MediaFoundation cast to IMFMediaEventGenerator: {err}"
                ))
            })?)
        } else {
            None
        };

        tracing::info!(
            width = cfg.width,
            height = cfg.height,
            fps = cfg.fps,
            bitrate_kbps = cfg.bitrate_kbps,
            max_qp = ?cfg.max_qp,
            hardware,
            r#async = is_async,
            "initializing mediafoundation encoder"
        );

        log_codec_readback(codec_api.as_ref(), is_async, cfg.fps);
        let last_set_bitrate_bps = cfg.bitrate_kbps.saturating_mul(1000);

        let mut encoder = Self {
            transform,
            is_async,
            events,
            codec_api,
            cfg,
            started: Instant::now(),
            provides_samples,
            output_buffer_size,
            output_cache: None,
            nv12_writer: Nv12Writer::new(),
            credits: 0,
            pending: VecDeque::new(),
            rate_warned: false,
            in_flight: 0,
            backlog_warned: false,
            last_set_bitrate_bps,
            last_seen_bitrate_bps: None,
            drift_checked_at: Instant::now(),
            _mf: mf,
            _mta: mta,
        };

        if is_async {
            // An async MFT starts sending `METransformNeedInput` events as
            // soon as streaming begins, before `encode()` is ever called --
            // drain those now so `credits` reflects reality from the start.
            encoder.drain_events_no_wait()?;
        }

        Ok(encoder)
    }

    /// Copies `sample`'s encoded payload out and wraps it as an
    /// `EncodedFrame`, using `MFSampleExtension_CleanPoint` (falling back to
    /// scanning for an IDR NAL, since not every MFT sets the attribute) to
    /// decide `keyframe`.
    fn frame_from_output_sample(&self, sample: &IMFSample) -> Result<EncodedFrame, EncodeError> {
        // SAFETY: `sample` is a live `IMFSample` handed back by a successful
        // `ProcessOutput`. A missing attribute (the common case on MFTs that
        // don't set it) surfaces as `Err`, treated as "not a clean point"
        // here -- `has_idr` below is the fallback that actually decides
        // `keyframe` in that case.
        let clean_point =
            unsafe { sample.GetUINT32(&MFSampleExtension_CleanPoint) }.unwrap_or(0) == 1;

        // SAFETY: same sample.
        let buffer = unsafe { sample.ConvertToContiguousBuffer() }.map_err(|err| {
            EncodeError::Backend(format!("MediaFoundation ConvertToContiguousBuffer: {err}"))
        })?;

        let mut ptr: *mut u8 = std::ptr::null_mut();
        let mut current_len: u32 = 0;
        // SAFETY: `ptr`/`current_len` are valid stack out-parameters; the
        // buffer was just obtained above and isn't shared with anything
        // else yet.
        unsafe { buffer.Lock(&mut ptr, None, Some(&mut current_len)) }.map_err(|err| {
            EncodeError::Backend(format!(
                "MediaFoundation output IMFMediaBuffer::Lock: {err}"
            ))
        })?;
        // SAFETY: `Lock` returned success just above, so `ptr` is valid for
        // `current_len` bytes until `Unlock`.
        let data = unsafe { std::slice::from_raw_parts(ptr, current_len as usize) }.to_vec();
        // SAFETY: matches the successful `Lock` above.
        unsafe { buffer.Unlock() }.map_err(|err| {
            EncodeError::Backend(format!(
                "MediaFoundation output IMFMediaBuffer::Unlock: {err}"
            ))
        })?;

        let has_idr = nal_types(&data).contains(&5);
        let keyframe = clean_point || has_idr;

        // SAFETY: same sample.
        let sample_time_100ns = unsafe { sample.GetSampleTime() }.unwrap_or(0).max(0) as u64;
        let captured_at = self.started + Duration::from_nanos(sample_time_100ns * 100);

        Ok(EncodedFrame {
            data,
            keyframe,
            ts: Instant::now(),
            captured_at,
        })
    }

    /// Builds an input `IMFSample` from `frame`'s NV12 pixels, stamped with
    /// `frame.ts()` (relative to `self.started`) as its sample time.
    fn build_input_sample(&mut self, frame: &RawFrame) -> Result<IMFSample, EncodeError> {
        let len = nv12_len(frame);
        let total_len = len as u32;

        // SAFETY: `total_len` is a plain size argument.
        let buffer = unsafe { MFCreateMemoryBuffer(total_len) }.map_err(|err| {
            EncodeError::Backend(format!("MediaFoundation MFCreateMemoryBuffer: {err}"))
        })?;

        let mut ptr: *mut u8 = std::ptr::null_mut();
        // SAFETY: `ptr` is a valid stack out-parameter; the buffer was just
        // created above and isn't shared with anything else yet.
        unsafe { buffer.Lock(&mut ptr, None, None) }.map_err(|err| {
            EncodeError::Backend(format!("MediaFoundation input IMFMediaBuffer::Lock: {err}"))
        })?;
        // SAFETY: `Lock` returned success just above, so `ptr` is non-null
        // and valid for writes of at least `total_len == len` bytes (the
        // buffer was created with that capacity) until `Unlock`; the buffer
        // is brand new and unshared, so nothing else aliases the slice.
        let dst = unsafe { std::slice::from_raw_parts_mut(ptr, len) };
        self.nv12_writer.write(frame, dst);
        // SAFETY: matches the successful `Lock` above.
        unsafe { buffer.Unlock() }.map_err(|err| {
            EncodeError::Backend(format!(
                "MediaFoundation input IMFMediaBuffer::Unlock: {err}"
            ))
        })?;
        // SAFETY: `buffer` is valid; `total_len` matches what was just
        // written into it.
        unsafe { buffer.SetCurrentLength(total_len) }.map_err(|err| {
            EncodeError::Backend(format!(
                "MediaFoundation input IMFMediaBuffer::SetCurrentLength: {err}"
            ))
        })?;

        // SAFETY: no preconditions beyond MF being started (done in `new`).
        let sample = unsafe { MFCreateSample() }.map_err(|err| {
            EncodeError::Backend(format!("MediaFoundation MFCreateSample: {err}"))
        })?;
        // SAFETY: `sample` and `buffer` were both just created above.
        unsafe { sample.AddBuffer(&buffer) }.map_err(|err| {
            EncodeError::Backend(format!("MediaFoundation IMFSample::AddBuffer: {err}"))
        })?;

        let pts_100ns = frame
            .ts()
            .saturating_duration_since(self.started)
            .as_nanos()
            / 100;
        // SAFETY: `sample` is valid.
        unsafe { sample.SetSampleTime(pts_100ns as i64) }.map_err(|err| {
            EncodeError::Backend(format!("MediaFoundation IMFSample::SetSampleTime: {err}"))
        })?;
        let duration_100ns = 10_000_000 / self.cfg.fps.max(1);
        // SAFETY: same sample.
        unsafe { sample.SetSampleDuration(i64::from(duration_100ns)) }.map_err(|err| {
            EncodeError::Backend(format!(
                "MediaFoundation IMFSample::SetSampleDuration: {err}"
            ))
        })?;

        Ok(sample)
    }

    /// Calls `ProcessOutput` once, handling a format-change status by
    /// re-negotiating the output type and retrying. `Ok(None)` covers both
    /// "no output yet" (`MF_E_TRANSFORM_NEED_MORE_INPUT`) and "the MFT
    /// reported success with no sample" -- neither is an error.
    fn drain_output(&mut self) -> Result<Option<EncodedFrame>, EncodeError> {
        loop {
            let provided_sample = if self.provides_samples {
                None
            } else {
                if self.output_cache.is_none() {
                    // SAFETY: `output_buffer_size` was read from
                    // `GetOutputStreamInfo` (or is the frame's raw pixel
                    // count, whichever is larger) at construction time.
                    let buffer = unsafe { MFCreateMemoryBuffer(self.output_buffer_size) }.map_err(
                        |err| {
                            EncodeError::Backend(format!(
                                "MediaFoundation MFCreateMemoryBuffer (output): {err}"
                            ))
                        },
                    )?;
                    // SAFETY: no preconditions beyond MF being started.
                    let sample = unsafe { MFCreateSample() }.map_err(|err| {
                        EncodeError::Backend(format!(
                            "MediaFoundation MFCreateSample (output): {err}"
                        ))
                    })?;
                    // SAFETY: both just created above.
                    unsafe { sample.AddBuffer(&buffer) }.map_err(|err| {
                        EncodeError::Backend(format!(
                            "MediaFoundation IMFSample::AddBuffer (output): {err}"
                        ))
                    })?;
                    self.output_cache = Some((sample, buffer));
                }
                let Some((sample, buffer)) = self.output_cache.as_ref() else {
                    unreachable!("output_cache was just filled above");
                };
                // SAFETY: `buffer` is valid and not locked (every `Lock` in
                // this module is paired with an `Unlock` before returning);
                // the previous output was already copied out by
                // `frame_from_output_sample` before this call, so nothing
                // reads the old contents.
                unsafe { buffer.SetCurrentLength(0) }.map_err(|err| {
                    EncodeError::Backend(format!(
                        "MediaFoundation IMFMediaBuffer::SetCurrentLength (output): {err}"
                    ))
                })?;
                // SAFETY: `sample` is valid. Clears attributes the MFT set on
                // the previous frame (e.g. `MFSampleExtension_CleanPoint`),
                // which would otherwise leak into this frame's keyframe
                // detection.
                unsafe { sample.DeleteAllItems() }.map_err(|err| {
                    EncodeError::Backend(format!(
                        "MediaFoundation IMFSample::DeleteAllItems (output): {err}"
                    ))
                })?;
                // The slot gets a clone (AddRef); `ManuallyDrop::take` below
                // releases that clone, the cache keeps its own reference.
                Some(sample.clone())
            };

            let mut buffers = [MFT_OUTPUT_DATA_BUFFER {
                dwStreamID: 0,
                pSample: ManuallyDrop::new(provided_sample),
                dwStatus: 0,
                pEvents: ManuallyDrop::new(None),
            }];
            let mut status: u32 = 0;
            // SAFETY: `buffers` is a single well-formed
            // `MFT_OUTPUT_DATA_BUFFER`; `status` is a valid stack
            // out-parameter.
            let result = unsafe { self.transform.ProcessOutput(0, &mut buffers, &mut status) };
            let format_change =
                buffers[0].dwStatus & MFT_OUTPUT_DATA_BUFFER_FORMAT_CHANGE.0 as u32 != 0;
            // SAFETY: `ProcessOutput` either filled/returned the sample we
            // handed it or left the slot untouched -- either way we own
            // whatever is in it now (ours, or a fresh one the MFT
            // allocated) and must run its `Drop` (releasing the COM
            // reference) exactly once instead of leaking it, regardless of
            // `result`.
            let output_sample = unsafe { ManuallyDrop::take(&mut buffers[0].pSample) };
            // SAFETY: same reasoning, for the (always unused here) events
            // collection slot.
            drop(unsafe { ManuallyDrop::take(&mut buffers[0].pEvents) });

            let real_error = result
                .as_ref()
                .is_err_and(|err| err.code() != MF_E_TRANSFORM_NEED_MORE_INPUT);
            if format_change || real_error {
                // The buffer may be too small / in an unknown state now;
                // recreate on the next call (safer than reasoning about it).
                // `MF_E_TRANSFORM_NEED_MORE_INPUT` -- the usual end of every
                // drain -- leaves the buffer untouched, so it stays cached.
                self.output_cache = None;
            }

            match result {
                Ok(()) => {
                    if format_change {
                        drop(output_sample);
                        self.refresh_output_type()?;
                        continue;
                    }
                    return match output_sample {
                        Some(sample) => self.frame_from_output_sample(&sample).map(Some),
                        None => Ok(None),
                    };
                }
                Err(err) if err.code() == MF_E_TRANSFORM_NEED_MORE_INPUT => return Ok(None),
                Err(err) => {
                    return Err(EncodeError::Backend(format!(
                        "MediaFoundation ProcessOutput: {err}"
                    )))
                }
            }
        }
    }

    /// Re-reads the MFT's (now different) preferred output type after a
    /// `MFT_OUTPUT_DATA_BUFFER_FORMAT_CHANGE` status and applies it.
    fn refresh_output_type(&self) -> Result<(), EncodeError> {
        // SAFETY: `self.transform` is valid and was already producing
        // output, so an available output type at index 0 must exist.
        let media_type = unsafe { self.transform.GetOutputAvailableType(0, 0) }.map_err(|err| {
            EncodeError::Backend(format!(
                "MediaFoundation GetOutputAvailableType (format change): {err}"
            ))
        })?;
        // SAFETY: `media_type` was just obtained from the same transform.
        unsafe { self.transform.SetOutputType(0, &media_type, 0) }.map_err(|err| {
            EncodeError::Backend(format!(
                "MediaFoundation SetOutputType (format change): {err}"
            ))
        })
    }

    fn encode_sync(&mut self, sample: IMFSample) -> Result<Option<EncodedFrame>, EncodeError> {
        // SAFETY: `sample` was just built by `build_input_sample`.
        let result = unsafe { self.transform.ProcessInput(0, &sample, 0) };
        if let Err(err) = result {
            if err.code() == MF_E_NOTACCEPTING {
                // The MFT's internal queue is full: drain whatever output is
                // pending (queued rather than dropped, since it belongs to
                // an earlier frame) and retry submitting this one once.
                if let Some(frame) = self.drain_output()? {
                    self.pending.push_back(frame);
                }
                // SAFETY: same as above.
                unsafe { self.transform.ProcessInput(0, &sample, 0) }.map_err(|err| {
                    EncodeError::Backend(format!(
                        "MediaFoundation ProcessInput (retry after NOTACCEPTING): {err}"
                    ))
                })?;
            } else {
                return Err(EncodeError::Backend(format!(
                    "MediaFoundation ProcessInput: {err}"
                )));
            }
        }

        if let Some(frame) = self.pending.pop_front() {
            return Ok(Some(frame));
        }
        self.drain_output()
    }

    /// Async-MFT submit path. Hardware MFTs may deliver `METransformHaveOutput`
    /// before the next `METransformNeedInput`, so `ProcessInput` is called only
    /// once `wait_for_credit` has secured a credit (otherwise `MF_E_NOTACCEPTING`).
    fn encode_async(&mut self, sample: IMFSample) -> Result<Option<EncodedFrame>, EncodeError> {
        self.drain_events_no_wait()?;
        self.wait_for_credit()?;
        // SAFETY: `sample` was just built by `build_input_sample`.
        unsafe { self.transform.ProcessInput(0, &sample, 0) }.map_err(|err| {
            EncodeError::Backend(format!("MediaFoundation ProcessInput (async): {err}"))
        })?;
        self.credits = self.credits.saturating_sub(1);
        self.in_flight += 1;
        self.wait_for_output()?;
        if self.pending.len() > 1 && !self.backlog_warned {
            tracing::warn!(
                pending = self.pending.len(),
                "async MFT output backlog: encoder output lags input"
            );
            self.backlog_warned = true;
        }
        Ok(self.pending.pop_front())
    }

    /// Waits (bounded by `OUTPUT_WAIT_TIMEOUT`) until every submitted input has
    /// produced its output, handling every event that arrives meanwhile, so the
    /// frame returned by `encode()` is the one just submitted rather than its
    /// predecessor. Expiry is not an error: the output simply surfaces on a
    /// later call, as it did before this wait existed.
    fn wait_for_output(&mut self) -> Result<(), EncodeError> {
        let Some(events) = self.events.clone() else {
            return Ok(());
        };
        let deadline = Instant::now() + OUTPUT_WAIT_TIMEOUT;
        while self.in_flight > 0 {
            if Instant::now() >= deadline {
                // Resync: a `METransformHaveOutput` that yielded no sample
                // never decrements `in_flight`, and a stale count would make
                // every later call wait the full timeout.
                self.in_flight = 0;
                return Ok(());
            }
            // SAFETY: same as `wait_for_credit`; `MF_EVENT_FLAG_NO_WAIT` never
            // blocks.
            match unsafe { events.GetEvent(MF_EVENT_FLAG_NO_WAIT) } {
                Ok(event) => self.handle_event(&event)?,
                Err(err) if err.code() == MF_E_NO_EVENTS_AVAILABLE => {
                    std::thread::sleep(Duration::from_millis(1));
                }
                Err(err) => {
                    return Err(EncodeError::Backend(format!(
                        "MediaFoundation GetEvent: {err}"
                    )));
                }
            }
        }
        Ok(())
    }

    /// Drains every event currently queued (`MF_EVENT_FLAG_NO_WAIT`) without
    /// blocking; returns as soon as `GetEvent` reports none available.
    fn drain_events_no_wait(&mut self) -> Result<(), EncodeError> {
        loop {
            let Some(events) = self.events.clone() else {
                return Ok(());
            };
            // SAFETY: `events` is a live `IMFMediaEventGenerator` cast from
            // `self.transform`, valid for the encoder's whole lifetime.
            let event = match unsafe { events.GetEvent(MF_EVENT_FLAG_NO_WAIT) } {
                Ok(event) => event,
                // No further events queued right now (typically
                // `MF_E_NO_EVENTS_AVAILABLE`) -- not an error.
                Err(_) => return Ok(()),
            };
            self.handle_event(&event)?;
        }
    }

    /// Waits (bounded by `CREDIT_WAIT_TIMEOUT`) until at least one
    /// `METransformNeedInput` credit is available, handling every event that
    /// arrives meanwhile. A hardware MFT can emit `METransformHaveOutput`
    /// before `NeedInput`, so waiting for a single event is not enough.
    fn wait_for_credit(&mut self) -> Result<(), EncodeError> {
        let Some(events) = self.events.clone() else {
            return Ok(());
        };
        let deadline = Instant::now() + CREDIT_WAIT_TIMEOUT;
        while self.credits == 0 {
            if Instant::now() >= deadline {
                return Err(EncodeError::Backend(format!(
                    "MediaFoundation: no METransformNeedInput within {:?} (async MFT starved of input credits)",
                    CREDIT_WAIT_TIMEOUT
                )));
            }
            // SAFETY: same as `drain_events_no_wait`; `MF_EVENT_FLAG_NO_WAIT`
            // never blocks.
            match unsafe { events.GetEvent(MF_EVENT_FLAG_NO_WAIT) } {
                Ok(event) => self.handle_event(&event)?,
                Err(err) if err.code() == MF_E_NO_EVENTS_AVAILABLE => {
                    std::thread::sleep(Duration::from_millis(1));
                }
                Err(err) => {
                    return Err(EncodeError::Backend(format!(
                        "MediaFoundation GetEvent: {err}"
                    )));
                }
            }
        }
        Ok(())
    }

    fn handle_event(&mut self, event: &IMFMediaEvent) -> Result<(), EncodeError> {
        // SAFETY: `event` is a live `IMFMediaEvent` just returned by
        // `GetEvent`.
        let event_type = unsafe { event.GetType() }.map_err(|err| {
            EncodeError::Backend(format!("MediaFoundation IMFMediaEvent::GetType: {err}"))
        })?;
        if event_type == METransformNeedInput.0 as u32 {
            self.credits += 1;
        } else if event_type == METransformHaveOutput.0 as u32 {
            if let Some(frame) = self.drain_output()? {
                self.in_flight = self.in_flight.saturating_sub(1);
                self.pending.push_back(frame);
            }
        }
        Ok(())
    }
}

impl Encoder for MediaFoundationEncoder {
    fn encode(
        &mut self,
        frame: &RawFrame,
        force_keyframe: bool,
    ) -> Result<Option<EncodedFrame>, EncodeError> {
        let (width, height) = frame_dims(frame);
        if width != self.cfg.width || height != self.cfg.height {
            return Err(EncodeError::Backend(format!(
                "MediaFoundation encoder configured for {}x{}, got a {}x{} frame",
                self.cfg.width, self.cfg.height, width, height
            )));
        }

        let sample = self.build_input_sample(frame)?;

        if force_keyframe {
            if let Some(codec_api) = &self.codec_api {
                soft_set_codec_value(
                    codec_api,
                    "AVEncVideoForceKeyFrame",
                    &CODECAPI_AVEncVideoForceKeyFrame,
                    &variant_u32(1),
                );
            }
        }

        self.check_bitrate_drift();

        if self.is_async {
            self.encode_async(sample)
        } else {
            self.encode_sync(sample)
        }
    }

    /// Changes the mean bitrate. fps is part of the MFT's negotiated media
    /// type (`MF_MT_FRAME_RATE`, set once in `set_input_type`/
    /// `set_output_type`) and cannot be changed on a streaming MFT without a
    /// full re-negotiation. The MFT budgets a frame as
    /// `MeanBitRate / MF_MT_FRAME_RATE`, so `target.fps` is taken into account
    /// by compensating the bitrate (see `fps_compensated_bitrate_bps`).
    fn set_rate(&mut self, target: RateTarget) -> Result<(), EncodeError> {
        let Some(codec_api) = &self.codec_api else {
            if !self.rate_warned {
                tracing::warn!(
                    "MediaFoundation encoder has no ICodecAPI, cannot change bitrate at runtime"
                );
                self.rate_warned = true;
            }
            return Ok(());
        };

        let bitrate_bps =
            fps_compensated_bitrate_bps(target.bitrate_kbps, self.cfg.fps, target.fps);
        // SAFETY: `codec_api` is a live `ICodecAPI` obtained at construction
        // time; `CODECAPI_AVEncCommonMeanBitRate` is an extern immutable
        // static of the documented `GUID` type; `variant_u32` builds a live
        // `VARIANT` kept alive through the call.
        unsafe {
            codec_api
                .SetValue(&CODECAPI_AVEncCommonMeanBitRate, &variant_u32(bitrate_bps))
                .map_err(|err| {
                    EncodeError::Backend(format!(
                        "MediaFoundation CODECAPI_AVEncCommonMeanBitRate (set_rate): {err}"
                    ))
                })?;
        }

        let readback_bps = read_codec_u32(codec_api, &CODECAPI_AVEncCommonMeanBitRate);
        tracing::debug!(
            bitrate_kbps = target.bitrate_kbps,
            fps = target.fps,
            cfg_fps = self.cfg.fps,
            mean_bitrate_bps = bitrate_bps,
            readback_bps = ?readback_bps,
            "media foundation rate target"
        );
        self.last_set_bitrate_bps = bitrate_bps;
        self.last_seen_bitrate_bps = readback_bps;
        self.drift_checked_at = Instant::now();

        Ok(())
    }
}

impl MediaFoundationEncoder {
    /// Reads `MeanBitRate` back at most once per `DRIFT_CHECK_INTERVAL` and
    /// logs (once per distinct value) when it differs from what we last set.
    fn check_bitrate_drift(&mut self) {
        if self.drift_checked_at.elapsed() < DRIFT_CHECK_INTERVAL {
            return;
        }
        self.drift_checked_at = Instant::now();
        let Some(codec_api) = &self.codec_api else {
            return;
        };
        let Some(current) = read_codec_u32(codec_api, &CODECAPI_AVEncCommonMeanBitRate) else {
            return;
        };
        if current != self.last_set_bitrate_bps && self.last_seen_bitrate_bps != Some(current) {
            tracing::info!(
                set_bps = self.last_set_bitrate_bps,
                readback_bps = current,
                previous_readback_bps = ?self.last_seen_bitrate_bps,
                "media foundation mean bitrate drifted"
            );
        }
        self.last_seen_bitrate_bps = Some(current);
    }
}

impl Drop for MediaFoundationEncoder {
    fn drop(&mut self) {
        // SAFETY: `self.transform` was successfully created in `new()`;
        // failures here are logged-and-ignored, best-effort shutdown (same
        // pattern as the rest of this module's soft-failure helpers) --
        // there is no useful recovery once the encoder is being dropped.
        unsafe {
            if let Err(err) = self
                .transform
                .ProcessMessage(MFT_MESSAGE_NOTIFY_END_OF_STREAM, 0)
            {
                tracing::warn!(error = %err, "MediaFoundation NOTIFY_END_OF_STREAM failed during shutdown");
            }
            if let Err(err) = self.transform.ProcessMessage(MFT_MESSAGE_COMMAND_FLUSH, 0) {
                tracing::warn!(error = %err, "MediaFoundation COMMAND_FLUSH failed during shutdown");
            }
        }
        // `MFShutdown` is done by the `_mf` field (`MfSession`) after the
        // fields above are released.
    }
}

fn frame_dims(frame: &RawFrame) -> (u32, u32) {
    match frame {
        RawFrame::Nv12 { width, height, .. } | RawFrame::Bgra { width, height, .. } => {
            (*width, *height)
        }
    }
}

/// Enumerates `MFT_CATEGORY_VIDEO_ENCODER` MFTs matching `input_type`
/// (NV12) -> `output_type` (H.264) with the given `flags`, taking ownership
/// of every non-null `IMFActivate` the OS hands back and freeing the
/// `CoTaskMemAlloc`'d array itself.
fn enumerate_mfts(
    flags: MFT_ENUM_FLAG,
    input_type: &MFT_REGISTER_TYPE_INFO,
    output_type: &MFT_REGISTER_TYPE_INFO,
) -> Result<Vec<IMFActivate>, EncodeError> {
    let mut activates_ptr: *mut Option<IMFActivate> = std::ptr::null_mut();
    let mut count: u32 = 0;
    // SAFETY: `input_type`/`output_type` are valid for the duration of the
    // call; `activates_ptr`/`count` are valid stack out-parameters.
    unsafe {
        MFTEnumEx(
            MFT_CATEGORY_VIDEO_ENCODER,
            flags,
            Some(std::ptr::from_ref(input_type)),
            Some(std::ptr::from_ref(output_type)),
            &mut activates_ptr,
            &mut count,
        )
    }
    .map_err(|err| EncodeError::Backend(format!("MediaFoundation MFTEnumEx: {err}")))?;

    if activates_ptr.is_null() || count == 0 {
        if !activates_ptr.is_null() {
            // SAFETY: `MFTEnumEx` allocated this array via `CoTaskMemAlloc`
            // per its documented contract, even though it holds zero usable
            // entries here.
            unsafe { CoTaskMemFree(Some(activates_ptr.cast())) };
        }
        return Ok(Vec::new());
    }

    let mut result = Vec::with_capacity(count as usize);
    for i in 0..count as usize {
        // SAFETY: `activates_ptr` points at `count` contiguous
        // `Option<IMFActivate>` slots allocated by `MFTEnumEx`; `ptr::read`
        // takes ownership of the `i`-th one (its +1 COM reference) without
        // running `IMFActivate::drop` on the source, so no reference is
        // released twice -- the array's own memory is freed as a raw block
        // right after this loop, not per-element.
        let entry = unsafe { std::ptr::read(activates_ptr.add(i)) };
        if let Some(activate) = entry {
            result.push(activate);
        }
    }
    // SAFETY: matches the successful `MFTEnumEx` call above; every element
    // has already been moved out via `ptr::read`.
    unsafe { CoTaskMemFree(Some(activates_ptr.cast())) };

    Ok(result)
}

/// Reads `MFT_FRIENDLY_NAME_Attribute` off a freshly enumerated
/// `IMFActivate`, purely for the startup log line -- any failure (missing
/// attribute, allocation failure) is reported as `"<unknown>"` rather than
/// failing encoder construction.
fn read_friendly_name(activate: &IMFActivate) -> String {
    let mut pwstr = PWSTR::null();
    let mut len: u32 = 0;
    // SAFETY: `pwstr`/`len` are valid stack out-parameters; `activate` is a
    // live `IMFActivate` (inherits `IMFAttributes` via `Deref`).
    let result =
        unsafe { activate.GetAllocatedString(&MFT_FRIENDLY_NAME_Attribute, &mut pwstr, &mut len) };
    if result.is_err() || pwstr.is_null() {
        return "<unknown>".to_string();
    }
    // SAFETY: `GetAllocatedString` returned success, so `pwstr` points at a
    // `CoTaskMemAlloc`'d, null-terminated UTF-16 string this function now
    // owns.
    let name = unsafe { pwstr.to_string() }.unwrap_or_else(|_| "<unknown>".to_string());
    // SAFETY: `pwstr` was allocated by `GetAllocatedString` via
    // `CoTaskMemAlloc`, per its documented contract.
    unsafe { CoTaskMemFree(Some(pwstr.0.cast())) };
    name
}

/// Detects whether `transform` is an async MFT (`MF_TRANSFORM_ASYNC == 1` in
/// its attributes) and, if so, sets `MF_TRANSFORM_ASYNC_UNLOCK` -- required
/// before an async MFT will accept any further calls. A `GetAttributes`
/// failure means a synchronous MFT (it has no attribute store to ask).
fn unlock_async(transform: &IMFTransform) -> Result<bool, EncodeError> {
    // SAFETY: `transform` was just activated.
    let attributes = match unsafe { transform.GetAttributes() } {
        Ok(attributes) => attributes,
        Err(_) => return Ok(false),
    };
    // SAFETY: `attributes` is a live `IMFAttributes`.
    let is_async = unsafe { attributes.GetUINT32(&MF_TRANSFORM_ASYNC) }.unwrap_or(0) == 1;
    if is_async {
        // SAFETY: same attributes store.
        unsafe { attributes.SetUINT32(&MF_TRANSFORM_ASYNC_UNLOCK, 1) }.map_err(|err| {
            EncodeError::Backend(format!("MediaFoundation MF_TRANSFORM_ASYNC_UNLOCK: {err}"))
        })?;
    }
    Ok(is_async)
}

/// Sets every `ICodecAPI` rate-control/latency property this encoder needs,
/// before `SetOutputType`. Returns `Ok(None)` (not an error) when the MFT
/// has no `ICodecAPI` at all -- some software MFTs don't implement it.
/// `CommonRateControlMode`/`CommonMeanBitRate` failures are treated as fatal
/// (without them the encoder won't hit the configured bitrate at all);
/// every other property is best-effort (`tracing::warn!` and continue).
fn configure_codec_api(
    transform: &IMFTransform,
    cfg: &EncoderConfig,
) -> Result<Option<ICodecAPI>, EncodeError> {
    let codec_api = match transform.cast::<ICodecAPI>() {
        Ok(codec_api) => codec_api,
        Err(err) => {
            tracing::warn!(
                error = %err,
                "MediaFoundation encoder has no ICodecAPI, skipping rate-control tuning"
            );
            return Ok(None);
        }
    };

    // SAFETY (this whole block): `codec_api` is a live `ICodecAPI` cast from
    // `transform`; every `SetValue` key is an extern immutable static of the
    // documented `GUID` type, and every value is a `VARIANT` built by
    // `variant_u32`/`variant_bool` right at the call site (temporary
    // lifetime extension keeps it alive through the call).
    unsafe {
        codec_api
            .SetValue(
                &CODECAPI_AVEncCommonRateControlMode,
                &variant_u32(eAVEncCommonRateControlMode_CBR.0 as u32),
            )
            .map_err(|err| {
                EncodeError::Backend(format!(
                    "MediaFoundation CODECAPI_AVEncCommonRateControlMode: {err}"
                ))
            })?;

        let bitrate_bps = cfg.bitrate_kbps.saturating_mul(1000);
        codec_api
            .SetValue(&CODECAPI_AVEncCommonMeanBitRate, &variant_u32(bitrate_bps))
            .map_err(|err| {
                EncodeError::Backend(format!(
                    "MediaFoundation CODECAPI_AVEncCommonMeanBitRate: {err}"
                ))
            })?;
    }

    soft_set_codec_value(
        &codec_api,
        "AVLowLatencyMode",
        &CODECAPI_AVLowLatencyMode,
        &variant_bool(true),
    );
    soft_set_codec_value(
        &codec_api,
        "AVEncMPVGOPSize",
        &CODECAPI_AVEncMPVGOPSize,
        &variant_u32(cfg.keyframe_interval_frames),
    );
    if let Some(max_qp) = cfg.max_qp {
        soft_set_codec_value(
            &codec_api,
            "AVEncVideoMaxQP",
            &CODECAPI_AVEncVideoMaxQP,
            &variant_u32(u32::from(max_qp)),
        );
    }

    Ok(Some(codec_api))
}

/// Like a direct `SetValue` call, but a failure is only `tracing::warn!`-ed,
/// not propagated -- for properties whose absence degrades the encoder
/// (default GOP size, no forced low-latency mode, no QP ceiling, a missed
/// forced keyframe) rather than breaking it.
fn soft_set_codec_value(codec_api: &ICodecAPI, name: &str, key: &GUID, value: &VARIANT) {
    // SAFETY: `codec_api` is a live `ICodecAPI`; `key` is an extern
    // immutable static; `value` is a live `VARIANT` built by the caller.
    if let Err(err) = unsafe { codec_api.SetValue(key, value) } {
        tracing::warn!(
            name,
            error = %err,
            "MediaFoundation codec property not accepted, continuing without it"
        );
    }
}

fn variant_u32(value: u32) -> VARIANT {
    VARIANT {
        Anonymous: VARIANT_0 {
            Anonymous: ManuallyDrop::new(VARIANT_0_0 {
                vt: VT_UI4,
                wReserved1: 0,
                wReserved2: 0,
                wReserved3: 0,
                Anonymous: VARIANT_0_0_0 { ulVal: value },
            }),
        },
    }
}

/// Reads an unsigned integer out of a `VARIANT` of type `VT_UI4`, `VT_UI8` or
/// (non-negative) `VT_I4`; `None` for any other type.
fn variant_to_u64(value: &VARIANT) -> Option<u64> {
    // SAFETY: `value` is a live `VARIANT`; `vt` is the discriminant of the
    // union `Anonymous`, and each union field is read only when `vt` names
    // exactly that field's type.
    unsafe {
        let inner = &value.Anonymous.Anonymous;
        if inner.vt == VT_UI4 {
            Some(u64::from(inner.Anonymous.ulVal))
        } else if inner.vt == VT_UI8 {
            Some(inner.Anonymous.ullVal)
        } else if inner.vt == VT_I4 {
            u64::try_from(inner.Anonymous.lVal).ok()
        } else {
            None
        }
    }
}

/// `ICodecAPI::GetValue` as `Option<u32>`: `None` if the call fails or the
/// value is not an integer fitting `u32`. Diagnostics only, never fatal.
fn read_codec_u32(codec_api: &ICodecAPI, key: &GUID) -> Option<u32> {
    // SAFETY: `codec_api` is a live `ICodecAPI`; `key` is an extern immutable
    // static (or other live `GUID`); the returned `VARIANT` is owned here and
    // holds only a plain integer for the types `variant_to_u64` accepts.
    let value = unsafe { codec_api.GetValue(key) }.ok()?;
    variant_to_u64(&value).and_then(|v| u32::try_from(v).ok())
}

/// One info line with the rate-control state the MFT reports after
/// configuration (V-4b diagnostics). Every value is best-effort.
fn log_codec_readback(codec_api: Option<&ICodecAPI>, is_async: bool, fps: u32) {
    let get = |key: &GUID| codec_api.and_then(|api| read_codec_u32(api, key));
    tracing::info!(
        rate_control_mode = ?get(&CODECAPI_AVEncCommonRateControlMode),
        mean_bitrate_bps = ?get(&CODECAPI_AVEncCommonMeanBitRate),
        buffer_size = ?get(&CODECAPI_AVEncCommonBufferSize),
        is_async,
        cfg_fps = fps,
        "media foundation codec readback"
    );
}

fn variant_bool(value: bool) -> VARIANT {
    VARIANT {
        Anonymous: VARIANT_0 {
            Anonymous: ManuallyDrop::new(VARIANT_0_0 {
                vt: VT_BOOL,
                wReserved1: 0,
                wReserved2: 0,
                wReserved3: 0,
                Anonymous: VARIANT_0_0_0 {
                    boolVal: if value { VARIANT_TRUE } else { VARIANT_FALSE },
                },
            }),
        },
    }
}

/// Builds and applies the H.264 output media type: Constrained Baseline
/// profile first, falling back to plain Baseline (with a `tracing::warn!`)
/// if the MFT rejects it -- Microsoft's own software encoder MFT documents
/// only Base/Main/High, not Constrained Baseline, on some Windows versions.
fn set_output_type(transform: &IMFTransform, cfg: &EncoderConfig) -> Result<(), EncodeError> {
    // SAFETY: no preconditions.
    let media_type = unsafe { MFCreateMediaType() }.map_err(|err| {
        EncodeError::Backend(format!("MediaFoundation MFCreateMediaType (output): {err}"))
    })?;

    let bitrate_bps = cfg.bitrate_kbps.saturating_mul(1000);
    // SAFETY (this block): `media_type` was just created above; every key
    // is an extern immutable static of the documented `GUID` type.
    unsafe {
        set_guid(
            &media_type,
            "MF_MT_MAJOR_TYPE",
            &MF_MT_MAJOR_TYPE,
            &MFMediaType_Video,
        )?;
        set_guid(
            &media_type,
            "MF_MT_SUBTYPE",
            &MF_MT_SUBTYPE,
            &MFVideoFormat_H264,
        )?;
        media_type
            .SetUINT32(&MF_MT_AVG_BITRATE, bitrate_bps)
            .map_err(|err| {
                EncodeError::Backend(format!("MediaFoundation MF_MT_AVG_BITRATE: {err}"))
            })?;
        media_type
            .SetUINT64(&MF_MT_FRAME_SIZE, frame_size_value(cfg.width, cfg.height))
            .map_err(|err| {
                EncodeError::Backend(format!("MediaFoundation MF_MT_FRAME_SIZE: {err}"))
            })?;
        media_type
            .SetUINT64(&MF_MT_FRAME_RATE, frame_rate_value(cfg.fps))
            .map_err(|err| {
                EncodeError::Backend(format!("MediaFoundation MF_MT_FRAME_RATE: {err}"))
            })?;
        media_type
            .SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32)
            .map_err(|err| {
                EncodeError::Backend(format!("MediaFoundation MF_MT_INTERLACE_MODE: {err}"))
            })?;
        media_type
            .SetUINT32(
                &MF_MT_MPEG2_PROFILE,
                eAVEncH264VProfile_ConstrainedBase.0 as u32,
            )
            .map_err(|err| {
                EncodeError::Backend(format!("MediaFoundation MF_MT_MPEG2_PROFILE: {err}"))
            })?;
    }

    // SAFETY: `media_type` is fully populated above.
    match unsafe { transform.SetOutputType(0, &media_type, 0) } {
        Ok(()) => Ok(()),
        Err(err) => {
            tracing::warn!(
                error = %err,
                "MediaFoundation encoder rejected Constrained Baseline profile, retrying with Base"
            );
            // SAFETY: `media_type` is still a live, valid media type; only
            // the profile attribute changes.
            unsafe {
                media_type
                    .SetUINT32(&MF_MT_MPEG2_PROFILE, eAVEncH264VProfile_Base.0 as u32)
                    .map_err(|err| {
                        EncodeError::Backend(format!(
                            "MediaFoundation MF_MT_MPEG2_PROFILE (Base): {err}"
                        ))
                    })?;
            }
            // SAFETY: same as above.
            unsafe { transform.SetOutputType(0, &media_type, 0) }.map_err(|err| {
                EncodeError::Backend(format!("MediaFoundation SetOutputType: {err}"))
            })
        }
    }
}

/// Finds an NV12 input type among `transform`'s `GetInputAvailableType`
/// list (filling in size/rate/interlace/independence on it), or -- if the
/// MFT has no available input types before its output type is set -- builds
/// one from scratch.
fn set_input_type(transform: &IMFTransform, cfg: &EncoderConfig) -> Result<(), EncodeError> {
    let mut found: Option<IMFMediaType> = None;
    let mut index = 0u32;
    loop {
        // SAFETY: `transform` is valid; an out-of-range `index` surfaces as
        // `Err`, ending the loop, not undefined behaviour.
        let candidate = match unsafe { transform.GetInputAvailableType(0, index) } {
            Ok(candidate) => candidate,
            Err(_) => break,
        };
        // SAFETY: `candidate` is a live `IMFMediaType`.
        let subtype = unsafe { candidate.GetGUID(&MF_MT_SUBTYPE) }.unwrap_or_default();
        if subtype == MFVideoFormat_NV12 {
            found = Some(candidate);
            break;
        }
        index += 1;
    }

    let media_type = match found {
        Some(media_type) => media_type,
        None => {
            // SAFETY: no preconditions.
            let media_type = unsafe { MFCreateMediaType() }.map_err(|err| {
                EncodeError::Backend(format!("MediaFoundation MFCreateMediaType (input): {err}"))
            })?;
            // SAFETY: `media_type` was just created above.
            unsafe {
                set_guid(
                    &media_type,
                    "MF_MT_MAJOR_TYPE",
                    &MF_MT_MAJOR_TYPE,
                    &MFMediaType_Video,
                )?;
                set_guid(
                    &media_type,
                    "MF_MT_SUBTYPE",
                    &MF_MT_SUBTYPE,
                    &MFVideoFormat_NV12,
                )?;
            }
            media_type
        }
    };

    // SAFETY (this block): `media_type` is a live, owned or freshly-built
    // `IMFMediaType`.
    unsafe {
        media_type
            .SetUINT64(&MF_MT_FRAME_SIZE, frame_size_value(cfg.width, cfg.height))
            .map_err(|err| {
                EncodeError::Backend(format!("MediaFoundation MF_MT_FRAME_SIZE (input): {err}"))
            })?;
        media_type
            .SetUINT64(&MF_MT_FRAME_RATE, frame_rate_value(cfg.fps))
            .map_err(|err| {
                EncodeError::Backend(format!("MediaFoundation MF_MT_FRAME_RATE (input): {err}"))
            })?;
        media_type
            .SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32)
            .map_err(|err| {
                EncodeError::Backend(format!(
                    "MediaFoundation MF_MT_INTERLACE_MODE (input): {err}"
                ))
            })?;
        media_type
            .SetUINT32(&MF_MT_ALL_SAMPLES_INDEPENDENT, 1)
            .map_err(|err| {
                EncodeError::Backend(format!(
                    "MediaFoundation MF_MT_ALL_SAMPLES_INDEPENDENT: {err}"
                ))
            })?;
    }

    // SAFETY: `media_type` is fully populated above.
    unsafe { transform.SetInputType(0, &media_type, 0) }
        .map_err(|err| EncodeError::Backend(format!("MediaFoundation SetInputType: {err}")))
}

/// # Safety
///
/// Caller must ensure `media_type` is a valid, live `IMFMediaType`.
unsafe fn set_guid(
    media_type: &IMFMediaType,
    name: &str,
    key: &GUID,
    value: &GUID,
) -> Result<(), EncodeError> {
    // SAFETY: forwarded from this function's own safety contract.
    unsafe { media_type.SetGUID(key, value) }
        .map_err(|err| EncodeError::Backend(format!("MediaFoundation {name}: {err}")))
}

fn frame_size_value(width: u32, height: u32) -> u64 {
    (u64::from(width) << 32) | u64::from(height)
}

fn frame_rate_value(fps: u32) -> u64 {
    (u64::from(fps) << 32) | 1
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::capture::synthetic::SyntheticSource;
    use crate::capture::FrameSource;

    #[test]
    fn variant_to_u64_reads_integers_and_rejects_others() {
        assert_eq!(variant_to_u64(&variant_u32(12_000_000)), Some(12_000_000));
        assert_eq!(variant_to_u64(&variant_bool(true)), None);
    }

    fn test_cfg() -> EncoderConfig {
        EncoderConfig {
            width: 64,
            height: 64,
            fps: 30,
            bitrate_kbps: 2000,
            keyframe_interval_frames: 60,
            max_qp: None,
        }
    }

    #[test]
    fn first_frame_is_keyframe_with_sps_pps_idr() {
        let mut source = SyntheticSource::new(64, 64, 1000);
        let mut encoder = MediaFoundationEncoder::new(test_cfg(), true).expect("encoder init");

        let mut first: Option<EncodedFrame> = None;
        for _ in 0..30 {
            let raw = source.next_frame().expect("frame");
            if let Some(encoded) = encoder.encode(&raw, false).expect("encode") {
                first = Some(encoded);
                break;
            }
        }
        let encoded = first.expect("expected at least one output within 30 frames");

        assert!(encoded.keyframe);
        let types = nal_types(&encoded.data);
        assert!(types.contains(&7), "missing SPS: {types:?}");
        assert!(types.contains(&8), "missing PPS: {types:?}");
        assert!(types.contains(&5), "missing IDR: {types:?}");
    }

    #[test]
    fn thirty_frames_have_deltas_and_honour_force_keyframe() {
        let mut source = SyntheticSource::new(64, 64, 1000);
        let mut encoder = MediaFoundationEncoder::new(test_cfg(), true).expect("encoder init");

        let mut outputs = Vec::new();
        for i in 0..30u32 {
            let raw = source.next_frame().expect("frame");
            let force = i == 19; // 20th frame (0-indexed)
            if let Some(encoded) = encoder.encode(&raw, force).expect("encode") {
                outputs.push((i, encoded));
            }
        }

        assert!(!outputs.is_empty());
        assert!(outputs[0].1.keyframe);

        let has_delta = outputs
            .iter()
            .skip(1)
            .take(10)
            .any(|(_, f)| nal_types(&f.data).contains(&1));
        assert!(
            has_delta,
            "expected at least one non-keyframe (NAL 1) among the next 10 frames"
        );

        // The encoder can lag a frame or two behind (async MFTs especially),
        // so look for the forced IDR among outputs from frame 20 onward
        // rather than requiring it to land exactly on index 19.
        let forced_or_later = outputs
            .iter()
            .filter(|(idx, _)| *idx >= 19)
            .any(|(_, f)| nal_types(&f.data).contains(&5));
        assert!(
            forced_or_later,
            "expected an IDR at or after the forced frame"
        );
    }

    #[test]
    fn bgra_input_is_accepted() {
        let mut encoder = MediaFoundationEncoder::new(test_cfg(), true).expect("encoder init");

        let frame = RawFrame::Bgra {
            width: 64,
            height: 64,
            data: vec![0x80u8; 256 * 64],
            stride: 256,
            ts: Instant::now(),
        };

        let mut first: Option<EncodedFrame> = None;
        for _ in 0..30 {
            if let Some(encoded) = encoder.encode(&frame, true).expect("encode") {
                first = Some(encoded);
                break;
            }
        }
        let encoded = first.expect("expected at least one output within 30 attempts");
        assert!(encoded.keyframe);
    }

    #[test]
    fn bgra_with_padded_stride_is_encoded_first_keyframe() {
        let mut encoder = MediaFoundationEncoder::new(test_cfg(), true).expect("encoder init");

        let stride = 64 * 4 + 32;
        let data: Vec<u8> = (0..stride * 64).map(|i| (i * 7 % 251) as u8).collect();
        let frame = RawFrame::Bgra {
            width: 64,
            height: 64,
            data,
            stride,
            ts: Instant::now(),
        };

        let mut first: Option<EncodedFrame> = None;
        for _ in 0..30 {
            if let Some(encoded) = encoder.encode(&frame, true).expect("encode") {
                first = Some(encoded);
                break;
            }
        }
        let encoded = first.expect("expected at least one output within 30 attempts");
        assert!(encoded.keyframe);
    }

    #[test]
    fn sixty_frames_encode_with_reused_output_sample() {
        let mut source = SyntheticSource::new(64, 64, 1000);
        let mut encoder = MediaFoundationEncoder::new(test_cfg(), true).expect("encoder init");

        let mut outputs = 0usize;
        for _ in 0..60u32 {
            let raw = source.next_frame().expect("frame");
            if let Some(encoded) = encoder.encode(&raw, false).expect("encode") {
                assert!(!encoded.data.is_empty());
                outputs += 1;
            }
        }
        assert!(outputs > 0, "expected non-empty output within 60 frames");
    }

    #[test]
    fn set_rate_mid_stream_keeps_encoding() {
        let mut source = SyntheticSource::new(64, 64, 1000);
        let mut encoder = MediaFoundationEncoder::new(test_cfg(), true).expect("encoder init");

        for _ in 0..20u32 {
            let raw = source.next_frame().expect("frame");
            encoder.encode(&raw, false).expect("encode");
        }

        encoder
            .set_rate(RateTarget {
                bitrate_kbps: 300,
                fps: 30,
            })
            .expect("set_rate");

        for _ in 0..20u32 {
            let raw = source.next_frame().expect("frame");
            encoder.encode(&raw, false).expect("encode");
        }
    }
}
