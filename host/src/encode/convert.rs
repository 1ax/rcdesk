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

/// Interleaves an I420 frame's U/V planes into NV12 (Y plane unchanged,
/// followed by a dense interleaved UV plane: U0 V0 U1 V1 ...). Used by the
/// Windows Media Foundation encoder, whose input type is NV12 rather than
/// I420 -- see `mediafoundation.rs`.
///
/// The returned Y plane is a copy of `frame.y` (both dense, so this could be
/// `frame.y.clone()`, but returning an owned `Vec` from both plane
/// computations keeps the two return values symmetric for callers).
pub fn i420_to_nv12(frame: &I420Frame) -> (Vec<u8>, Vec<u8>) {
    let w = frame.width as usize;
    let h = frame.height as usize;
    let cw = w.div_ceil(2);
    let ch = h.div_ceil(2);

    let y = frame.y.clone();

    let mut uv = vec![0u8; cw * 2 * ch];
    for row in 0..ch {
        for col in 0..cw {
            let src = row * cw + col;
            uv[row * cw * 2 + col * 2] = frame.u[src];
            uv[row * cw * 2 + col * 2 + 1] = frame.v[src];
        }
    }

    (y, uv)
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

    #[test]
    fn i420_to_nv12_interleaves_uv_exactly() {
        // 4x2 I420: Y unchanged, U/V (2x1 each) interleave to U0 V0 U1 V1.
        let frame = I420Frame {
            width: 4,
            height: 2,
            y: vec![0u8, 1, 2, 3, 4, 5, 6, 7],
            u: vec![10u8, 30],
            v: vec![20u8, 40],
        };

        let (y, uv) = i420_to_nv12(&frame);

        assert_eq!(y, frame.y);
        assert_eq!(uv, vec![10, 20, 30, 40]);
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
