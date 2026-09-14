//! H.264 encoding: turns `RawFrame`s into Annex-B access units.

use std::time::Instant;

use crate::capture::RawFrame;

pub mod convert;
#[cfg(target_os = "windows")]
pub mod mediafoundation;
pub mod openh264;
#[cfg(target_os = "macos")]
pub mod videotoolbox;

pub use convert::{to_i420, I420Frame};

/// One encoded H.264 access unit, Annex-B (start-code prefixed NAL units
/// concatenated together).
#[derive(Debug, Clone)]
pub struct EncodedFrame {
    pub data: Vec<u8>,
    pub keyframe: bool,
    pub ts: Instant,
    /// Wall-clock time the *source* frame was captured, as opposed to `ts`
    /// (when encoding finished). The encoder sets this from
    /// `RawFrame::ts()` right at the start of `encode()`. The transport uses
    /// it to stamp RTP timestamps from real capture gaps rather than a fixed
    /// `1/fps` step (see `docs/host-libs-api-notes.md`'s webrtc section).
    pub captured_at: Instant,
}

pub trait Encoder: Send {
    fn encode(
        &mut self,
        frame: &RawFrame,
        force_keyframe: bool,
    ) -> Result<Option<EncodedFrame>, EncodeError>;
}

/// Which H.264 encoder backend to use.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EncoderKind {
    /// Software encoder, built from source (see `openh264.rs`). Available on
    /// all platforms; used everywhere except macOS, and as the fallback
    /// there when VideoToolbox fails to initialize.
    OpenH264,
    /// macOS hardware encoder (see `videotoolbox.rs`). Only available when
    /// `target_os = "macos"`; `build_encoder` rejects it with
    /// `EncodeError::Unsupported` everywhere else.
    VideoToolbox,
    /// Windows hardware encoder (Media Foundation, see `mediafoundation.rs`).
    /// Only available when `target_os = "windows"`; `build_encoder` rejects
    /// it with `EncodeError::Unsupported` everywhere else.
    MediaFoundation,
}

impl EncoderKind {
    pub fn name(self) -> &'static str {
        match self {
            EncoderKind::OpenH264 => "openh264",
            EncoderKind::VideoToolbox => "videotoolbox",
            EncoderKind::MediaFoundation => "mediafoundation",
        }
    }
}

/// Builds an `Encoder` for the requested backend, returning the `EncoderKind`
/// actually chosen alongside it (useful when `kind` is `None`, i.e. "auto").
///
/// `Some(EncoderKind::VideoToolbox)` builds the macOS hardware encoder on
/// macOS, and fails with `EncodeError::Unsupported` everywhere else.
/// `Some(EncoderKind::MediaFoundation)` builds the Windows hardware-or-software
/// encoder (`allow_software = true`, since explicitly asking for this backend
/// implies accepting whatever MFT is available) on Windows, and fails with
/// `EncodeError::Unsupported` everywhere else.
///
/// `None` ("auto") tries the platform's native encoder first (VideoToolbox
/// on macOS; on Windows any Media Foundation MFT, hardware preferred and
/// Microsoft's software "H264 Encoder MFT" otherwise -- measured on the
/// owner's Windows 10 bench in slice 2.2 it beats openh264 on encode
/// latency, fps under load and text sharpness), falling back to openh264
/// only if building it fails (`tracing::warn!`). On every other platform
/// "auto" goes straight to openh264 (there is no native backend there).
pub fn build_encoder(
    kind: Option<EncoderKind>,
    cfg: EncoderConfig,
) -> Result<(Box<dyn Encoder>, EncoderKind), EncodeError> {
    #[cfg(target_os = "macos")]
    if kind.is_none() {
        // Auto: try the hardware encoder first, fall back to openh264 (with
        // a warning) if it fails to initialize -- e.g. no hardware encoder
        // available at all, which happens on some CI/VM configurations.
        tracing::info!(encoder = EncoderKind::VideoToolbox.name(), "video encoder");
        match videotoolbox::VideoToolboxEncoder::new(cfg) {
            Ok(encoder) => return Ok((Box::new(encoder), EncoderKind::VideoToolbox)),
            Err(err) => {
                tracing::warn!(
                    error = %err,
                    "videotoolbox encoder unavailable, falling back to openh264"
                );
            }
        }
    }

    #[cfg(target_os = "windows")]
    if kind.is_none() {
        // Auto: any MFT, hardware first, Microsoft's software encoder next
        // (see this function's doc comment). Every Windows 10+ install ships
        // the software MFT, so failing here is unexpected -- `warn!`.
        tracing::info!(
            encoder = EncoderKind::MediaFoundation.name(),
            "video encoder"
        );
        match mediafoundation::MediaFoundationEncoder::new(cfg, true) {
            Ok(encoder) => return Ok((Box::new(encoder), EncoderKind::MediaFoundation)),
            Err(err) => {
                tracing::warn!(
                    error = %err,
                    "mediafoundation encoder unavailable, falling back to openh264"
                );
            }
        }
    }

    let kind = kind.unwrap_or(EncoderKind::OpenH264);
    tracing::info!(encoder = kind.name(), "video encoder");

    match kind {
        EncoderKind::OpenH264 => {
            let encoder = openh264::OpenH264Encoder::new(cfg)?;
            Ok((Box::new(encoder), EncoderKind::OpenH264))
        }
        #[cfg(target_os = "macos")]
        EncoderKind::VideoToolbox => {
            let encoder = videotoolbox::VideoToolboxEncoder::new(cfg)?;
            Ok((Box::new(encoder), EncoderKind::VideoToolbox))
        }
        #[cfg(not(target_os = "macos"))]
        EncoderKind::VideoToolbox => Err(EncodeError::Unsupported(kind)),
        #[cfg(target_os = "windows")]
        EncoderKind::MediaFoundation => {
            let encoder = mediafoundation::MediaFoundationEncoder::new(cfg, true)?;
            Ok((Box::new(encoder), EncoderKind::MediaFoundation))
        }
        #[cfg(not(target_os = "windows"))]
        EncoderKind::MediaFoundation => Err(EncodeError::Unsupported(kind)),
    }
}

#[derive(Debug, Clone, Copy)]
pub struct EncoderConfig {
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    pub bitrate_kbps: u32,
    pub keyframe_interval_frames: u32,
    /// Hard ceiling on the encoder's QP (quantization parameter), 0..=51.
    /// `None` leaves each backend's own default: openh264 caps at 30
    /// (`openh264::DEFAULT_MAX_QP`, its rate control otherwise blurs typed
    /// text), VideoToolbox and Media Foundation apply no extra cap.
    /// `Some(n)` caps the worst-case QP at `n`, putting a floor under frame
    /// quality on hard-to-compress content at the cost of the encoder
    /// possibly exceeding the target bitrate to hold that quality.
    pub max_qp: Option<u8>,
}

#[derive(Debug, thiserror::Error)]
pub enum EncodeError {
    #[error("encoder backend error: {0}")]
    Backend(String),
    /// Requested a hardware backend this build doesn't have -- VideoToolbox
    /// is only built on macOS, Media Foundation only on Windows.
    #[error("{0:?} encoder is not available in this build")]
    Unsupported(EncoderKind),
}

/// Parses an Annex-B bitstream (3- or 4-byte start codes) and returns the
/// `nal_unit_type` (low 5 bits of the NAL header byte) of every NAL unit
/// found, in order.
pub fn nal_types(annexb: &[u8]) -> Vec<u8> {
    let mut types = Vec::new();
    let mut i = 0;

    while i + 3 <= annexb.len() {
        let is_start4 = i + 4 <= annexb.len()
            && annexb[i] == 0
            && annexb[i + 1] == 0
            && annexb[i + 2] == 0
            && annexb[i + 3] == 1;
        let is_start3 = !is_start4 && annexb[i] == 0 && annexb[i + 1] == 0 && annexb[i + 2] == 1;

        if is_start4 {
            if let Some(&header) = annexb.get(i + 4) {
                types.push(header & 0x1F);
            }
            i += 4;
        } else if is_start3 {
            if let Some(&header) = annexb.get(i + 3) {
                types.push(header & 0x1F);
            }
            i += 3;
        } else {
            i += 1;
        }
    }

    types
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nal_types_parses_3_and_4_byte_start_codes() {
        // SPS (7) with a 4-byte start code, PPS (8) and IDR (5) with 3-byte
        // start codes, matching what an Annex-B encoder emits.
        #[rustfmt::skip]
        let stream: Vec<u8> = vec![
            0, 0, 0, 1, 0x67, 0xAA, 0xBB, // SPS
            0, 0, 1, 0x68, 0xCC,          // PPS
            0, 0, 1, 0x65, 0xDD, 0xEE,    // IDR
        ];

        assert_eq!(nal_types(&stream), vec![7, 8, 5]);
    }

    #[test]
    fn nal_types_empty_for_no_start_code() {
        assert!(nal_types(&[1, 2, 3, 4]).is_empty());
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
    fn build_encoder_rejects_backends_not_built_in() {
        #[cfg(target_os = "macos")]
        {
            assert!(matches!(
                build_encoder(Some(EncoderKind::MediaFoundation), test_cfg()),
                Err(EncodeError::Unsupported(_))
            ));

            let (_, kind) = build_encoder(Some(EncoderKind::VideoToolbox), test_cfg())
                .expect("videotoolbox must build on macOS");
            assert_eq!(kind, EncoderKind::VideoToolbox);

            let (_, kind) = build_encoder(None, test_cfg()).expect("auto must build on macOS");
            assert_eq!(kind, EncoderKind::VideoToolbox);
        }

        #[cfg(target_os = "windows")]
        {
            assert!(matches!(
                build_encoder(Some(EncoderKind::VideoToolbox), test_cfg()),
                Err(EncodeError::Unsupported(_))
            ));

            // CI (`windows-latest`) always has Microsoft's own software
            // "H264 Encoder MFT", so an explicit request must succeed there
            // even with no hardware encoder present.
            let (_, kind) = build_encoder(Some(EncoderKind::MediaFoundation), test_cfg())
                .expect("mediafoundation must build on windows (software MFT if no hardware)");
            assert_eq!(kind, EncoderKind::MediaFoundation);

            // "Auto" accepts the software MFT too, so on CI (no hardware
            // encoder) it must still resolve to Media Foundation.
            let (_, kind) = build_encoder(None, test_cfg()).expect("auto must build on windows");
            assert_eq!(kind, EncoderKind::MediaFoundation);
        }

        #[cfg(not(any(target_os = "macos", target_os = "windows")))]
        {
            assert!(matches!(
                build_encoder(Some(EncoderKind::VideoToolbox), test_cfg()),
                Err(EncodeError::Unsupported(_))
            ));
            assert!(matches!(
                build_encoder(Some(EncoderKind::MediaFoundation), test_cfg()),
                Err(EncodeError::Unsupported(_))
            ));

            let (_, kind) = build_encoder(None, test_cfg()).expect("auto must build openh264");
            assert_eq!(kind, EncoderKind::OpenH264);
        }
    }
}
