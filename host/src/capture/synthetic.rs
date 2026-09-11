//! A synthetic `FrameSource` that needs no OS permission: it generates NV12
//! frames with a moving gradient so the encoder always sees inter-frame
//! change. Used by tests, CI and the `bench --synthetic` CLI path; the real
//! `scap` backend is exercised manually by whoever has screen-recording
//! permission on their machine.

use std::time::{Duration, Instant};

use super::{CaptureError, FrameSource, RawFrame};

pub struct SyntheticSource {
    width: u32,
    height: u32,
    frame_interval: Duration,
    next_tick: Instant,
    frame_index: u64,
}

impl SyntheticSource {
    pub fn new(width: u32, height: u32, fps: u32) -> Self {
        assert!(fps > 0, "fps must be positive");
        assert!(width > 0 && height > 0, "dimensions must be positive");

        Self {
            width,
            height,
            frame_interval: Duration::from_secs_f64(1.0 / f64::from(fps)),
            next_tick: Instant::now(),
            frame_index: 0,
        }
    }

    /// Renders one NV12 frame. The Y plane is a horizontal gradient that
    /// shifts every frame (`wrapping_add(shift)`), and the interleaved UV
    /// plane gets a small periodic tint -- together they guarantee a real
    /// pixel delta between consecutive frames, which is what the encoder
    /// needs to produce non-trivial P-frames.
    fn render(&self) -> (Vec<u8>, Vec<u8>) {
        let w = self.width as usize;
        let h = self.height as usize;
        let shift = (self.frame_index % 256) as u8;

        let mut y = vec![0u8; w * h];
        for row in 0..h {
            for col in 0..w {
                let gradient = ((col * 255) / w) as u8;
                y[row * w + col] = gradient.wrapping_add(shift);
            }
        }

        let uv_w = w / 2;
        let uv_h = h / 2;
        let tint = shift / 4;
        let mut uv = vec![128u8; uv_w * uv_h * 2];
        for px in uv.as_chunks_mut::<2>().0 {
            px[0] = 128u8.wrapping_add(tint);
            px[1] = 128u8.wrapping_sub(tint);
        }

        (y, uv)
    }
}

impl FrameSource for SyntheticSource {
    fn next_frame(&mut self) -> Result<RawFrame, CaptureError> {
        let now = Instant::now();
        if now < self.next_tick {
            std::thread::sleep(self.next_tick - now);
        }
        // Fixed-rate pacing: advance the schedule by one interval regardless
        // of how long this call took, so timing doesn't drift over time.
        self.next_tick += self.frame_interval;

        let (y, uv) = self.render();
        self.frame_index += 1;

        Ok(RawFrame::Nv12 {
            width: self.width,
            height: self.height,
            y,
            y_stride: self.width as usize,
            uv,
            uv_stride: self.width as usize,
            ts: Instant::now(),
        })
    }

    fn size(&self) -> (u32, u32) {
        (self.width, self.height)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn produces_even_sized_nv12_frames_that_change_over_time() {
        let mut source = SyntheticSource::new(16, 8, 1000);
        let first = source.next_frame().expect("first frame");
        let second = source.next_frame().expect("second frame");

        let (RawFrame::Nv12 { y: y1, .. }, RawFrame::Nv12 { y: y2, .. }) = (&first, &second) else {
            panic!("synthetic source must produce NV12 frames");
        };

        assert_eq!(source.size(), (16, 8));
        assert_ne!(y1, y2, "consecutive frames should differ");
    }
}
