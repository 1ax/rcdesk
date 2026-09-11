//! Frame sources: something that produces raw video frames for the encode
//! stage. `scap` (macOS/Windows) is the real backend; `synthetic` is a
//! platform-independent generator used by tests, CI and local development on
//! machines without screen-recording permission.

use std::time::Instant;

pub mod synthetic;

#[cfg(any(target_os = "macos", target_os = "windows"))]
pub mod scap;

/// A single raw (unencoded) video frame captured from a source.
///
/// `Nv12` is what `scap` hands us on macOS (biplanar 4:2:0, Y + interleaved
/// CbCr). `Bgra` is what `scap` hands us on Windows (packed BGRA8).
#[derive(Debug, Clone)]
pub enum RawFrame {
    Nv12 {
        width: u32,
        height: u32,
        y: Vec<u8>,
        y_stride: usize,
        uv: Vec<u8>,
        uv_stride: usize,
        ts: Instant,
    },
    Bgra {
        width: u32,
        height: u32,
        data: Vec<u8>,
        stride: usize,
        ts: Instant,
    },
}

impl RawFrame {
    /// Capture timestamp, used by the pipeline to track capture-to-encode
    /// latency (see ARCHITECTURE.md §12 latency targets).
    pub fn ts(&self) -> Instant {
        match self {
            RawFrame::Nv12 { ts, .. } | RawFrame::Bgra { ts, .. } => *ts,
        }
    }
}

/// Something that produces a sequence of raw frames, one call at a time.
///
/// Implementations may block inside `next_frame` (e.g. waiting for the next
/// screen update); callers are expected to run them on a dedicated thread.
pub trait FrameSource: Send {
    fn next_frame(&mut self) -> Result<RawFrame, CaptureError>;
    fn size(&self) -> (u32, u32);
}

/// A capturable display, as reported by the OS.
#[derive(Debug, Clone)]
pub struct DisplayInfo {
    pub id: u32,
    pub title: String,
}

#[derive(Debug, thiserror::Error)]
pub enum CaptureError {
    #[error("screen recording permission not granted")]
    PermissionDenied,
    #[error("screen capture is not supported on this platform")]
    Unsupported,
    #[error("capture backend error: {0}")]
    Backend(String),
    #[error("capture source stopped")]
    Stopped,
}

/// Lists capturable displays via `scap`. Requires screen-recording
/// permission to be granted already: this never prompts (there is nobody to
/// click the dialog on a headless/CI/terminal session), it just reports the
/// permission is missing.
#[cfg(any(target_os = "macos", target_os = "windows"))]
pub fn list_displays() -> Result<Vec<DisplayInfo>, CaptureError> {
    if !::scap::is_supported() {
        return Err(CaptureError::Unsupported);
    }
    if !::scap::has_permission() {
        return Err(CaptureError::PermissionDenied);
    }

    Ok(::scap::get_all_targets()
        .into_iter()
        .filter_map(|target| match target {
            ::scap::Target::Display(display) => Some(DisplayInfo {
                id: display.id,
                title: display.title,
            }),
            ::scap::Target::Window(_) => None,
        })
        .collect())
}

#[cfg(not(any(target_os = "macos", target_os = "windows")))]
pub fn list_displays() -> Result<Vec<DisplayInfo>, CaptureError> {
    Err(CaptureError::Unsupported)
}
