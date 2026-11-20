//! Annex-B / slice QP parser shared by the encoder benches
//! (`vt_scroll_bench`, `mf_bench`). Platform independent.

#![allow(dead_code)]

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
    if (pps.weighted_pred && matches!(slice_type, 0 | 3)) || (pps.weighted_bipred_idc == 1 && is_b)
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
    // VideoToolbox-backed test below exists only on macOS; the module itself
    // is shared with the Windows-only `mf_bench`.
    #[cfg(target_os = "macos")]
    use rcdesk_host::capture::synthetic::SyntheticSource;
    #[cfg(target_os = "macos")]
    use rcdesk_host::capture::FrameSource;
    #[cfg(target_os = "macos")]
    use rcdesk_host::encode::videotoolbox::{VideoToolboxEncoder, VtProfile, VtTuning};
    #[cfg(target_os = "macos")]
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

    #[cfg(target_os = "macos")]
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
