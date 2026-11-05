//! Raw frame -> I420 conversion. openh264 only accepts I420 (planar 4:2:0),
//! so both capture backends (NV12 on macOS, BGRA on Windows) get converted
//! here before hitting the encoder.

use openh264::formats::{BgraSliceU8, YUVBuffer, YUVSource};

use crate::capture::RawFrame;

/// A dense (no row padding) I420 frame: three contiguous planes.
#[derive(Debug, Clone)]
pub struct I420Frame {
    pub width: u32,
    pub height: u32,
    pub y: Vec<u8>,
    pub u: Vec<u8>,
    pub v: Vec<u8>,
}

/// De-interleaves an NV12 frame (Y plane + interleaved CbCr plane, what
/// `scap` delivers on macOS) into a dense I420 frame.
pub fn nv12_to_i420(
    width: u32,
    height: u32,
    y: &[u8],
    y_stride: usize,
    uv: &[u8],
    uv_stride: usize,
) -> I420Frame {
    let w = width as usize;
    let h = height as usize;

    let mut y_plane = vec![0u8; w * h];
    for row in 0..h {
        y_plane[row * w..(row + 1) * w].copy_from_slice(&y[row * y_stride..row * y_stride + w]);
    }

    let cw = w.div_ceil(2);
    let ch = h.div_ceil(2);
    let mut u_plane = vec![0u8; cw * ch];
    let mut v_plane = vec![0u8; cw * ch];
    for row in 0..ch {
        let src_row = &uv[row * uv_stride..row * uv_stride + cw * 2];
        for col in 0..cw {
            u_plane[row * cw + col] = src_row[col * 2];
            v_plane[row * cw + col] = src_row[col * 2 + 1];
        }
    }

    I420Frame {
        width,
        height,
        y: y_plane,
        u: u_plane,
        v: v_plane,
    }
}

/// Converts a packed BGRA frame (what `scap` delivers on Windows) into I420.
///
/// Uses openh264's own `YUVBuffer::from_bgra8_source`, which is SIMD
/// accelerated and matches the crate's own limited-range BT.601-ish
/// coefficients (verified against the crate's unit tests: solid white maps
/// to Y=235, U=V=128 -- the same values this module's tests check for,
/// within a small tolerance). Writing a bespoke conversion would just
/// duplicate that logic with a real chance of picking a different matrix
/// than what the encoder's own SPS ends up implying.
pub fn bgra_to_i420(width: u32, height: u32, data: &[u8], stride: usize) -> I420Frame {
    let w = width as usize;
    let h = height as usize;
    let row_bytes = w * 4;

    let repacked;
    let packed: &[u8] = if stride == row_bytes {
        data
    } else {
        let mut buf = vec![0u8; row_bytes * h];
        for row in 0..h {
            buf[row * row_bytes..(row + 1) * row_bytes]
                .copy_from_slice(&data[row * stride..row * stride + row_bytes]);
        }
        repacked = buf;
        &repacked
    };

    let source = BgraSliceU8::new(packed, (w, h));
    let yuv = YUVBuffer::from_bgra8_source(source);

    I420Frame {
        width,
        height,
        y: yuv.y().to_vec(),
        u: yuv.u().to_vec(),
        v: yuv.v().to_vec(),
    }
}

/// Dispatches on the raw frame kind, cropping to even dimensions first
/// (openh264 requires even width/height).
pub fn to_i420(frame: &RawFrame) -> I420Frame {
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
            let (ew, eh) = even_dims(*width, *height);
            nv12_to_i420(ew, eh, y, *y_stride, uv, *uv_stride)
        }
        RawFrame::Bgra {
            width,
            height,
            data,
            stride,
            ..
        } => {
            let (ew, eh) = even_dims(*width, *height);
            bgra_to_i420(ew, eh, data, *stride)
        }
    }
}

fn even_dims(width: u32, height: u32) -> (u32, u32) {
    (width & !1, height & !1)
}

/// Length in bytes of the dense NV12 image `Nv12Writer::write` produces for
/// `frame`: `Y (w*h)` followed by the interleaved CbCr plane
/// `(ceil(w/2)*2) * ceil(h/2)`. `Bgra` frames are cropped to even dimensions
/// first (as in `to_i420`); `Nv12` frames keep their native size.
pub fn nv12_len(frame: &RawFrame) -> usize {
    let (w, h) = match frame {
        RawFrame::Nv12 { width, height, .. } => (*width, *height),
        RawFrame::Bgra { width, height, .. } => even_dims(*width, *height),
    };
    let (w, h) = (w as usize, h as usize);
    w * h + w.div_ceil(2) * 2 * h.div_ceil(2)
}

/// Writes frames as dense NV12 straight into a caller-provided buffer (the
/// Windows Media Foundation encoder's input type is NV12 rather than I420),
/// reusing one BGRA->I420 scratch image between calls so that the per-frame
/// path allocates nothing in the common (`stride == width * 4`) case.
#[derive(Default)]
pub struct Nv12Writer {
    scratch: Option<YUVBuffer>,
}

impl Nv12Writer {
    pub fn new() -> Self {
        Self::default()
    }

    /// Writes `frame` as dense NV12 into `dst`.
    ///
    /// # Panics
    ///
    /// If `dst.len() != nv12_len(frame)`.
    pub fn write(&mut self, frame: &RawFrame, dst: &mut [u8]) {
        assert_eq!(
            dst.len(),
            nv12_len(frame),
            "Nv12Writer::write: dst length must equal nv12_len(frame)"
        );
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
                let w = *width as usize;
                let h = *height as usize;
                let (y_dst, uv_dst) = dst.split_at_mut(w * h);
                for row in 0..h {
                    y_dst[row * w..(row + 1) * w]
                        .copy_from_slice(&y[row * y_stride..row * y_stride + w]);
                }
                let uv_h = h.div_ceil(2);
                let uv_row_bytes = w.div_ceil(2) * 2;
                for row in 0..uv_h {
                    uv_dst[row * uv_row_bytes..(row + 1) * uv_row_bytes]
                        .copy_from_slice(&uv[row * uv_stride..row * uv_stride + uv_row_bytes]);
                }
            }
            RawFrame::Bgra {
                width,
                height,
                data,
                stride,
                ..
            } => {
                let (ew, eh) = even_dims(*width, *height);
                let (w, h) = (ew as usize, eh as usize);
                let row_bytes = w * 4;

                let repacked;
                let packed: &[u8] = if *stride == row_bytes {
                    data
                } else {
                    let mut buf = vec![0u8; row_bytes * h];
                    for row in 0..h {
                        buf[row * row_bytes..(row + 1) * row_bytes]
                            .copy_from_slice(&data[row * stride..row * stride + row_bytes]);
                    }
                    repacked = buf;
                    &repacked
                };

                if self.scratch.as_ref().map(|s| s.dimensions()) != Some((w, h)) {
                    self.scratch = Some(YUVBuffer::new(w, h));
                }
                let Some(yuv) = self.scratch.as_mut() else {
                    unreachable!("scratch was just ensured above");
                };
                yuv.read_bgra8(BgraSliceU8::new(packed, (w, h)));

                let (y_dst, uv_dst) = dst.split_at_mut(w * h);
                y_dst.copy_from_slice(yuv.y());
                for ((pair, &u), &v) in uv_dst
                    .as_chunks_mut::<2>()
                    .0
                    .iter_mut()
                    .zip(yuv.u().iter())
                    .zip(yuv.v().iter())
                {
                    pair[0] = u;
                    pair[1] = v;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Instant;

    #[test]
    fn nv12_to_i420_deinterleaves_uv_exactly() {
        let y = vec![0u8, 1, 2, 3, 4, 5, 6, 7];
        let uv = vec![10u8, 20, 30, 40];

        let frame = nv12_to_i420(4, 2, &y, 4, &uv, 4);

        assert_eq!(frame.width, 4);
        assert_eq!(frame.height, 2);
        assert_eq!(frame.y, y);
        assert_eq!(frame.u, vec![10, 30]);
        assert_eq!(frame.v, vec![20, 40]);
    }

    #[test]
    fn bgra_to_i420_white_is_high_luma_neutral_chroma() {
        let (w, h) = (8usize, 8usize);
        let mut data = vec![0u8; w * h * 4];
        for px in data.as_chunks_mut::<4>().0 {
            px[0] = 255; // B
            px[1] = 255; // G
            px[2] = 255; // R
            px[3] = 255; // A
        }

        let frame = bgra_to_i420(w as u32, h as u32, &data, w * 4);

        assert_eq!(frame.y.len(), w * h);
        assert_eq!(frame.u.len(), (w / 2) * (h / 2));
        assert_eq!(frame.v.len(), (w / 2) * (h / 2));
        for &y in &frame.y {
            assert!((232..=238).contains(&y), "unexpected Y value {y}");
        }
        for &u in &frame.u {
            assert!((125..=131).contains(&u), "unexpected U value {u}");
        }
        for &v in &frame.v {
            assert!((125..=131).contains(&v), "unexpected V value {v}");
        }
    }

    /// The pre-V-5 path: `to_i420` + interleave (Bgra), de-stride (Nv12).
    fn reference_nv12(frame: &RawFrame) -> Vec<u8> {
        match frame {
            RawFrame::Bgra { .. } => {
                let i = to_i420(frame);
                let cw = (i.width as usize).div_ceil(2);
                let ch = (i.height as usize).div_ceil(2);
                let mut out = i.y.clone();
                for k in 0..cw * ch {
                    out.push(i.u[k]);
                    out.push(i.v[k]);
                }
                out
            }
            RawFrame::Nv12 {
                width,
                height,
                y,
                y_stride,
                uv,
                uv_stride,
                ..
            } => {
                let (w, h) = (*width as usize, *height as usize);
                let mut out = Vec::new();
                for row in 0..h {
                    out.extend_from_slice(&y[row * y_stride..row * y_stride + w]);
                }
                let rb = w.div_ceil(2) * 2;
                for row in 0..h.div_ceil(2) {
                    out.extend_from_slice(&uv[row * uv_stride..row * uv_stride + rb]);
                }
                out
            }
        }
    }

    fn pseudo(len: usize, seed: u32) -> Vec<u8> {
        let mut x = seed.wrapping_mul(2654435761).wrapping_add(12345);
        (0..len)
            .map(|_| {
                x ^= x << 13;
                x ^= x >> 17;
                x ^= x << 5;
                (x >> 8) as u8
            })
            .collect()
    }

    fn bgra_frame(w: u32, h: u32, stride: usize, seed: u32) -> RawFrame {
        RawFrame::Bgra {
            width: w,
            height: h,
            data: pseudo(stride * h as usize, seed),
            stride,
            ts: Instant::now(),
        }
    }

    fn nv12_frame(w: u32, h: u32, y_stride: usize, uv_stride: usize, seed: u32) -> RawFrame {
        RawFrame::Nv12 {
            width: w,
            height: h,
            y: pseudo(y_stride * h as usize, seed),
            y_stride,
            uv: pseudo(uv_stride * (h as usize).div_ceil(2), seed + 1),
            uv_stride,
            ts: Instant::now(),
        }
    }

    fn written(writer: &mut Nv12Writer, frame: &RawFrame) -> Vec<u8> {
        let mut dst = vec![0u8; nv12_len(frame)];
        writer.write(frame, &mut dst);
        dst
    }

    #[test]
    fn nv12_writer_bgra_matches_old_path() {
        let mut writer = Nv12Writer::new();
        for (w, h, stride) in [
            (16, 8, 64),
            (37, 21, 37 * 4),
            (37, 21, 37 * 4 + 20),
            (20, 10, 96),
        ] {
            let frame = bgra_frame(w, h, stride, w + h);
            assert_eq!(
                written(&mut writer, &frame),
                reference_nv12(&frame),
                "{w}x{h} stride {stride}"
            );
        }
    }

    #[test]
    fn nv12_writer_nv12_matches_old_destride() {
        let mut writer = Nv12Writer::new();
        for (w, h, ys, uvs) in [(16, 8, 16, 16), (16, 8, 24, 32), (17, 9, 20, 22)] {
            let frame = nv12_frame(w, h, ys, uvs, w * h);
            assert_eq!(
                written(&mut writer, &frame),
                reference_nv12(&frame),
                "{w}x{h}"
            );
        }
    }

    #[test]
    fn nv12_writer_reuse_across_sizes() {
        let mut writer = Nv12Writer::new();
        let a = bgra_frame(32, 16, 128, 1);
        let b = bgra_frame(18, 10, 18 * 4, 2);
        assert_eq!(written(&mut writer, &a), reference_nv12(&a));
        assert_eq!(written(&mut writer, &b), reference_nv12(&b));
        assert_eq!(written(&mut writer, &a), reference_nv12(&a));
    }

    #[test]
    #[should_panic(expected = "dst length must equal nv12_len")]
    fn nv12_writer_wrong_dst_len_panics() {
        let frame = bgra_frame(16, 8, 64, 3);
        let mut dst = vec![0u8; nv12_len(&frame) - 1];
        Nv12Writer::new().write(&frame, &mut dst);
    }

    #[test]
    fn to_i420_crops_odd_dimensions_to_even() {
        let raw = RawFrame::Nv12 {
            width: 5,
            height: 3,
            y: vec![0u8; 5 * 3],
            y_stride: 5,
            uv: vec![128u8; 5 * 2],
            uv_stride: 5,
            ts: Instant::now(),
        };

        let frame = to_i420(&raw);

        assert_eq!(frame.width, 4);
        assert_eq!(frame.height, 2);
        assert_eq!(frame.y.len(), 8);
        assert_eq!(frame.u.len(), 2);
        assert_eq!(frame.v.len(), 2);
    }
}
