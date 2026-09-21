//! `VideoToolbox`-backed `Encoder`: macOS hardware H.264
//! (`VTCompressionSession`). This whole module is macOS-only -- gated at
//! `mod.rs` (`#[cfg(target_os = "macos")]`), not per-item here.
//!
//! See `docs/host-libs-api-notes.md`'s VideoToolbox section for the API
//! notes (session lifecycle, AVCC->Annex-B conversion, keyframe detection)
//! this module was written against.

use std::ffi::{c_int, c_void};
use std::ptr::NonNull;
use std::sync::Mutex;
use std::time::Instant;

use objc2_core_foundation::{
    CFArray, CFBoolean, CFDictionary, CFNumber, CFRetained, CFString, CFType,
};
use objc2_core_media::{
    kCMSampleAttachmentKey_NotSync, kCMTimeInvalid, kCMVideoCodecType_H264, CMFormatDescription,
    CMSampleBuffer, CMTime, CMVideoFormatDescriptionGetH264ParameterSetAtIndex,
};
use objc2_core_video::{
    kCVPixelFormatType_32BGRA, kCVPixelFormatType_420YpCbCr8BiPlanarVideoRange, CVPixelBuffer,
    CVPixelBufferCreate, CVPixelBufferGetBaseAddress, CVPixelBufferGetBaseAddressOfPlane,
    CVPixelBufferGetBytesPerRow, CVPixelBufferGetBytesPerRowOfPlane, CVPixelBufferLockBaseAddress,
    CVPixelBufferLockFlags, CVPixelBufferUnlockBaseAddress,
};
use objc2_video_toolbox::{
    kVTCompressionPropertyKey_AllowFrameReordering, kVTCompressionPropertyKey_AverageBitRate,
    kVTCompressionPropertyKey_DataRateLimits, kVTCompressionPropertyKey_ExpectedFrameRate,
    kVTCompressionPropertyKey_MaxAllowedFrameQP, kVTCompressionPropertyKey_MaxFrameDelayCount,
    kVTCompressionPropertyKey_MaxKeyFrameInterval, kVTCompressionPropertyKey_ProfileLevel,
    kVTCompressionPropertyKey_RealTime,
    kVTCompressionPropertyKey_UsingHardwareAcceleratedVideoEncoder,
    kVTEncodeFrameOptionKey_ForceKeyFrame, kVTProfileLevel_H264_ConstrainedBaseline_AutoLevel,
    kVTVideoEncoderSpecification_EnableHardwareAcceleratedVideoEncoder, VTCompressionSession,
    VTEncodeInfoFlags, VTSessionCopyProperty, VTSessionSetProperty,
};

use crate::capture::RawFrame;

use super::{EncodeError, EncodedFrame, Encoder, EncoderConfig, RateTarget};

/// Annex-B start code every emitted NAL unit is prefixed with.
const START_CODE: [u8; 4] = [0, 0, 0, 1];

/// One encoded output the compression callback produced, already converted
/// to Annex-B.
struct EncodedOutput {
    data: Vec<u8>,
    keyframe: bool,
}

/// Shared state between `VideoToolboxEncoder` and the C callback VT invokes
/// with each compressed frame. Lives in a `Box` so its heap address stays
/// stable no matter how the owning `VideoToolboxEncoder` gets moved --
/// `output_callback_ref_con` is set once at session creation to a raw
/// pointer into this box.
struct CallbackState {
    /// Almost always holds 0 or 1 entries by the time `encode()` drains it
    /// (VT is configured for `MaxFrameDelayCount = 0` / synchronous-ish
    /// real-time encoding, and `encode()` calls `CompleteFrames` right after
    /// submitting); more than one is handled (see `encode()`) rather than
    /// assumed impossible.
    pending: Mutex<Vec<Result<EncodedOutput, i32>>>,
}

pub struct VideoToolboxEncoder {
    session: CFRetained<VTCompressionSession>,
    // Never read after `new()` sets `output_callback_ref_con` from it --
    // only needs to keep living at a stable address until `Drop` invalidates
    // the session (after which VT can no longer call back into it).
    callback_state: Box<CallbackState>,
    cfg: EncoderConfig,
    /// Wall-clock reference for the presentation timestamps handed to VT
    /// (see `docs/host-libs-api-notes.md`'s webrtc section for why these
    /// need to track real elapsed time rather than a frame-count-derived
    /// step -- same reasoning as `openh264::OpenH264Encoder::started`).
    started: Instant,
}

// SAFETY: `VideoToolboxEncoder` (like `openh264::OpenH264Encoder`) is only
// ever driven from the single dedicated encode thread that owns it for its
// entire lifetime; we only need `Send` to hand the freshly-built value to
// that thread once. Apple documents `VTCompressionSession` as safe to use
// from any thread as long as it isn't used concurrently from multiple
// threads at once, which single-threaded ownership here guarantees; the
// `Box<CallbackState>` it points its output callback at only exposes a
// `Mutex`-guarded `Vec`, which is `Send`/`Sync` on its own.
unsafe impl Send for VideoToolboxEncoder {}

impl VideoToolboxEncoder {
    pub fn new(cfg: EncoderConfig) -> Result<Self, EncodeError> {
        // SAFETY: reading an extern immutable static of the documented type.
        let hw_key = unsafe { kVTVideoEncoderSpecification_EnableHardwareAcceleratedVideoEncoder };
        let encoder_spec = CFDictionary::<CFString, CFType>::from_slices(
            &[hw_key],
            &[CFBoolean::new(true).as_ref()],
        );

        let callback_state = Box::new(CallbackState {
            pending: Mutex::new(Vec::new()),
        });
        let refcon = (&*callback_state as *const CallbackState)
            .cast_mut()
            .cast::<c_void>();

        let mut session_ptr: *mut VTCompressionSession = std::ptr::null_mut();
        // SAFETY: `session_ptr` is a valid stack out-pointer; `refcon` points
        // at `callback_state`, which this `Self` keeps alive for at least as
        // long as `session` can still invoke the callback (see `Drop`);
        // `compression_output_callback` matches `VTCompressionOutputCallback`'s
        // signature exactly and never panics (see its own doc comment).
        let status = unsafe {
            VTCompressionSession::create(
                None,
                cfg.width as i32,
                cfg.height as i32,
                kCMVideoCodecType_H264,
                Some(encoder_spec.as_ref()),
                None,
                None,
                Some(compression_output_callback),
                refcon,
                NonNull::from(&mut session_ptr),
            )
        };
        if status != 0 {
            return Err(EncodeError::Backend(format!(
                "VideoToolbox VTCompressionSessionCreate: OSStatus {status}"
            )));
        }
        let session_ptr = NonNull::new(session_ptr).ok_or_else(|| {
            EncodeError::Backend(
                "VideoToolbox VTCompressionSessionCreate: succeeded with a null session".into(),
            )
        })?;
        // SAFETY: `VTCompressionSessionCreate` returned success (OSStatus 0),
        // which per its contract hands us a +1 reference to a fresh session.
        let session: CFRetained<VTCompressionSession> =
            unsafe { CFRetained::from_raw(session_ptr) };

        if let Err(err) = configure_session(&session, &cfg) {
            // SAFETY: `session` was just successfully created above.
            unsafe { session.invalidate() };
            return Err(err);
        }

        // SAFETY: `session` is a valid, freshly-configured session.
        let status = unsafe { session.prepare_to_encode_frames() };
        if status != 0 {
            // SAFETY: `session` was successfully created above.
            unsafe { session.invalidate() };
            return Err(EncodeError::Backend(format!(
                "VideoToolbox VTCompressionSessionPrepareToEncodeFrames: OSStatus {status}"
            )));
        }

        let hardware = using_hardware_encoder(&session);
        tracing::info!(
            width = cfg.width,
            height = cfg.height,
            fps = cfg.fps,
            bitrate_kbps = cfg.bitrate_kbps,
            max_qp = ?cfg.max_qp,
            hardware,
            "initializing videotoolbox encoder"
        );

        Ok(Self {
            session,
            callback_state,
            cfg,
            started: Instant::now(),
        })
    }
}

/// Sets every session property `new()` needs before encoding can start.
/// Split out of `new()` so every property-setting failure funnels through
/// one `?` path that `new()` can `invalidate()` the session on.
fn configure_session(
    session: &VTCompressionSession,
    cfg: &EncoderConfig,
) -> Result<(), EncodeError> {
    // SAFETY (this whole function): every `kVTCompressionPropertyKey_*`/
    // `kVTProfileLevel_*` access below reads an extern immutable static of
    // the documented `&'static CFString` type; every `VTSessionSetProperty`
    // call passes a live CF object for the key and value, both retained by
    // the callee (VT copies whatever it needs synchronously).
    unsafe {
        set_property(
            session,
            "RealTime",
            kVTCompressionPropertyKey_RealTime,
            CFBoolean::new(true).as_ref(),
        )?;
        set_property(
            session,
            "ProfileLevel",
            kVTCompressionPropertyKey_ProfileLevel,
            kVTProfileLevel_H264_ConstrainedBaseline_AutoLevel.as_ref(),
        )?;

        apply_rate(session, cfg.bitrate_kbps, cfg.fps)?;

        set_property(
            session,
            "AllowFrameReordering",
            kVTCompressionPropertyKey_AllowFrameReordering,
            CFBoolean::new(false).as_ref(),
        )?;
        set_property(
            session,
            "MaxKeyFrameInterval",
            kVTCompressionPropertyKey_MaxKeyFrameInterval,
            CFNumber::new_i32(cfg.keyframe_interval_frames as i32).as_ref(),
        )?;
        // Soft failure: found on real hardware (Apple M4, macOS 26.6) that
        // this encoder rejects `MaxFrameDelayCount = 0` with
        // `kVTPropertyNotSupportedErr` even though it's one of Apple's
        // documented compression properties -- unlike the properties above,
        // rejecting it doesn't make the session unusable, it just means VT
        // is free to buffer a frame or two internally before handing back
        // output (harmless for `encode()`'s call-and-drain loop, which
        // already tolerates zero outputs per call).
        soft_set_property(
            session,
            "MaxFrameDelayCount",
            kVTCompressionPropertyKey_MaxFrameDelayCount,
            CFNumber::new_i32(0).as_ref(),
        );

        if let Some(max_qp) = cfg.max_qp {
            // Soft failure: `kVTCompressionPropertyKey_MaxAllowedFrameQP` is
            // a newer key that isn't guaranteed to exist on every macOS
            // version this binary runs on -- unlike the properties above,
            // rejecting it doesn't make the session unusable.
            soft_set_property(
                session,
                "MaxAllowedFrameQP",
                kVTCompressionPropertyKey_MaxAllowedFrameQP,
                CFNumber::new_i32(max_qp as i32).as_ref(),
            );
        }
    }

    Ok(())
}

/// Sets the session properties that control output rate: the average
/// bitrate ceiling, the hard data-rate limit derived from it, and the frame
/// rate VT budgets that bitrate over. Split out of `configure_session` so
/// `set_rate` can call it again on a live session -- Apple documents these
/// three as changeable mid-stream via `VTSessionSetProperty`, unlike e.g.
/// `ProfileLevel`.
///
/// # Safety
///
/// Caller must ensure `session` is a valid, live `VTCompressionSession`.
unsafe fn apply_rate(
    session: &VTCompressionSession,
    bitrate_kbps: u32,
    fps: u32,
) -> Result<(), EncodeError> {
    let bitrate_bps = bitrate_kbps.saturating_mul(1000);
    // SAFETY: forwarded from this function's own safety contract; every key
    // is an extern immutable static of the documented `&'static CFString`
    // type.
    unsafe {
        set_property(
            session,
            "AverageBitRate",
            kVTCompressionPropertyKey_AverageBitRate,
            CFNumber::new_i32(bitrate_bps as i32).as_ref(),
        )?;

        let data_rate_limits = CFArray::<CFNumber>::from_objects(&[
            &CFNumber::new_i32((bitrate_bps / 8) as i32),
            &CFNumber::new_i32(1),
        ]);
        set_property(
            session,
            "DataRateLimits",
            kVTCompressionPropertyKey_DataRateLimits,
            data_rate_limits.as_ref(),
        )?;

        set_property(
            session,
            "ExpectedFrameRate",
            kVTCompressionPropertyKey_ExpectedFrameRate,
            CFNumber::new_i32(fps as i32).as_ref(),
        )?;
    }

    Ok(())
}

/// # Safety
///
/// Caller must ensure `key` and `value` are valid, live CF objects for the
/// duration of the call.
unsafe fn set_property(
    session: &VTCompressionSession,
    name: &str,
    key: &CFString,
    value: &CFType,
) -> Result<(), EncodeError> {
    // SAFETY: forwarded from this function's own safety contract.
    let status = unsafe { VTSessionSetProperty(session, key, Some(value)) };
    if status != 0 {
        return Err(EncodeError::Backend(format!(
            "VideoToolbox VTSessionSetProperty({name}): OSStatus {status}"
        )));
    }
    Ok(())
}

/// Like `set_property`, but a non-zero `OSStatus` is only `tracing::warn!`-ed,
/// not treated as fatal -- for properties whose absence degrades the
/// session (more buffering, no QP ceiling) rather than breaking it.
///
/// # Safety
///
/// Caller must ensure `key` and `value` are valid, live CF objects for the
/// duration of the call.
unsafe fn soft_set_property(
    session: &VTCompressionSession,
    name: &str,
    key: &CFString,
    value: &CFType,
) {
    // SAFETY: forwarded from this function's own safety contract.
    let status = unsafe { VTSessionSetProperty(session, key, Some(value)) };
    if status != 0 {
        // `debug`, not `warn`: by definition non-fatal, and M4 rejects
        // `MaxFrameDelayCount` on every single session (debt D12).
        tracing::debug!(
            name,
            status,
            "VideoToolbox property not accepted, continuing without it"
        );
    }
}

/// Reads back `kVTCompressionPropertyKey_UsingHardwareAcceleratedVideoEncoder`
/// after `PrepareToEncodeFrames`, purely for the startup log line. Any
/// failure to read it (missing key, unexpected type) is reported as `false`
/// rather than failing encoder construction -- this is diagnostic, not load
/// bearing.
fn using_hardware_encoder(session: &VTCompressionSession) -> bool {
    let mut value: *const CFBoolean = std::ptr::null();
    // SAFETY: `value` is a valid stack out-pointer of the type
    // `VTSessionCopyProperty` is documented to write for this key
    // (CFBoolean); reading the extern static key is sound (documented
    // `&'static CFString`).
    let status = unsafe {
        VTSessionCopyProperty(
            session,
            kVTCompressionPropertyKey_UsingHardwareAcceleratedVideoEncoder,
            None,
            (&mut value as *mut *const CFBoolean).cast(),
        )
    };
    if status != 0 {
        return false;
    }
    let Some(ptr) = NonNull::new(value.cast_mut()) else {
        return false;
    };
    // SAFETY: `VTSessionCopyProperty` returned success (OSStatus 0), which
    // per its contract hands us a +1 reference to the copied property value.
    let value: CFRetained<CFBoolean> = unsafe { CFRetained::from_raw(ptr) };
    value.as_bool()
}

impl Encoder for VideoToolboxEncoder {
    fn encode(
        &mut self,
        frame: &RawFrame,
        force_keyframe: bool,
    ) -> Result<Option<EncodedFrame>, EncodeError> {
        let captured_at = frame.ts();

        let (width, height) = frame_dims(frame);
        if width != self.cfg.width || height != self.cfg.height {
            return Err(EncodeError::Backend(format!(
                "VideoToolbox encoder configured for {}x{}, got a {}x{} frame",
                self.cfg.width, self.cfg.height, width, height
            )));
        }

        let pixel_buffer = create_pixel_buffer(frame)?;

        let ts_us = self.started.elapsed().as_micros() as i64;
        // SAFETY: `CMTime::new` (aka `CMTimeMake`) is a pure value
        // constructor with no aliasing/lifetime concerns.
        let pts = unsafe { CMTime::new(ts_us, 1_000_000) };
        // SAFETY: reading an extern immutable static of the documented
        // `CMTime` type.
        let duration = unsafe { kCMTimeInvalid };

        let force_key_dict = force_keyframe.then(|| {
            // SAFETY: reading an extern immutable static of the documented
            // `&'static CFString` type.
            let key = unsafe { kVTEncodeFrameOptionKey_ForceKeyFrame };
            CFDictionary::<CFString, CFType>::from_slices(&[key], &[CFBoolean::new(true).as_ref()])
        });

        // SAFETY: `pixel_buffer` is a valid `CVPixelBuffer` matching the
        // session's configured dimensions and pixel format (converted from
        // whichever `RawFrame` variant we were given, locked/unlocked while
        // filling it in `create_pixel_buffer`); `force_key_dict` (if any) is
        // a live `CFDictionary`; we pass null for `source_frame_refcon` (the
        // callback gets its state from `output_callback_ref_con` set at
        // session creation instead) and null for `info_flags_out` (a
        // synchronously dropped frame just yields no queued output below,
        // which `encode()` already treats as `Ok(None)`).
        let status = unsafe {
            self.session.encode_frame(
                &pixel_buffer,
                pts,
                duration,
                force_key_dict.as_ref().map(|d| d.as_ref()),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
            )
        };
        if status != 0 {
            return Err(EncodeError::Backend(format!(
                "VideoToolbox VTCompressionSessionEncodeFrame: OSStatus {status}"
            )));
        }

        // SAFETY: `session` is valid. `kCMTimeInvalid` asks VT to complete
        // every pending frame; with `RealTime` + `MaxFrameDelayCount = 0`
        // this pulls the frame just submitted through the output callback
        // before the call returns, which is what lets `encode()` be
        // call-and-immediately-drain rather than needing its own queue.
        let status = unsafe { self.session.complete_frames(kCMTimeInvalid) };
        if status != 0 {
            return Err(EncodeError::Backend(format!(
                "VideoToolbox VTCompressionSessionCompleteFrames: OSStatus {status}"
            )));
        }

        let outputs = {
            let mut pending = self
                .callback_state
                .pending
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            std::mem::take(&mut *pending)
        };

        if outputs.len() > 1 {
            tracing::warn!(
                count = outputs.len(),
                "VideoToolbox produced more than one output for a single encode() call, \
                 keeping only the first"
            );
        }

        match outputs.into_iter().next() {
            None => Ok(None),
            Some(Ok(out)) => Ok(Some(EncodedFrame {
                data: out.data,
                keyframe: out.keyframe,
                ts: Instant::now(),
                captured_at,
            })),
            Some(Err(status)) => Err(EncodeError::Backend(format!(
                "VideoToolbox encode callback reported OSStatus {status}"
            ))),
        }
    }

    fn set_rate(&mut self, target: RateTarget) -> Result<(), EncodeError> {
        // SAFETY: `self.session` was successfully created and configured in
        // `new()` and stays valid until `Drop::drop` invalidates it.
        unsafe { apply_rate(&self.session, target.bitrate_kbps, target.fps) }
    }
}

impl Drop for VideoToolboxEncoder {
    fn drop(&mut self) {
        // SAFETY: `self.session` was successfully created in `new()` and is
        // invalidated exactly once, here; `CFRetained`'s own `Drop` releases
        // our reference right after (VT's docs: invalidate, then release).
        unsafe { self.session.invalidate() };
    }
}

fn frame_dims(frame: &RawFrame) -> (u32, u32) {
    match frame {
        RawFrame::Nv12 { width, height, .. } | RawFrame::Bgra { width, height, .. } => {
            (*width, *height)
        }
    }
}

/// Builds a `CVPixelBuffer` matching `frame`'s dimensions and pixel format,
/// filled in with `frame`'s pixel data.
fn create_pixel_buffer(frame: &RawFrame) -> Result<CFRetained<CVPixelBuffer>, EncodeError> {
    let (width, height) = frame_dims(frame);
    let pixel_format = match frame {
        RawFrame::Nv12 { .. } => kCVPixelFormatType_420YpCbCr8BiPlanarVideoRange,
        RawFrame::Bgra { .. } => kCVPixelFormatType_32BGRA,
    };

    let mut buffer_ptr: *mut CVPixelBuffer = std::ptr::null_mut();
    // SAFETY: `buffer_ptr` is a valid stack out-pointer; no pixel buffer
    // attributes dictionary is needed since we fill every byte ourselves
    // right after, before handing it to VT.
    let status = unsafe {
        CVPixelBufferCreate(
            None,
            width as usize,
            height as usize,
            pixel_format,
            None,
            NonNull::from(&mut buffer_ptr),
        )
    };
    if status != 0 {
        return Err(EncodeError::Backend(format!(
            "VideoToolbox CVPixelBufferCreate: CVReturn {status}"
        )));
    }
    let buffer_ptr = NonNull::new(buffer_ptr).ok_or_else(|| {
        EncodeError::Backend(
            "VideoToolbox CVPixelBufferCreate: succeeded with a null buffer".into(),
        )
    })?;
    // SAFETY: `CVPixelBufferCreate` returned success (`kCVReturnSuccess`),
    // which per its contract hands us a +1 reference to a fresh buffer.
    let buffer: CFRetained<CVPixelBuffer> = unsafe { CFRetained::from_raw(buffer_ptr) };

    // SAFETY: `buffer` was just created above and isn't shared with
    // anything else yet.
    let lock_status =
        unsafe { CVPixelBufferLockBaseAddress(&buffer, CVPixelBufferLockFlags::empty()) };
    if lock_status != 0 {
        return Err(EncodeError::Backend(format!(
            "VideoToolbox CVPixelBufferLockBaseAddress: CVReturn {lock_status}"
        )));
    }

    let copy_result = copy_frame_into_pixel_buffer(frame, &buffer);

    // SAFETY: matches the successful lock above; unlocked unconditionally
    // (even on a copy error) so the buffer is never left permanently
    // locked.
    unsafe { CVPixelBufferUnlockBaseAddress(&buffer, CVPixelBufferLockFlags::empty()) };

    copy_result?;
    Ok(buffer)
}

fn copy_frame_into_pixel_buffer(
    frame: &RawFrame,
    buffer: &CVPixelBuffer,
) -> Result<(), EncodeError> {
    match frame {
        RawFrame::Nv12 {
            width,
            height,
            y,
            y_stride,
            uv,
            uv_stride,
            ..
        } => {
            let width = *width as usize;
            let height = *height as usize;

            let y_dst_stride = CVPixelBufferGetBytesPerRowOfPlane(buffer, 0);
            let y_dst = CVPixelBufferGetBaseAddressOfPlane(buffer, 0);
            if y_dst.is_null() {
                return Err(EncodeError::Backend(
                    "VideoToolbox: NV12 Y plane base address is null".into(),
                ));
            }
            copy_plane(y, *y_stride, width, height, y_dst, y_dst_stride);

            let uv_height = height.div_ceil(2);
            let uv_row_bytes = width.div_ceil(2) * 2; // interleaved CbCr: 2 bytes per chroma sample pair
            let uv_dst_stride = CVPixelBufferGetBytesPerRowOfPlane(buffer, 1);
            let uv_dst = CVPixelBufferGetBaseAddressOfPlane(buffer, 1);
            if uv_dst.is_null() {
                return Err(EncodeError::Backend(
                    "VideoToolbox: NV12 UV plane base address is null".into(),
                ));
            }
            copy_plane(
                uv,
                *uv_stride,
                uv_row_bytes,
                uv_height,
                uv_dst,
                uv_dst_stride,
            );

            Ok(())
        }
        RawFrame::Bgra {
            width,
            height,
            data,
            stride,
            ..
        } => {
            let row_bytes = *width as usize * 4;
            let dst_stride = CVPixelBufferGetBytesPerRow(buffer);
            let dst = CVPixelBufferGetBaseAddress(buffer);
            if dst.is_null() {
                return Err(EncodeError::Backend(
                    "VideoToolbox: BGRA base address is null".into(),
                ));
            }
            copy_plane(data, *stride, row_bytes, *height as usize, dst, dst_stride);
            Ok(())
        }
    }
}

/// Copies `rows` rows of `row_bytes` bytes each from `src` (stride
/// `src_stride`) into the CVPixelBuffer-owned buffer at `dst_base` (stride
/// `dst_stride`), row by row -- the two strides are usually different
/// (`dst_stride` is whatever alignment CoreVideo picked for the buffer it
/// allocated).
fn copy_plane(
    src: &[u8],
    src_stride: usize,
    row_bytes: usize,
    rows: usize,
    dst_base: *mut c_void,
    dst_stride: usize,
) {
    // SAFETY: `dst_base` is the base address of a plane/buffer inside a
    // `CVPixelBuffer` that's currently locked (`CVPixelBufferLockBaseAddress`
    // was just called successfully in `create_pixel_buffer`), sized by
    // CoreVideo for at least `rows` rows of `dst_stride` bytes each; CoreVideo
    // never returns a stride narrower than the pixel format's row size, so
    // `row_bytes <= dst_stride` and every write below stays within the
    // buffer.
    let dst = unsafe { std::slice::from_raw_parts_mut(dst_base.cast::<u8>(), rows * dst_stride) };
    for row in 0..rows {
        dst[row * dst_stride..row * dst_stride + row_bytes]
            .copy_from_slice(&src[row * src_stride..row * src_stride + row_bytes]);
    }
}

/// The C callback VT invokes (on some internal VT thread, possibly not the
/// one that called `VTCompressionSessionEncodeFrame`) with each compressed
/// frame. Must never panic -- there is no sound way to unwind across this
/// FFI boundary -- so every fallible step below is funneled through
/// `Result` and turned into a `pending.push(Err(status))` entry instead.
///
/// # Safety
///
/// Must only be installed as a `VTCompressionSession`'s `output_callback`
/// with `output_callback_ref_con` set to a `*const CallbackState` that
/// outlives every call VT can make through this callback (guaranteed by
/// `VideoToolboxEncoder::new`/`Drop`: the session is invalidated, which
/// per Apple's docs makes further callback invocations impossible, before
/// `callback_state` is dropped).
unsafe extern "C-unwind" fn compression_output_callback(
    output_callback_ref_con: *mut c_void,
    _source_frame_ref_con: *mut c_void,
    status: i32,
    info_flags: VTEncodeInfoFlags,
    sample_buffer: *mut CMSampleBuffer,
) {
    // SAFETY: forwarded from this function's own safety contract.
    let state = unsafe { &*output_callback_ref_con.cast::<CallbackState>() };

    let result = compression_output(status, info_flags, sample_buffer);

    let mut pending = state
        .pending
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    if let Some(result) = result {
        pending.push(result);
    }
}

/// `None` means the frame was dropped (nothing to report); `Some(Err(_))`
/// covers both a reported encode failure and any failure while converting a
/// successful sample buffer to Annex-B (there is no other way to surface
/// the latter from this callback than as an encode error).
fn compression_output(
    status: i32,
    info_flags: VTEncodeInfoFlags,
    sample_buffer: *mut CMSampleBuffer,
) -> Option<Result<EncodedOutput, i32>> {
    if status != 0 {
        return Some(Err(status));
    }
    if info_flags.contains(VTEncodeInfoFlags::FrameDropped) {
        return None;
    }
    let sample_buffer = NonNull::new(sample_buffer)?;
    // SAFETY: VT hands us a valid, live `CMSampleBuffer` for the duration of
    // this callback (status is 0 and the frame wasn't dropped, so it must be
    // non-null per VT's documented contract); we only read from it here, we
    // don't take ownership.
    let sample_buffer = unsafe { sample_buffer.as_ref() };
    Some(encode_sample_buffer(sample_buffer))
}

fn encode_sample_buffer(sbuf: &CMSampleBuffer) -> Result<EncodedOutput, i32> {
    let keyframe = sample_is_keyframe(sbuf)?;

    // SAFETY: read-only accessor on a valid sample buffer.
    let format_desc = unsafe { sbuf.format_description() }.ok_or(-1)?;
    let (sps, nal_length_size) = h264_parameter_set(&format_desc, 0)?;

    let mut data = Vec::new();
    if keyframe {
        data.extend_from_slice(&START_CODE);
        data.extend_from_slice(sps);
        let (pps, _) = h264_parameter_set(&format_desc, 1)?;
        data.extend_from_slice(&START_CODE);
        data.extend_from_slice(pps);
    }

    // SAFETY: read-only accessor on a valid sample buffer.
    let block = unsafe { sbuf.data_buffer() }.ok_or(-1)?;
    // SAFETY: read-only accessor on a valid block buffer.
    let len = unsafe { block.data_length() };
    let mut avcc = vec![0u8; len];
    if len > 0 {
        let dst = NonNull::new(avcc.as_mut_ptr().cast::<c_void>()).ok_or(-1)?;
        // SAFETY: `dst` points at exactly `len` freshly allocated bytes,
        // matching the length we just read from the same block buffer.
        let status = unsafe { block.copy_data_bytes(0, len, dst) };
        if status != 0 {
            return Err(status);
        }
    }

    avcc_to_annexb(nal_length_size, &avcc, &mut data)?;

    Ok(EncodedOutput { data, keyframe })
}

/// `kCMSampleAttachmentKey_NotSync`'s *absence* (or an explicit `false`
/// value) means the sample is a sync sample, i.e. a keyframe -- see
/// `docs/host-libs-api-notes.md`'s VideoToolbox section.
fn sample_is_keyframe(sbuf: &CMSampleBuffer) -> Result<bool, i32> {
    // SAFETY: read-only accessor on a valid sample buffer.
    let Some(array) = (unsafe { sbuf.sample_attachments_array(false) }) else {
        return Ok(true);
    };
    // SAFETY: `CMSampleBufferGetSampleAttachmentsArray` always returns an
    // array of `CFDictionary` elements (one per sample) per Apple's
    // documented contract; reinterpreting the array's opaque element type is
    // sound because `CFArray<T>`'s representation doesn't depend on `T`.
    let array: CFRetained<CFArray<CFDictionary<CFString, CFType>>> =
        unsafe { CFRetained::cast_unchecked(array) };
    let Some(dict) = array.get(0) else {
        return Ok(true);
    };
    // SAFETY: reading an extern immutable static of the documented
    // `&'static CFString` type.
    let not_sync_key = unsafe { kCMSampleAttachmentKey_NotSync };
    match dict.get(not_sync_key) {
        None => Ok(true),
        Some(value) => {
            // SAFETY: `kCMSampleAttachmentKey_NotSync`'s value is always a
            // `CFBoolean` per Apple's documented contract.
            let flag: CFRetained<CFBoolean> = unsafe { CFRetained::cast_unchecked(value) };
            Ok(!flag.as_bool())
        }
    }
}

/// Returns NAL unit `index` (0 = SPS, 1 = PPS) from an H.264 format
/// description's AVC decoder configuration record, along with the AVCC NAL
/// length-prefix size (in bytes) recorded there -- the latter is the same
/// for every parameter set index, so callers that only need it can pass
/// `index = 0` and ignore the returned bytes.
fn h264_parameter_set(desc: &CMFormatDescription, index: usize) -> Result<(&[u8], usize), i32> {
    let mut ptr: *const u8 = std::ptr::null();
    let mut size: usize = 0;
    let mut count: usize = 0;
    let mut nal_header_len: c_int = 0;
    // SAFETY: all four out-parameters are valid stack pointers.
    let status = unsafe {
        CMVideoFormatDescriptionGetH264ParameterSetAtIndex(
            desc,
            index,
            &mut ptr,
            &mut size,
            &mut count,
            &mut nal_header_len,
        )
    };
    if status != 0 {
        return Err(status);
    }
    if ptr.is_null() {
        return Err(-1);
    }
    // SAFETY: VT guarantees `ptr` is valid for `size` bytes for as long as a
    // retain on `desc` is held, which it is for the lifetime of this
    // returned slice's borrow (tied to `desc`'s own lifetime parameter).
    let bytes = unsafe { std::slice::from_raw_parts(ptr, size) };
    Ok((bytes, nal_header_len as usize))
}

/// Converts an AVCC bitstream (each NAL unit prefixed by a big-endian
/// `nal_length_size`-byte length instead of a start code) into Annex-B,
/// appending the result to `out`.
fn avcc_to_annexb(nal_length_size: usize, avcc: &[u8], out: &mut Vec<u8>) -> Result<(), i32> {
    if nal_length_size == 0 {
        return Err(-1);
    }

    let mut i = 0;
    while i + nal_length_size <= avcc.len() {
        let mut len: usize = 0;
        for &b in &avcc[i..i + nal_length_size] {
            len = (len << 8) | usize::from(b);
        }
        i += nal_length_size;

        if i + len > avcc.len() {
            return Err(-1);
        }
        out.extend_from_slice(&START_CODE);
        out.extend_from_slice(&avcc[i..i + len]);
        i += len;
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::capture::synthetic::SyntheticSource;
    use crate::capture::FrameSource;
    use crate::encode::nal_types;

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
    fn first_frame_is_keyframe_with_sps_pps_idr_constrained_baseline() {
        let mut source = SyntheticSource::new(64, 64, 1000);
        let mut encoder = VideoToolboxEncoder::new(test_cfg()).expect("encoder init");

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

        // Parse the SPS NAL to check the profile: Constrained Baseline is
        // profile_idc 0x42 with constraint_set1_flag (0x40) set in the next
        // byte.
        let sps_offset = encoded
            .data
            .windows(5)
            .position(|w| w[..4] == [0, 0, 0, 1] && w[4] & 0x1F == 7)
            .expect("SPS start code not found");
        let sps_payload = &encoded.data[sps_offset + 4..];
        assert_eq!(
            sps_payload[1], 0x42,
            "profile_idc must be Constrained Baseline (0x42)"
        );
        assert_eq!(
            sps_payload[2] & 0x40,
            0x40,
            "constraint_set1_flag must be set"
        );
    }

    #[test]
    fn thirty_frames_have_deltas_and_honour_force_keyframe() {
        let mut source = SyntheticSource::new(64, 64, 1000);
        let mut encoder = VideoToolboxEncoder::new(test_cfg()).expect("encoder init");

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

        let forced = outputs
            .iter()
            .find(|(idx, _)| *idx == 19)
            .expect("frame 20 should produce output");
        assert!(
            nal_types(&forced.1.data).contains(&5),
            "expected IDR after force_keyframe"
        );

        let total_size: usize = outputs.iter().map(|(_, f)| f.data.len()).sum();
        assert!(
            total_size < 200 * 1024,
            "total encoded size too large: {total_size} bytes"
        );
    }

    #[test]
    fn max_qp_is_accepted() {
        let mut cfg = test_cfg();
        cfg.max_qp = Some(30);

        let mut source = SyntheticSource::new(64, 64, 1000);
        let mut encoder = VideoToolboxEncoder::new(cfg).expect("encoder init must accept max_qp");

        let mut first: Option<EncodedFrame> = None;
        for _ in 0..30 {
            let raw = source.next_frame().expect("frame");
            if let Some(encoded) = encoder.encode(&raw, false).expect("encode") {
                first = Some(encoded);
                break;
            }
        }
        assert!(
            first.is_some(),
            "expected at least one output within 30 frames"
        );
    }

    #[test]
    fn bgra_input_is_accepted() {
        let mut encoder = VideoToolboxEncoder::new(test_cfg()).expect("encoder init");

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
    fn set_rate_mid_stream_keeps_encoding_without_new_keyframe() {
        let mut cfg = test_cfg();
        cfg.width = 64;
        cfg.height = 64;

        let mut source = SyntheticSource::new(64, 64, 1000);
        let mut encoder = VideoToolboxEncoder::new(cfg).expect("encoder init");

        for _ in 0..20u32 {
            let raw = source.next_frame().expect("frame");
            encoder.encode(&raw, false).expect("encode");
        }

        encoder
            .set_rate(RateTarget {
                bitrate_kbps: 300,
                fps: 15,
            })
            .expect("set_rate");

        let mut outputs = Vec::new();
        for _ in 0..20u32 {
            let raw = source.next_frame().expect("frame");
            if let Some(encoded) = encoder.encode(&raw, false).expect("encode") {
                outputs.push(encoded);
            }
        }

        assert!(
            !outputs.is_empty(),
            "expected at least one output after set_rate"
        );
        assert!(
            !outputs[0].keyframe,
            "set_rate must not force a new keyframe"
        );
    }
}
