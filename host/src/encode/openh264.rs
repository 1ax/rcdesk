//! `openh264`-backed `Encoder`: software H.264, built from source (the
//! `source` feature, on by default) so it compiles the same way on all three
//! target OSes without needing a system-provided shared library.

use std::ffi::c_int;
use std::time::Instant;

use ::openh264::encoder::{
    BitRate, Encoder as Oh264Encoder, EncoderConfig as Oh264Config, FrameRate, FrameType,
    IntraFramePeriod, Profile, QpRange, RateControlMode, SpsPpsStrategy, UsageType,
};
use ::openh264::formats::YUVSlices;
use ::openh264::OpenH264API;
use ::openh264::Timestamp;
use openh264_sys2::{
    SBitrateInfo, ENCODER_OPTION_BITRATE, ENCODER_OPTION_FRAME_RATE, ENCODER_OPTION_MAX_BITRATE,
    SPATIAL_LAYER_ALL,
};

use crate::capture::RawFrame;

use super::{to_i420, EncodeError, EncodedFrame, Encoder, EncoderConfig, RateTarget};

/// Lower QP bound passed to openh264 alongside `EncoderConfig::max_qp`; see
/// the comment at the call site for why it can't be 0.
const MIN_QP: u8 = 1;

/// QP ceiling used when `EncoderConfig::max_qp` is `None`. Chosen on the
/// owner's Windows 10 bench (GDI 1080p, slice 2.2): without a ceiling
/// openh264's delta frames sit at QP > 34 and typed text is visibly blurred;
/// at 30 the text is crisp and bitrate under a playing video stays at
/// 2–3.5 Mbit/s of the 6 Mbit/s target. `--max-qp` overrides it.
const DEFAULT_MAX_QP: u8 = 30;

pub struct OpenH264Encoder {
    inner: Oh264Encoder,
    /// Wall-clock reference for `Timestamp::from_millis` below: the encoder
    /// needs monotonically increasing millisecond timestamps for its rate
    /// control, not frame-count-derived ones (see
    /// `docs/host-libs-api-notes.md`'s webrtc section for why `frame_count *
    /// 1000 / fps` drifted from real time for a variable-rate source).
    started: Instant,
}

impl OpenH264Encoder {
    pub fn new(cfg: EncoderConfig) -> Result<Self, EncodeError> {
        tracing::info!(
            width = cfg.width,
            height = cfg.height,
            fps = cfg.fps,
            bitrate_kbps = cfg.bitrate_kbps,
            max_qp = cfg.max_qp.unwrap_or(DEFAULT_MAX_QP),
            "initializing openh264 encoder"
        );

        let api = OpenH264API::from_source();
        let oh264_cfg = Oh264Config::new()
            .bitrate(BitRate::from_bps(cfg.bitrate_kbps.saturating_mul(1000)))
            .max_frame_rate(FrameRate::from_hz(cfg.fps as f32))
            .usage_type(UsageType::ScreenContentRealTime)
            .rate_control_mode(RateControlMode::Bitrate)
            .profile(Profile::Baseline)
            .sps_pps_strategy(SpsPpsStrategy::ConstantId)
            .intra_frame_period(IntraFramePeriod::from_num_frames(
                cfg.keyframe_interval_frames,
            ))
            .skip_frames(false)
            .num_threads(0);
        // The lower bound must be >= 1: openh264's `ParamValidationExt`
        // (`encoder_ext.cpp`, "Change QP Range") throws the *whole* range
        // away and reinstates its screen-content defaults when either bound
        // is <= 0, so `QpRange::new(0, max)` would silently disable the cap.
        // It clips the minimum up to its own floor anyway, so 1 is never
        // actually reached.
        let max_qp = cfg.max_qp.unwrap_or(DEFAULT_MAX_QP).clamp(MIN_QP, 51);
        let oh264_cfg = oh264_cfg.qp(QpRange::new(MIN_QP, max_qp));

        let inner = Oh264Encoder::with_api_config(api, oh264_cfg)
            .map_err(|err| EncodeError::Backend(err.to_string()))?;

        Ok(Self {
            inner,
            started: Instant::now(),
        })
    }
}

impl Encoder for OpenH264Encoder {
    fn encode(
        &mut self,
        frame: &RawFrame,
        force_keyframe: bool,
    ) -> Result<Option<EncodedFrame>, EncodeError> {
        let captured_at = frame.ts();
        let i420 = to_i420(frame);

        if force_keyframe {
            self.inner.force_intra_frame();
        }

        let w = i420.width as usize;
        let h = i420.height as usize;
        let cw = w.div_ceil(2);
        let source = YUVSlices::new((&i420.y, &i420.u, &i420.v), (w, h), (w, cw, cw));

        let ts_ms = self.started.elapsed().as_millis() as u64;
        let bitstream = self
            .inner
            .encode_at(&source, Timestamp::from_millis(ts_ms))
            .map_err(|err| EncodeError::Backend(err.to_string()))?;

        let data = bitstream.to_vec();
        if data.is_empty() {
            return Ok(None);
        }

        let keyframe =
            bitstream.frame_type() == FrameType::IDR || super::nal_types(&data).contains(&5);

        Ok(Some(EncodedFrame {
            data,
            keyframe,
            ts: Instant::now(),
            captured_at,
        }))
    }

    /// Changes bitrate and frame rate on the live encoder via openh264's raw
    /// `SetOption` API (`openh264`'s own `EncoderConfig` builder only applies
    /// at construction time, so slice 2.3's runtime rate control has to go
    /// through `openh264_sys2` directly). No keyframe is forced and SPS/PPS
    /// are untouched -- only the rate-control budget changes.
    fn set_rate(&mut self, target: RateTarget) -> Result<(), EncodeError> {
        tracing::debug!(
            bitrate_kbps = target.bitrate_kbps,
            fps = target.fps,
            "openh264 rate target"
        );

        let bitrate_bps = target.bitrate_kbps.saturating_mul(1000);
        let mut bitrate_info = SBitrateInfo {
            iLayer: SPATIAL_LAYER_ALL,
            iBitrate: bitrate_bps as c_int,
        };
        let mut frame_rate = target.fps as f32;

        // SAFETY: `raw_api()` requires a live encoder, which `self.inner` is
        // for the whole lifetime of this `&mut self` call; each `set_option`
        // call passes a pointer to a value of the type that
        // `ENCODER_OPTION`'s documentation (openh264's `codec_api.h`) pairs
        // with that option -- `SBitrateInfo` for both bitrate options, `f32`
        // for the frame rate -- and each pointer stays valid (its local
        // points at a stack value) for the duration of its call.
        unsafe {
            let raw = self.inner.raw_api();

            let status = raw.set_option(
                ENCODER_OPTION_BITRATE,
                (&mut bitrate_info as *mut SBitrateInfo).cast(),
            );
            if status != 0 {
                return Err(EncodeError::Backend(format!(
                    "openh264 SetOption(ENCODER_OPTION_BITRATE): {status}"
                )));
            }

            let status = raw.set_option(
                ENCODER_OPTION_MAX_BITRATE,
                (&mut bitrate_info as *mut SBitrateInfo).cast(),
            );
            if status != 0 {
                return Err(EncodeError::Backend(format!(
                    "openh264 SetOption(ENCODER_OPTION_MAX_BITRATE): {status}"
                )));
            }

            let status = raw.set_option(
                ENCODER_OPTION_FRAME_RATE,
                (&mut frame_rate as *mut f32).cast(),
            );
            if status != 0 {
                return Err(EncodeError::Backend(format!(
                    "openh264 SetOption(ENCODER_OPTION_FRAME_RATE): {status}"
                )));
            }
        }

        Ok(())
    }
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
    fn first_frame_is_keyframe_with_sps_pps_idr() {
        let mut source = SyntheticSource::new(64, 64, 1000);
        let mut encoder = OpenH264Encoder::new(test_cfg()).expect("encoder init");

        let raw = source.next_frame().expect("frame");
        let encoded = encoder
            .encode(&raw, false)
            .expect("encode")
            .expect("first frame must produce output");

        assert!(encoded.keyframe);
        let types = nal_types(&encoded.data);
        assert!(types.contains(&7), "missing SPS: {types:?}");
        assert!(types.contains(&8), "missing PPS: {types:?}");
        assert!(types.contains(&5), "missing IDR: {types:?}");
    }

    #[test]
    fn thirty_frames_have_deltas_respect_force_keyframe_and_stay_small() {
        let mut source = SyntheticSource::new(64, 64, 1000);
        let mut encoder = OpenH264Encoder::new(test_cfg()).expect("encoder init");

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
    fn max_qp_cap_encodes_keyframe_and_deltas() {
        let mut cfg = test_cfg();
        cfg.max_qp = Some(30);

        let mut source = SyntheticSource::new(64, 64, 1000);
        let mut encoder = OpenH264Encoder::new(cfg).expect("encoder init");

        let mut outputs = Vec::new();
        for i in 0..30u32 {
            let raw = source.next_frame().expect("frame");
            let encoded = encoder.encode(&raw, false).expect("encode");
            if let Some(encoded) = encoded {
                outputs.push((i, encoded));
            }
        }

        assert!(!outputs.is_empty());
        assert!(outputs[0].1.keyframe);
        let types = nal_types(&outputs[0].1.data);
        assert!(types.contains(&7), "missing SPS: {types:?}");
        assert!(types.contains(&8), "missing PPS: {types:?}");
        assert!(types.contains(&5), "missing IDR: {types:?}");

        let has_delta = outputs
            .iter()
            .skip(1)
            .take(10)
            .any(|(_, f)| nal_types(&f.data).contains(&1));
        assert!(
            has_delta,
            "expected at least one non-keyframe (NAL 1) among the next 10 frames"
        );
    }

    #[test]
    fn set_rate_lowers_delta_frame_size() {
        let mut cfg = test_cfg();
        cfg.width = 320;
        cfg.height = 240;
        cfg.fps = 30;
        cfg.bitrate_kbps = 4000;

        let mut source = SyntheticSource::new(320, 240, 1000);
        let mut encoder = OpenH264Encoder::new(cfg).expect("encoder init");

        let mut before = Vec::new();
        for _ in 0..40u32 {
            let raw = source.next_frame().expect("frame");
            let encoded = encoder
                .encode(&raw, false)
                .expect("encode")
                .expect("output for every frame in this run");
            before.push(encoded);
        }
        assert!(before[0].keyframe, "first frame must be a keyframe");
        let before_avg: f64 = before[10..40]
            .iter()
            .map(|f| f.data.len() as f64)
            .sum::<f64>()
            / 30.0;

        encoder
            .set_rate(RateTarget {
                bitrate_kbps: 200,
                fps: 30,
            })
            .expect("set_rate");

        let mut after = Vec::new();
        for _ in 0..40u32 {
            let raw = source.next_frame().expect("frame");
            let encoded = encoder
                .encode(&raw, false)
                .expect("encode")
                .expect("output for every frame in this run");
            after.push(encoded);
        }
        // set_rate must not recreate the encoder (no new IDR right after the
        // change).
        assert!(
            !nal_types(&after[0].data).contains(&5),
            "set_rate must not force a new IDR"
        );
        let after_avg: f64 = after[10..40]
            .iter()
            .map(|f| f.data.len() as f64)
            .sum::<f64>()
            / 30.0;

        assert!(
            after_avg < before_avg,
            "expected smaller delta frames after lowering bitrate: before={before_avg}, after={after_avg}"
        );
    }
}
