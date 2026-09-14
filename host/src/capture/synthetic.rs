//! A synthetic `FrameSource` that needs no OS permission: it generates NV12
//! frames with a moving gradient so the encoder always sees inter-frame
//! change. Used by tests, CI and the `bench --synthetic` CLI path; the real
//! `scap` backend is exercised manually by whoever has screen-recording
//! permission on their machine.

use std::time::Instant;

use super::{CaptureError, DisplayInfo, FramePacer, FrameSource, RawFrame};

/// The fixed set of displays `for_display` can build a source for (slice
/// 2.4): two differently-sized/oriented synthetic screens side by side, so a
/// display switch is visible even with no real hardware.
pub fn list_displays() -> Vec<DisplayInfo> {
    vec![
        DisplayInfo {
            id: 1,
            title: "Synthetic 1".to_string(),
            x: 0,
            y: 0,
            width: 1280,
            height: 720,
            primary: true,
        },
        DisplayInfo {
            id: 2,
            title: "Synthetic 2".to_string(),
            x: 1280,
            y: 0,
            width: 1024,
            height: 768,
            primary: false,
        },
    ]
}

/// Builds the synthetic source for one of `list_displays`' entries: `id: 1`
/// is 1280x720 with the horizontal gradient `SyntheticSource` has always
/// rendered, `id: 2` is 1024x768 with a vertical gradient and an
/// opposite-signed tint, so switching between them is visible by eye, not
/// just by size.
pub fn for_display(id: u32, fps: u32) -> Result<SyntheticSource, CaptureError> {
    match id {
        1 => Ok(SyntheticSource::new(1280, 720, fps)),
        2 => Ok(SyntheticSource::new_vertical(1024, 768, fps)),
        other => Err(CaptureError::Backend(format!("display {other} not found"))),
    }
}

pub struct SyntheticSource {
    width: u32,
    height: u32,
    pacer: FramePacer,
    frame_index: u64,
    /// `false`: horizontal gradient (the original rendering). `true`:
    /// vertical gradient with an inverted tint sign, used for the second
    /// synthetic display (`for_display(2, ..)`) so a switch is visible.
    vertical: bool,
}

impl SyntheticSource {
    pub fn new(width: u32, height: u32, fps: u32) -> Self {
        assert!(fps > 0, "fps must be positive");
        assert!(width > 0 && height > 0, "dimensions must be positive");

        Self {
            width,
            height,
            pacer: FramePacer::new(fps),
            frame_index: 0,
            vertical: false,
        }
    }

    /// Same as `new`, but renders a vertical gradient with an inverted tint
    /// sign instead of the default horizontal one.
    fn new_vertical(width: u32, height: u32, fps: u32) -> Self {
        Self {
            vertical: true,
            ..Self::new(width, height, fps)
        }
    }

    /// Renders one NV12 frame. The Y plane is a gradient (horizontal or
    /// vertical, see `vertical`) that shifts every frame
    /// (`wrapping_add(shift)`), and the interleaved UV plane gets a small
    /// periodic tint (sign flipped for the vertical variant) -- together
    /// they guarantee a real pixel delta between consecutive frames, which
    /// is what the encoder needs to produce non-trivial P-frames.
    fn render(&self) -> (Vec<u8>, Vec<u8>) {
        let w = self.width as usize;
        let h = self.height as usize;
        let shift = (self.frame_index % 256) as u8;

        let mut y = vec![0u8; w * h];
        for row in 0..h {
            for col in 0..w {
                let gradient = if self.vertical {
                    ((row * 255) / h) as u8
                } else {
                    ((col * 255) / w) as u8
                };
                y[row * w + col] = gradient.wrapping_add(shift);
            }
        }

        let uv_w = w / 2;
        let uv_h = h / 2;
        let tint = shift / 4;
        let mut uv = vec![128u8; uv_w * uv_h * 2];
        for px in uv.as_chunks_mut::<2>().0 {
            if self.vertical {
                px[0] = 128u8.wrapping_sub(tint);
                px[1] = 128u8.wrapping_add(tint);
            } else {
                px[0] = 128u8.wrapping_add(tint);
                px[1] = 128u8.wrapping_sub(tint);
            }
        }

        (y, uv)
    }
}

impl FrameSource for SyntheticSource {
    fn next_frame(&mut self) -> Result<RawFrame, CaptureError> {
        // Fixed-rate pacing without catch-up bursts: if this source was
        // created well before its capture thread started (the loopback test
        // builds it before the WebRTC handshake), a naive `next_tick +=
        // interval` schedule would emit a burst of back-to-back frames to
        // "catch up" -- `FramePacer` snaps the schedule forward instead.
        self.pacer.wait();

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

    #[test]
    fn for_display_2_is_1024x768() {
        let source = for_display(2, 1000).expect("display 2 exists");
        assert_eq!(source.size(), (1024, 768));
    }

    #[test]
    fn for_display_2_renders_a_vertical_gradient_unlike_display_1() {
        let mut horizontal = for_display(1, 1000).expect("display 1 exists");
        let mut vertical = for_display(2, 1000).expect("display 2 exists");

        let (
            RawFrame::Nv12 {
                y: y1, width: w1, ..
            },
            RawFrame::Nv12 {
                y: y2, width: w2, ..
            },
        ) = (
            horizontal.next_frame().expect("display 1 frame"),
            vertical.next_frame().expect("display 2 frame"),
        )
        else {
            panic!("synthetic source must produce NV12 frames");
        };

        // Display 1's first row is a horizontal gradient (varies across the
        // row); display 2's first row is a vertical gradient, so at row 0 it
        // is constant across the row -- a cheap, size-independent way to
        // tell the two renderings apart.
        let first_row_1 = &y1[..w1 as usize];
        let first_row_2 = &y2[..w2 as usize];
        assert!(
            first_row_1.windows(2).any(|pair| pair[0] != pair[1]),
            "display 1's first row should vary (horizontal gradient)"
        );
        assert!(
            first_row_2.windows(2).all(|pair| pair[0] == pair[1]),
            "display 2's first row should be constant (vertical gradient)"
        );
    }

    #[test]
    fn for_display_unknown_id_is_an_error() {
        assert!(for_display(3, 30).is_err());
    }
}
