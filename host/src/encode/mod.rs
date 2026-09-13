//! H.264 encoding: turns `RawFrame`s into Annex-B access units.

use std::time::Instant;

use crate::capture::RawFrame;

pub mod convert;
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
    /// Windows hardware encoder (Media Foundation). Not implemented yet --
    /// lands in slice 2.2d.
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
/// `Some(EncoderKind::MediaFoundation)` always fails with
/// `EncodeError::Unsupported` in this slice: the Windows hardware backend
/// lands in 2.2d.
///
/// `None` ("auto") tries the platform's hardware encoder first on macOS,
/// falling back to openh264 with a `tracing::warn!` if building it fails;
/// on every other platform it goes straight to openh264 (there is no
/// hardware backend to try there yet).
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
    /// `None` leaves the encoder's own default (QP allowed up to 51, i.e.
    /// no extra cap beyond what the bitrate/rate control already produces).
    /// `Some(n)` caps the worst-case QP at `n`, putting a floor under frame
    /// quality on hard-to-compress content at the cost of the encoder
    /// possibly exceeding the target bitrate to hold that quality.
    pub max_qp: Option<u8>,
}

#[derive(Debug, thiserror::Error)]
pub enum EncodeError {
    #[error("encoder backend error: {0}")]
    Backend(String),
    /// Requested a hardware backend that isn't implemented in this build yet
    /// (VideoToolbox lands in 2.2c, Media Foundation in 2.2d).
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
        // Media Foundation isn't implemented on any platform yet (lands in
        // 2.2d).
        assert!(matches!(
            build_encoder(Some(EncoderKind::MediaFoundation), test_cfg()),
            Err(EncodeError::Unsupported(_))
        ));

        #[cfg(target_os = "macos")]
        {
            let (_, kind) = build_encoder(Some(EncoderKind::VideoToolbox), test_cfg())
                .expect("videotoolbox must build on macOS");
            assert_eq!(kind, EncoderKind::VideoToolbox);

            let (_, kind) = build_encoder(None, test_cfg()).expect("auto must build on macOS");
            assert_eq!(kind, EncoderKind::VideoToolbox);
        }

        #[cfg(not(target_os = "macos"))]
        {
            assert!(matches!(
                build_encoder(Some(EncoderKind::VideoToolbox), test_cfg()),
                Err(EncodeError::Unsupported(_))
            ));

            let (_, kind) = build_encoder(None, test_cfg()).expect("auto must build openh264");
            assert_eq!(kind, EncoderKind::OpenH264);
        }
    }
}
