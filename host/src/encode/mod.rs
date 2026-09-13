//! H.264 encoding: turns `I420Frame`s into Annex-B access units.

use std::time::Instant;

pub mod convert;
pub mod openh264;

pub use convert::{to_i420, I420Frame};

/// One encoded H.264 access unit, Annex-B (start-code prefixed NAL units
/// concatenated together).
#[derive(Debug, Clone)]
pub struct EncodedFrame {
    pub data: Vec<u8>,
    pub keyframe: bool,
    pub ts: Instant,
    /// Wall-clock time the *source* frame was captured (`RawFrame::ts()`),
    /// as opposed to `ts` (when encoding finished). The encoder doesn't see
    /// the raw frame, so it fills this with a placeholder; `pipeline::start`
    /// overwrites it with the real value right after `encode()` returns.
    /// The transport uses it to stamp RTP timestamps from real capture
    /// gaps rather than a fixed `1/fps` step (see
    /// `docs/host-libs-api-notes.md`'s webrtc section).
    pub captured_at: Instant,
}

pub trait Encoder: Send {
    fn encode(
        &mut self,
        frame: &I420Frame,
        force_keyframe: bool,
    ) -> Result<Option<EncodedFrame>, EncodeError>;
}

#[derive(Debug, Clone, Copy)]
pub struct EncoderConfig {
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    pub bitrate_kbps: u32,
    pub keyframe_interval_frames: u32,
}

#[derive(Debug, thiserror::Error)]
pub enum EncodeError {
    #[error("encoder backend error: {0}")]
    Backend(String),
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
}
