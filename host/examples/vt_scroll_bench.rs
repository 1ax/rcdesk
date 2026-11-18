//! Measurement tool: how does the VideoToolbox encoder behave on scrolling
//! text after an idle period? Builds a procedural text page, feeds it to a
//! fresh VT session per variant in real time (idle: one frame / 500 ms,
//! scroll: 30 fps), parses slice QP from the Annex-B output and prints one
//! Markdown table. Not used by the production host.
//!
//! `cargo run --release -p rcdesk-host --example vt_scroll_bench -- --help`

#[cfg(target_os = "macos")]
mod mac {
    use std::time::{Duration, Instant};

    use clap::Parser;
    use rcdesk_host::capture::RawFrame;
    use rcdesk_host::encode::videotoolbox::{VideoToolboxEncoder, VtProfile, VtTuning};
    use rcdesk_host::encode::{Encoder, EncoderConfig};

    // ------------------------------------------------------------------
    // Annex-B / slice QP parser
    // ------------------------------------------------------------------
    pub mod qp {
        /// Bit reader over RBSP bytes (emulation prevention already removed).
        pub struct Bits<'a> {
            data: &'a [u8],
            pos: usize,
        }

        impl<'a> Bits<'a> {
            pub fn new(data: &'a [u8]) -> Self {
                Self { data, pos: 0 }
            }
            pub fn u1(&mut self) -> Option<u32> {
                let byte = *self.data.get(self.pos / 8)?;
                let bit = (byte >> (7 - self.pos % 8)) & 1;
                self.pos += 1;
                Some(u32::from(bit))
            }
            pub fn u(&mut self, n: u32) -> Option<u32> {
                let mut v = 0u32;
                for _ in 0..n {
                    v = (v << 1) | self.u1()?;
                }
                Some(v)
            }
            pub fn ue(&mut self) -> Option<u32> {
                let mut zeros = 0;
                while self.u1()? == 0 {
                    zeros += 1;
                    if zeros > 31 {
                        return None;
                    }
                }
                let rest = self.u(zeros)?;
                Some((1u32 << zeros) - 1 + rest)
            }
            pub fn se(&mut self) -> Option<i32> {
                let k = self.ue()?;
                let mag = k.div_ceil(2) as i32;
                Some(if k % 2 == 1 { mag } else { -mag })
            }
        }

        /// Splits an Annex-B stream into NAL units (header byte included).
        pub fn split_nals(annexb: &[u8]) -> Vec<&[u8]> {
            let mut starts = Vec::new();
            let mut i = 0;
            while i + 3 <= annexb.len() {
                if annexb[i] == 0 && annexb[i + 1] == 0 && annexb[i + 2] == 1 {
                    starts.push(i + 3);
                    i += 3;
                } else {
                    i += 1;
                }
            }
            let mut out = Vec::new();
            for (k, &s) in starts.iter().enumerate() {
                let mut e = starts.get(k + 1).map_or(annexb.len(), |n| n - 3);
                while e > s && annexb[e - 1] == 0 {
                    e -= 1; // trailing zero of a 4-byte start code
                }
                out.push(&annexb[s..e]);
            }
            out
        }

        /// Removes emulation-prevention bytes (00 00 03 -> 00 00).
        pub fn unescape(nal_body: &[u8]) -> Vec<u8> {
            let mut out = Vec::with_capacity(nal_body.len());
            let mut zeros = 0;
            for &b in nal_body {
                if zeros >= 2 && b == 3 {
                    zeros = 0;
                    continue;
                }
                zeros = if b == 0 { zeros + 1 } else { 0 };
                out.push(b);
            }
            out
        }

        #[derive(Debug, Clone, Default, PartialEq)]
        pub struct Sps {
            pub profile_idc: u32,
            pub chroma_format_idc: u32,
            pub separate_colour_plane: bool,
            pub log2_max_frame_num: u32,
            pub poc_type: u32,
            pub log2_max_poc_lsb: u32,
            pub delta_pic_order_always_zero: bool,
            pub frame_mbs_only: bool,
        }

        #[derive(Debug, Clone, Default, PartialEq)]
        pub struct Pps {
            pub cabac: bool,
            pub bottom_field_pic_order: bool,
            pub num_ref_idx_l0_default: u32,
            pub num_ref_idx_l1_default: u32,
            pub weighted_pred: bool,
            pub weighted_bipred_idc: u32,
            pub pic_init_qp: i32,
            pub redundant_pic_cnt_present: bool,
        }

        fn skip_scaling_list(b: &mut Bits, size: usize) -> Option<()> {
            let (mut last, mut next) = (8i32, 8i32);
            for _ in 0..size {
                if next != 0 {
                    next = (last + b.se()? + 256) % 256;
                }
                if next != 0 {
                    last = next;
                }
            }
            Some(())
        }

        /// `nal` = NAL unit without start code, header byte included.
        pub fn parse_sps(nal: &[u8]) -> Option<Sps> {
            let rbsp = unescape(nal.get(1..)?);
            let mut b = Bits::new(&rbsp);
            let profile_idc = b.u(8)?;
            b.u(8)?; // constraint flags + reserved
            b.u(8)?; // level_idc
            b.ue()?; // sps_id
            let mut s = Sps {
                profile_idc,
                chroma_format_idc: 1,
                ..Sps::default()
            };
            if matches!(
                profile_idc,
                100 | 110 | 122 | 244 | 44 | 83 | 86 | 118 | 128 | 138 | 139 | 134 | 135
            ) {
                s.chroma_format_idc = b.ue()?;
                if s.chroma_format_idc == 3 {
                    s.separate_colour_plane = b.u1()? == 1;
                }
                b.ue()?; // bit_depth_luma_minus8
                b.ue()?; // bit_depth_chroma_minus8
                b.u1()?; // qpprime_y_zero_transform_bypass
                if b.u1()? == 1 {
                    let n = if s.chroma_format_idc != 3 { 8 } else { 12 };
                    for i in 0..n {
                        if b.u1()? == 1 {
                            skip_scaling_list(&mut b, if i < 6 { 16 } else { 64 })?;
                        }
                    }
                }
            }
            s.log2_max_frame_num = b.ue()? + 4;
            s.poc_type = b.ue()?;
            if s.poc_type == 0 {
                s.log2_max_poc_lsb = b.ue()? + 4;
            } else if s.poc_type == 1 {
                s.delta_pic_order_always_zero = b.u1()? == 1;
                b.se()?;
                b.se()?;
                let n = b.ue()?;
                for _ in 0..n {
                    b.se()?;
                }
            }
            b.ue()?; // max_num_ref_frames
            b.u1()?; // gaps_in_frame_num_value_allowed
            b.ue()?; // width in mbs - 1
            b.ue()?; // height in map units - 1
            s.frame_mbs_only = b.u1()? == 1;
            Some(s)
        }

        pub fn parse_pps(nal: &[u8]) -> Option<Pps> {
            let rbsp = unescape(nal.get(1..)?);
            let mut b = Bits::new(&rbsp);
            b.ue()?; // pps_id
            b.ue()?; // sps_id
            let mut p = Pps {
                cabac: b.u1()? == 1,
                bottom_field_pic_order: b.u1()? == 1,
                ..Pps::default()
            };
            if b.ue()? != 0 {
                return None; // slice groups (FMO): not produced by VT
            }
            p.num_ref_idx_l0_default = b.ue()? + 1;
            p.num_ref_idx_l1_default = b.ue()? + 1;
            p.weighted_pred = b.u1()? == 1;
            p.weighted_bipred_idc = b.u(2)?;
            p.pic_init_qp = 26 + b.se()?;
            b.se()?; // pic_init_qs
            b.se()?; // chroma_qp_index_offset
            b.u1()?; // deblocking_filter_control_present
            b.u1()?; // constrained_intra_pred
            p.redundant_pic_cnt_present = b.u1()? == 1;
            Some(p)
        }

        /// Slice QP (26 + pic_init_qp_minus26 + slice_qp_delta) of one slice
        /// NAL (type 1 or 5). `None` for unsupported syntax (B slices with
        /// weighted prediction, FMO, ...).
        pub fn slice_qp(nal: &[u8], sps: &Sps, pps: &Pps) -> Option<i32> {
            let nal_ref_idc = (nal.first()? >> 5) & 3;
            let nal_type = nal.first()? & 0x1F;
            let rbsp = unescape(nal.get(1..)?);
            let mut b = Bits::new(&rbsp);
            b.ue()?; // first_mb_in_slice
            let slice_type = b.ue()? % 5; // 0 P, 1 B, 2 I, 3 SP, 4 SI
            b.ue()?; // pps_id
            if sps.separate_colour_plane {
                b.u(2)?;
            }
            b.u(sps.log2_max_frame_num)?;
            let mut field_pic = false;
            if !sps.frame_mbs_only {
                field_pic = b.u1()? == 1;
                if field_pic {
                    b.u1()?;
                }
            }
            if nal_type == 5 {
                b.ue()?; // idr_pic_id
            }
            if sps.poc_type == 0 {
                b.u(sps.log2_max_poc_lsb)?;
                if pps.bottom_field_pic_order && !field_pic {
                    b.se()?;
                }
            } else if sps.poc_type == 1 && !sps.delta_pic_order_always_zero {
                b.se()?;
                if pps.bottom_field_pic_order && !field_pic {
                    b.se()?;
                }
            }
            if pps.redundant_pic_cnt_present {
                b.ue()?;
            }
            let is_b = slice_type == 1;
            let is_pb = matches!(slice_type, 0 | 1 | 3);
            if is_b {
                b.u1()?; // direct_spatial_mv_pred
            }
            let (mut n0, mut n1) = (pps.num_ref_idx_l0_default, pps.num_ref_idx_l1_default);
            if is_pb && b.u1()? == 1 {
                n0 = b.ue()? + 1;
                if is_b {
                    n1 = b.ue()? + 1;
                }
            }
            if !matches!(slice_type, 2 | 4) {
                for _ in 0..(1 + u32::from(is_b)) {
                    if b.u1()? == 1 {
                        loop {
                            let idc = b.ue()?;
                            if idc == 3 {
                                break;
                            }
                            if idc > 5 {
                                return None;
                            }
                            b.ue()?;
                        }
                    }
                }
            }
            if (pps.weighted_pred && matches!(slice_type, 0 | 3))
                || (pps.weighted_bipred_idc == 1 && is_b)
            {
                b.ue()?; // luma_log2_weight_denom
                let chroma = sps.chroma_format_idc != 0 && !sps.separate_colour_plane;
                if chroma {
                    b.ue()?;
                }
                for _ in 0..n0 {
                    if b.u1()? == 1 {
                        b.se()?;
                        b.se()?;
                    }
                    if chroma && b.u1()? == 1 {
                        for _ in 0..4 {
                            b.se()?;
                        }
                    }
                }
                if is_b {
                    for _ in 0..n1 {
                        if b.u1()? == 1 {
                            b.se()?;
                            b.se()?;
                        }
                        if chroma && b.u1()? == 1 {
                            for _ in 0..4 {
                                b.se()?;
                            }
                        }
                    }
                }
            }
            if nal_ref_idc != 0 {
                if nal_type == 5 {
                    b.u1()?;
                    b.u1()?;
                } else if b.u1()? == 1 {
                    loop {
                        match b.ue()? {
                            0 => break,
                            1 | 2 | 4 | 6 => {
                                b.ue()?;
                            }
                            3 => {
                                b.ue()?;
                                b.ue()?;
                            }
                            5 => {}
                            _ => return None,
                        }
                    }
                }
            }
            if pps.cabac && !matches!(slice_type, 2 | 4) {
                b.ue()?; // cabac_init_idc
            }
            Some(pps.pic_init_qp + b.se()?)
        }

        /// Stateful per-stream parser: remembers the last SPS/PPS.
        #[derive(Default)]
        pub struct QpParser {
            sps: Option<Sps>,
            pps: Option<Pps>,
        }

        impl QpParser {
            /// QP of the first slice of the frame, `None` if unparseable.
            pub fn frame_qp(&mut self, annexb: &[u8]) -> Option<i32> {
                let mut result = None;
                for nal in split_nals(annexb) {
                    match nal.first()? & 0x1F {
                        7 => self.sps = parse_sps(nal),
                        8 => self.pps = parse_pps(nal),
                        1 | 5 if result.is_none() => {
                            result = slice_qp(nal, self.sps.as_ref()?, self.pps.as_ref()?);
                        }
                        _ => {}
                    }
                }
                result
            }
        }

        #[cfg(test)]
        mod tests {
            use super::*;
            use rcdesk_host::capture::synthetic::SyntheticSource;
            use rcdesk_host::capture::FrameSource;
            use rcdesk_host::encode::videotoolbox::{VideoToolboxEncoder, VtProfile, VtTuning};
            use rcdesk_host::encode::{Encoder, EncoderConfig};

            fn bits(s: &str) -> Vec<u8> {
                let s: String = s.chars().filter(|c| !c.is_whitespace()).collect();
                let mut out = vec![0u8; s.len().div_ceil(8)];
                for (i, c) in s.chars().enumerate() {
                    if c == '1' {
                        out[i / 8] |= 0x80 >> (i % 8);
                    }
                }
                out
            }

            #[test]
            fn exp_golomb_ue() {
                let d = bits("1 010 011 00100 00101 00110 00111 0001000");
                let mut b = Bits::new(&d);
                for want in 0..=7 {
                    assert_eq!(b.ue(), Some(want));
                }
            }

            #[test]
            fn exp_golomb_se() {
                // ue 1,2,3,4 -> +1,-1,+2,-2 ; ue 0 -> 0
                let d = bits("010 011 00100 00101 1");
                let mut b = Bits::new(&d);
                for want in [1, -1, 2, -2, 0] {
                    assert_eq!(b.se(), Some(want));
                }
            }

            #[test]
            fn exp_golomb_truncated_is_none() {
                let d = bits("0000");
                assert_eq!(Bits::new(&d).ue(), None);
            }

            #[test]
            fn unescape_removes_emulation_prevention() {
                assert_eq!(unescape(&[0, 0, 3, 1, 0, 0, 3]), vec![0, 0, 1, 0, 0]);
            }

            #[test]
            fn real_vt_streams_parse_with_plausible_qp() {
                for profile in [
                    VtProfile::ConstrainedBaseline,
                    VtProfile::Main,
                    VtProfile::High,
                    VtProfile::ConstrainedHigh,
                ] {
                    let cfg = EncoderConfig {
                        width: 128,
                        height: 128,
                        fps: 30,
                        bitrate_kbps: 1000,
                        keyframe_interval_frames: 60,
                        max_qp: None,
                    };
                    let tuning = VtTuning {
                        profile,
                        ..VtTuning::default()
                    };
                    let mut enc = VideoToolboxEncoder::new_tuned(cfg, tuning).expect("init");
                    let mut src = SyntheticSource::new(128, 128, 1000);
                    let mut parser = QpParser::default();
                    let mut parsed = 0;
                    let mut saw_sps = false;
                    for _ in 0..40 {
                        let raw = src.next_frame().expect("frame");
                        let Some(out) = enc.encode(&raw, false).expect("encode") else {
                            continue;
                        };
                        if out.keyframe {
                            let sps = split_nals(&out.data)
                                .into_iter()
                                .find(|n| n[0] & 0x1F == 7)
                                .and_then(parse_sps)
                                .expect("SPS parses");
                            let want = match profile {
                                VtProfile::ConstrainedBaseline => 66,
                                VtProfile::Main => 77,
                                VtProfile::High | VtProfile::ConstrainedHigh => 100,
                            };
                            assert_eq!(sps.profile_idc, want, "{profile:?}");
                            saw_sps = true;
                        }
                        let q = parser
                            .frame_qp(&out.data)
                            .unwrap_or_else(|| panic!("{profile:?}: slice QP unparseable"));
                        assert!((0..=51).contains(&q), "{profile:?}: qp {q}");
                        parsed += 1;
                    }
                    assert!(saw_sps && parsed > 10, "{profile:?}: parsed {parsed}");
                }
            }
        }
    }

    // ------------------------------------------------------------------
    // Procedural text page
    // ------------------------------------------------------------------
    const W: usize = 1920;
    const H: usize = 1080;

    struct Rng(u64);
    impl Rng {
        fn next(&mut self) -> u64 {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            self.0
        }
        fn below(&mut self, n: u64) -> u64 {
            self.next() % n
        }
    }

    pub struct Page {
        height: usize,
        y: Vec<u8>,
        uv: Vec<u8>,
    }

    impl Page {
        fn rect(
            &mut self,
            x: usize,
            y: usize,
            w: usize,
            h: usize,
            luma: u8,
            cbcr: Option<(u8, u8)>,
        ) {
            for yy in y..(y + h).min(self.height) {
                for xx in x..(x + w).min(W) {
                    self.y[yy * W + xx] = luma;
                    if let Some((cb, cr)) = cbcr {
                        let i = (yy / 2) * W + (xx / 2) * 2;
                        self.uv[i] = cb;
                        self.uv[i + 1] = cr;
                    }
                }
            }
        }

        /// White page with glyph-like strokes: 18 px line pitch, ~11 px
        /// glyphs on a 7 px pitch, random words, rare coloured headings.
        pub fn generate(height: usize) -> Self {
            let mut p = Page {
                height,
                y: vec![235; W * height],
                uv: vec![128; W * (height / 2)],
            };
            let mut rng = Rng(0x9E37_79B9_7F4A_7C15);
            let mut line = 0usize;
            let mut y0 = 12usize;
            while y0 + 24 < height {
                let heading = line.is_multiple_of(29);
                let (gh, pitch, luma, color) = if heading {
                    (18, 11, 40u8, Some((200u8, 90u8)))
                } else if rng.below(12) == 0 {
                    (11, 7, 60, Some((190, 110))) // link-ish
                } else {
                    (11, 7, 28, None)
                };
                let mut x = 80usize;
                let right = W - 80;
                let last_word = if rng.below(5) == 0 { right / 2 } else { right };
                while x + 9 * pitch < last_word {
                    let letters = 2 + rng.below(8) as usize;
                    for _ in 0..letters {
                        let strokes = 2 + rng.below(3);
                        for _ in 0..strokes {
                            match rng.below(3) {
                                0 => {
                                    let sx = x + rng.below(4) as usize;
                                    let sh = gh / 2 + rng.below(gh as u64 / 2) as usize;
                                    p.rect(sx, y0 + gh - sh, 2, sh, luma, color);
                                }
                                1 => {
                                    let sy = y0 + rng.below(gh as u64 - 1) as usize;
                                    p.rect(x, sy, 5, 2, luma, color);
                                }
                                _ => {
                                    let sy = y0 + gh / 3 + rng.below(gh as u64 / 3) as usize;
                                    p.rect(x + 1, sy, 3, 3, luma, color);
                                }
                            }
                        }
                        x += pitch;
                    }
                    x += 10;
                }
                y0 += if heading { 30 } else { 18 };
                line += 1;
            }
            p
        }

        /// NV12 frame of the 1920x1080 window starting at (even) row `off`.
        pub fn window(&self, off: usize) -> RawFrame {
            let off = off & !1;
            assert!(off + H <= self.height);
            RawFrame::Nv12 {
                width: W as u32,
                height: H as u32,
                y: self.y[off * W..(off + H) * W].to_vec(),
                y_stride: W,
                uv: self.uv[(off / 2) * W..(off / 2 + H / 2) * W].to_vec(),
                uv_stride: W,
                ts: Instant::now(),
            }
        }
    }

    // ------------------------------------------------------------------
    // Variants
    // ------------------------------------------------------------------
    struct Variant {
        name: &'static str,
        tuning: VtTuning,
        max_qp: Option<u8>,
    }

    fn variants() -> Vec<Variant> {
        // Old production tuning (pre-measurement): the other variants in the
        // historical tables are expressed relative to it.
        let d = VtTuning {
            data_rate_limit: Some(1.0),
            avg_factor: 1.0,
            ..VtTuning::default()
        };
        let v = |name, tuning, max_qp| Variant {
            name,
            tuning,
            max_qp,
        };
        vec![
            v("old-base", d, None),
            v("base", VtTuning::default(), None),
            v(
                "no-drl",
                VtTuning {
                    data_rate_limit: None,
                    ..d
                },
                None,
            ),
            v(
                "drl-1.5",
                VtTuning {
                    data_rate_limit: Some(1.5),
                    ..d
                },
                None,
            ),
            v(
                "nodrl-avg0.6",
                VtTuning {
                    data_rate_limit: None,
                    avg_factor: 0.6,
                    ..d
                },
                None,
            ),
            v(
                "nodrl-avg0.75",
                VtTuning {
                    data_rate_limit: None,
                    avg_factor: 0.75,
                    ..d
                },
                None,
            ),
            v(
                "drl1.5-w3",
                VtTuning {
                    data_rate_limit: Some(1.5),
                    drl_window_s: 3.0,
                    ..d
                },
                None,
            ),
            v(
                "drl2-w3",
                VtTuning {
                    data_rate_limit: Some(2.0),
                    drl_window_s: 3.0,
                    ..d
                },
                None,
            ),
            v(
                "drl1.2-w2",
                VtTuning {
                    data_rate_limit: Some(1.2),
                    drl_window_s: 2.0,
                    ..d
                },
                None,
            ),
            v(
                "drl1.5-w3-avg0.75",
                VtTuning {
                    data_rate_limit: Some(1.5),
                    drl_window_s: 3.0,
                    avg_factor: 0.75,
                    ..d
                },
                None,
            ),
            v("qp36", d, Some(36)),
            v(
                "main",
                VtTuning {
                    profile: VtProfile::Main,
                    ..d
                },
                None,
            ),
            v(
                "high",
                VtTuning {
                    profile: VtProfile::High,
                    ..d
                },
                None,
            ),
            v(
                "chigh",
                VtTuning {
                    profile: VtProfile::ConstrainedHigh,
                    ..d
                },
                None,
            ),
            v(
                "llrc",
                VtTuning {
                    low_latency_rc: true,
                    ..d
                },
                None,
            ),
            v(
                "llrc-high",
                VtTuning {
                    low_latency_rc: true,
                    profile: VtProfile::High,
                    ..d
                },
                None,
            ),
            v(
                "llrc-qp36",
                VtTuning {
                    low_latency_rc: true,
                    ..d
                },
                Some(36),
            ),
        ]
    }

    // ------------------------------------------------------------------
    // Run + metrics
    // ------------------------------------------------------------------
    #[derive(Parser)]
    #[command(about = "VideoToolbox scroll-after-idle encoder bench")]
    struct Args {
        /// Target bitrate, kbit/s.
        #[arg(long, default_value_t = 6000)]
        bitrate: u32,
        #[arg(long, default_value_t = 30)]
        fps: u32,
        /// Scroll speed, px per frame (rounded down to even).
        #[arg(long, default_value_t = 40)]
        speed: usize,
        /// Variant name; repeatable; none = all.
        #[arg(long = "variant")]
        variants: Vec<String>,
        #[arg(long, default_value_t = 4)]
        cycles: usize,
    }

    struct Sample {
        t: f64, // seconds since phase start
        bytes: usize,
        qp: Option<i32>,
        key: bool,
        enc_ms: f64,
    }

    #[derive(Default)]
    struct Cycle {
        idle: Vec<Sample>,
        scroll: Vec<Sample>,
        scroll_fed: usize,
    }

    fn sleep_until(t: Instant) {
        let now = Instant::now();
        if t > now {
            std::thread::sleep(t - now);
        }
    }

    fn run_variant(var: &Variant, args: &Args, page: &Page) -> Result<Vec<Cycle>, String> {
        let cfg = EncoderConfig {
            width: W as u32,
            height: H as u32,
            fps: args.fps,
            bitrate_kbps: args.bitrate,
            keyframe_interval_frames: args.fps.max(1) * 2,
            max_qp: var.max_qp,
        };
        eprintln!("# variant {}", var.name);
        let mut enc = VideoToolboxEncoder::new_tuned(cfg, var.tuning).map_err(|e| e.to_string())?;
        let mut parser = qp::QpParser::default();
        let speed = args.speed & !1;
        let frame_dt = Duration::from_secs_f64(1.0 / f64::from(args.fps));
        let scroll_frames = (2.0 * f64::from(args.fps)) as usize;
        let mut off = 0usize;
        let mut first = true;
        let mut cycles = Vec::new();

        for _ in 0..args.cycles {
            let mut cy = Cycle::default();
            let mut feed = |frame: &mut RawFrame,
                            t0: Instant,
                            sink: &mut Vec<Sample>,
                            enc: &mut VideoToolboxEncoder,
                            first: &mut bool|
             -> Result<(), String> {
                let now = Instant::now();
                frame.set_ts(now);
                let t = now.duration_since(t0).as_secs_f64();
                let started = Instant::now();
                let out = enc
                    .encode(frame, std::mem::take(first))
                    .map_err(|e| e.to_string())?;
                let enc_ms = started.elapsed().as_secs_f64() * 1e3;
                if let Some(out) = out {
                    let qp = parser.frame_qp(&out.data);
                    sink.push(Sample {
                        t,
                        bytes: out.data.len(),
                        qp,
                        key: out.keyframe,
                        enc_ms,
                    });
                }
                Ok(())
            };

            // Idle: same frame every 500 ms for 3 s.
            let t0 = Instant::now();
            for i in 0..6u32 {
                let mut f = page.window(off);
                sleep_until(t0 + Duration::from_millis(500) * i);
                feed(&mut f, t0, &mut cy.idle, &mut enc, &mut first)?;
            }
            sleep_until(t0 + Duration::from_secs(3));

            // Scroll: `fps` frames/s for 2 s.
            let t0 = Instant::now();
            for i in 0..scroll_frames {
                off += speed;
                let mut f = page.window(off);
                sleep_until(t0 + frame_dt * i as u32);
                feed(&mut f, t0, &mut cy.scroll, &mut enc, &mut first)?;
                cy.scroll_fed += 1;
            }
            cycles.push(cy);
        }
        Ok(cycles)
    }

    fn kbps(samples: &[Sample], window: f64, dur: f64) -> f64 {
        let bytes: usize = samples
            .iter()
            .filter(|s| s.t < window)
            .map(|s| s.bytes)
            .sum();
        bytes as f64 * 8.0 / dur / 1000.0
    }

    fn mean(v: &[f64]) -> f64 {
        if v.is_empty() {
            f64::NAN
        } else {
            v.iter().sum::<f64>() / v.len() as f64
        }
    }

    fn row(name: &str, cycles: &[Cycle]) -> String {
        let per = |window: f64, c: &Cycle| kbps(&c.scroll, window, window);
        let c1 = &cycles[0];
        let avg = |w: f64| mean(&cycles.iter().map(|c| per(w, c)).collect::<Vec<_>>());
        let idle = mean(
            &cycles
                .iter()
                .map(|c| kbps(&c.idle, 3.0, 3.0))
                .collect::<Vec<_>>(),
        );
        let all: Vec<&Sample> = cycles.iter().flat_map(|c| c.scroll.iter()).collect();
        let fed: usize = cycles.iter().map(|c| c.scroll_fed).sum();
        let bpf = all.iter().map(|s| s.bytes).sum::<usize>() as f64 / all.len().max(1) as f64;
        let qps: Vec<f64> = all.iter().filter_map(|s| s.qp.map(f64::from)).collect();
        let qps_early: Vec<f64> = all
            .iter()
            .filter(|s| s.t < 0.5)
            .filter_map(|s| s.qp.map(f64::from))
            .collect();
        let qp_known = qps.len() == all.len() && !qps.is_empty();
        let qp_avg = if qp_known {
            format!("{:.1}", mean(&qps))
        } else {
            "?".into()
        };
        let qp_early = if qps_early.is_empty() {
            "?".to_string()
        } else {
            format!("{:.1}", mean(&qps_early))
        };
        let qp_max = if qp_known {
            format!("{:.0}", qps.iter().cloned().fold(f64::MIN, f64::max))
        } else {
            "?".into()
        };
        let keys = all.iter().filter(|s| s.key).count();
        let enc_ms = mean(&all.iter().map(|s| s.enc_ms).collect::<Vec<_>>());
        format!(
            "| {name} | {idle:.0} | {:.0} | {:.0} | {:.0} | {:.0} | {:.0} | {:.0} | {bpf:.0} | {qp_avg} | {qp_early} | {qp_max} | {keys} | {} / {fed} | {enc_ms:.1} |",
            per(0.5, c1),
            per(1.0, c1),
            per(2.0, c1),
            avg(0.5),
            avg(1.0),
            avg(2.0),
            all.len(),
        )
    }

    pub fn main() {
        tracing_subscriber::fmt()
            .with_env_filter(
                tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| {
                    tracing_subscriber::EnvFilter::new(
                        "warn,rcdesk_host::encode::videotoolbox=debug",
                    )
                }),
            )
            .with_writer(std::io::stderr)
            .init();
        let args = Args::parse();
        let all = variants();
        let wanted: Vec<&Variant> = if args.variants.is_empty() {
            all.iter().collect()
        } else {
            args.variants
                .iter()
                .map(|n| {
                    all.iter().find(|v| v.name == n).unwrap_or_else(|| {
                        let names: Vec<_> = all.iter().map(|v| v.name).collect();
                        panic!("unknown variant {n}; known: {names:?}")
                    })
                })
                .collect()
        };

        let page_h = H + args.speed * (2 * args.fps as usize) * args.cycles + 8;
        eprintln!("# generating {W}x{page_h} page");
        let page = Page::generate(page_h);

        println!(
            "bitrate {} kbit/s, {} fps, speed {} px/frame, {} cycles (idle 3 s @ 2 fps, scroll 2 s)\n",
            args.bitrate, args.fps, args.speed, args.cycles
        );
        println!(
            "| variant | idle kbps | c1 0.5s | c1 1s | c1 2s | avg 0.5s | avg 1s | avg 2s | B/frame | QP avg | QP avg 0.5s | QP max | KF | frames out/fed | enc ms |"
        );
        println!("|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|");
        for v in wanted {
            match run_variant(v, &args, &page) {
                Ok(c) => println!("{}", row(v.name, &c)),
                Err(e) => println!("| {} | ERR: {e} |", v.name),
            }
        }
    }
}

#[cfg(target_os = "macos")]
fn main() {
    mac::main();
}

#[cfg(not(target_os = "macos"))]
fn main() {
    println!("macOS only");
}
