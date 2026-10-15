//! macOS system cursor shape via `NSCursor.currentSystemCursor`.
//!
//! `currentSystemCursor` is deprecated in favor of ScreenCaptureKit's
//! `SCStreamConfiguration.showsCursor` -- but that only controls whether the
//! cursor is *baked into captured frames* (which this host already turns
//! off, see `crate::capture`'s `show_cursor: false`), it has no API to read
//! the cursor's current *shape*. `NSCursor.currentCursor` (the suggested,
//! non-deprecated alternative) isn't a substitute either: per its own doc
//! comment in `objc2-app-kit`, it "isn't necessarily the cursor that is
//! currently being displayed, as the system may be showing the cursor for
//! another running application" -- exactly the system-wide shape this
//! module needs. `currentSystemCursor` remains the only API that reports
//! it, hence the `#[allow(deprecated)]` below.
//!
//! `current()` runs ~30 times/s on a plain Rust thread, which has no
//! autorelease pool: every autoreleased `NSCursor`/`NSImage`/bitmap would
//! leak (~15 MB/s). All AppKit work is therefore wrapped in `autoreleasepool`.
use std::sync::Mutex;
use std::time::{Duration, Instant};

use objc2::rc::{autoreleasepool, Retained};
use objc2_app_kit::{NSBitmapFormat, NSBitmapImageRep, NSCursor};

use super::{CursorImage, CursorSource, CursorState};

/// Minimum interval between "failed to read cursor" debug logs.
const LOG_INTERVAL: Duration = Duration::from_secs(1);

pub struct MacCursorSource {
    last_log: Mutex<Option<Instant>>,
}

impl MacCursorSource {
    pub fn new() -> Self {
        Self {
            last_log: Mutex::new(None),
        }
    }

    fn log_failure(&self, reason: &str) {
        let mut last = self
            .last_log
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let now = Instant::now();
        let should_log = match *last {
            Some(t) => now.duration_since(t) >= LOG_INTERVAL,
            None => true,
        };
        if should_log {
            tracing::debug!(reason, "failed to read system cursor shape");
            *last = Some(now);
        }
    }
}

impl Default for MacCursorSource {
    fn default() -> Self {
        Self::new()
    }
}

impl CursorSource for MacCursorSource {
    fn current(&mut self) -> Option<CursorState> {
        // All AppKit objects below are autoreleased; see the module docs.
        autoreleasepool(|_| {
            #[allow(deprecated)]
            let cursor = NSCursor::currentSystemCursor();
            let Some(cursor) = cursor else {
                // Genuinely means "no current system cursor" per Apple's docs,
                // not a failure -- but there is nothing sensible to show
                // either, so treat it the same as "cursor hidden".
                return Some(CursorState::Hidden);
            };

            let image = cursor.image();
            let size = image.size();
            let hot_spot = cursor.hotSpot();
            if size.width <= 0.0 || size.height <= 0.0 {
                self.log_failure("cursor image has non-positive logical size");
                return None;
            }

            // `image.TIFFRepresentation()` was tried first and rejected: for the
            // real system arrow cursor it returns a TIFF whose primary frame is
            // a huge (hundreds-of-pixels) representation apparently meant for
            // the "pointer size" accessibility feature, not the 1x/2x image
            // actually drawn on screen -- confirmed via `prints_current_cursor`
            // (see that test's doc comment for the measured numbers). Instead,
            // scan `representations()` (each an `NSImageRep`, almost always
            // backed by an `NSBitmapImageRep` for a cursor) and pick whichever
            // one has a pixel-to-point ratio closest to a plausible display
            // scale factor (1x or 2x), skipping anything wildly larger.
            let representations = image.representations();
            let count = representations.count();
            let mut best: Option<Retained<NSBitmapImageRep>> = None;
            let mut best_score = f64::MAX;
            for i in 0..count {
                let Ok(bitmap) = representations
                    .objectAtIndex(i)
                    .downcast::<NSBitmapImageRep>()
                else {
                    continue;
                };
                let pixels_wide = bitmap.pixelsWide();
                if pixels_wide <= 0 {
                    continue;
                }
                let candidate_scale = pixels_wide as f64 / size.width;
                // Above 4x there is no real display scale factor this could be
                // -- almost certainly one of the oversized "large pointer"
                // representations seen with `TIFFRepresentation()` above.
                if candidate_scale > 4.0 {
                    continue;
                }
                let score = (candidate_scale - 1.0)
                    .abs()
                    .min((candidate_scale - 2.0).abs());
                if score < best_score {
                    best_score = score;
                    best = Some(bitmap);
                }
            }
            let Some(bitmap) = best else {
                self.log_failure("no usable NSBitmapImageRep in cursor image representations");
                return None;
            };

            let pixels_wide = bitmap.pixelsWide();
            let pixels_high = bitmap.pixelsHigh();
            if pixels_wide <= 0 || pixels_high <= 0 || size.width <= 0.0 || size.height <= 0.0 {
                self.log_failure("cursor bitmap has non-positive dimensions");
                return None;
            }

            let samples_per_pixel = bitmap.samplesPerPixel();
            if samples_per_pixel != 4 {
                // Only straightforward RGBA (or ARGB, handled below) bitmaps are
                // supported -- every system cursor observed in practice is one
                // of these; anything else (e.g. indexed/grayscale) is rare
                // enough for a cursor that it's not worth the extra conversion
                // code for this slice.
                self.log_failure("cursor bitmap is not 4 samples per pixel");
                return None;
            }

            let bytes_per_row = bitmap.bytesPerRow();
            let data_ptr = bitmap.bitmapData();
            if data_ptr.is_null() {
                self.log_failure("NSBitmapImageRep.bitmapData returned null");
                return None;
            }

            let width = pixels_wide as usize;
            let height = pixels_high as usize;
            let format = bitmap.bitmapFormat();
            let alpha_first = format.contains(NSBitmapFormat::AlphaFirst);

            // Safety: `data_ptr` is valid for `bytes_per_row * pixels_high`
            // bytes for the lifetime of `bitmap`, which outlives this read.
            let row_bytes =
                unsafe { std::slice::from_raw_parts(data_ptr, bytes_per_row as usize * height) };

            let mut rgba = vec![0u8; width * height * 4];
            for y in 0..height {
                let row = &row_bytes[y * bytes_per_row as usize..];
                for x in 0..width {
                    let px = &row[x * 4..x * 4 + 4];
                    let out = &mut rgba[(y * width + x) * 4..(y * width + x) * 4 + 4];
                    if alpha_first {
                        // ARGB in memory -> RGBA.
                        out.copy_from_slice(&[px[1], px[2], px[3], px[0]]);
                    } else {
                        out.copy_from_slice(px);
                    }
                }
            }

            let scale = pixels_wide as f64 / size.width;

            Some(CursorState::Shape(CursorImage {
                width: pixels_wide as u32,
                height: pixels_high as u32,
                hotspot_x: hot_spot.x,
                hotspot_y: hot_spot.y,
                scale,
                rgba,
            }))
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Manual check, not run in CI: reads the *real* current system cursor
    /// shape (see ACCEPTANCE in the slice prompt). Requires the "Screen
    /// Recording" TCC permission to have been granted to this terminal at
    /// least once in the past (querying the cursor shape itself does not
    /// require any TCC permission, but the deprecated API sometimes behaves
    /// oddly before ScreenCaptureKit has been used at all).
    ///
    /// Run with:
    /// `cargo test -p rcdesk-host prints_current_cursor -- --ignored --nocapture`
    #[test]
    #[cfg(target_os = "macos")]
    #[ignore]
    fn prints_current_cursor() {
        let mut source = MacCursorSource::new();
        match source.current() {
            Some(CursorState::Shape(image)) => {
                println!(
                    "cursor shape: {}x{} hotspot=({}, {}) scale={}",
                    image.width, image.height, image.hotspot_x, image.hotspot_y, image.scale
                );
            }
            Some(CursorState::Hidden) => println!("cursor is hidden"),
            None => println!("failed to read cursor shape"),
        }
    }

    /// Process `phys_footprint` in MB, via the external `footprint` tool.
    /// Line format: `name [pid]: 64-bit    Footprint: 13 MB (16384 bytes per page)`.
    fn footprint_mb() -> f64 {
        let out = std::process::Command::new("footprint")
            .args(["-p", &std::process::id().to_string()])
            .output()
            .expect("run footprint");
        let text = String::from_utf8_lossy(&out.stdout).into_owned();
        let line = text
            .lines()
            .find(|l| l.contains("Footprint:"))
            .unwrap_or_else(|| panic!("no Footprint line in: {text}"));
        let mut parts = line
            .split("Footprint:")
            .nth(1)
            .expect("text after Footprint:")
            .split_whitespace();
        let value: f64 = parts
            .next()
            .expect("footprint value")
            .parse()
            .expect("footprint number");
        match parts.next().expect("footprint unit") {
            "KB" => value / 1024.0,
            "MB" => value,
            "GB" => value * 1024.0,
            "bytes" | "B" => value / (1024.0 * 1024.0),
            other => panic!("unknown footprint unit: {other}"),
        }
    }

    /// Manual leak check, not run in CI: polls the cursor 3000 times from a
    /// non-main thread (as in production) and compares the process
    /// `phys_footprint` before and after. Without an autorelease pool in
    /// `current()` the growth is hundreds of MB.
    ///
    /// Run with:
    /// `cargo test -p rcdesk-host cursor_poll_does_not_leak -- --ignored --nocapture`
    #[test]
    #[cfg(target_os = "macos")]
    #[ignore]
    fn cursor_poll_does_not_leak() {
        let (before, after) = std::thread::spawn(|| {
            let mut source = MacCursorSource::new();
            // Warm-up: the first calls load AppKit/cursor assets (a constant
            // ~65 MB one-time cost, independent of call count), not a leak.
            for _ in 0..50 {
                let _ = source.current();
            }
            let before = footprint_mb();
            for _ in 0..3000 {
                let _ = source.current();
            }
            (before, footprint_mb())
        })
        .join()
        .expect("poll thread panicked");
        let delta_mb = after - before;
        println!("footprint before: {before:.1} MB, after: {after:.1} MB, delta: {delta_mb:.1} MB");
        assert!(
            delta_mb < 50.0,
            "cursor polling leaked {delta_mb:.1} MB over 3000 calls"
        );
    }
}
