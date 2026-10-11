//! `scap`-backed `FrameSource` for macOS (NV12/`YUVFrame` via ScreenCaptureKit)
//! and Windows (BGRA via Windows Graphics Capture).
//!
//! The terminal running this crate's tests/CI has no screen-recording
//! permission, so nothing here is exercised automatically: `ScapSource::new`
//! always checks `scap::has_permission()` first and returns a plain error
//! instead of panicking or calling `scap::request_permission()` (there is
//! nobody to click the TCC dialog in an unattended/terminal session). Live
//! capture is verified manually by whoever has the permission granted.

use std::time::Instant;

use ::scap::capturer::{Capturer, Options, Resolution};
use ::scap::frame::{Frame, FrameType};
use ::scap::{get_all_targets, has_permission, is_supported, Target};

use super::{CaptureError, FrameSource, RawFrame};

pub struct ScapSource {
    capturer: Capturer,
    width: u32,
    height: u32,
}

// SAFETY: `Capturer` is only ever touched from the single dedicated capture
// thread that owns it for its entire lifetime (created, used and dropped
// there); we only need `Send` to move the freshly-built value into that
// thread once.
unsafe impl Send for ScapSource {}

impl ScapSource {
    /// `exclude_windows` -- CGWindowID окон, которые не попадают в захват
    /// (окна шторки, слайс 4.2c). Работает только на macOS (scap
    /// документирует исключение лишь там); на Windows аргумент игнорируется.
    /// Окна должны существовать к моменту `Capturer::build`.
    pub fn new(
        display_id: Option<u32>,
        fps: u32,
        exclude_windows: &[u32],
    ) -> Result<Self, CaptureError> {
        if !is_supported() {
            return Err(CaptureError::Unsupported);
        }
        if !has_permission() {
            return Err(CaptureError::PermissionDenied);
        }

        let target = match display_id {
            Some(id) => get_all_targets()
                .into_iter()
                .find(|t| matches!(t, Target::Display(d) if d.id == id))
                .ok_or_else(|| CaptureError::Backend(format!("display {id} not found")))?,
            None => get_all_targets()
                .into_iter()
                .find(|t| matches!(t, Target::Display(_)))
                .ok_or_else(|| CaptureError::Backend("no capturable display found".to_string()))?,
        };

        #[cfg(target_os = "macos")]
        let output_type = FrameType::YUVFrame;
        #[cfg(target_os = "windows")]
        let output_type = FrameType::BGRAFrame;

        // `scap::Window` не реэкспортирован из крейта (модуль `targets`
        // приватный), поэтому литерал структуры не написать: берём любое
        // реальное окно из `get_all_targets()` как заготовку и подменяем в
        // клоне `id`/`raw_handle`. scap сверяет исключаемые окна только по
        // `id` (см. `capturer::engine::mac`).
        #[cfg(target_os = "macos")]
        let excluded_targets = if exclude_windows.is_empty() {
            None
        } else {
            let template = get_all_targets()
                .into_iter()
                .find(|t| matches!(t, Target::Window(_)))
                .ok_or_else(|| {
                    CaptureError::Backend(
                        "нет ни одного окна-заготовки для исключения окон шторки".to_string(),
                    )
                })?;
            Some(
                exclude_windows
                    .iter()
                    .map(|&id| {
                        let mut target = template.clone();
                        if let Target::Window(w) = &mut target {
                            w.id = id;
                            w.raw_handle = id;
                        }
                        target
                    })
                    .collect::<Vec<_>>(),
            )
        };
        #[cfg(not(target_os = "macos"))]
        let excluded_targets = {
            let _ = exclude_windows;
            None
        };

        let options = Options {
            fps,
            show_cursor: false,
            show_highlight: false,
            target: Some(target),
            crop_area: None,
            output_type,
            output_resolution: Resolution::Captured,
            excluded_targets,
        };

        let mut capturer = Capturer::build(options).map_err(|err| match err {
            ::scap::capturer::CapturerBuildError::NotSupported => CaptureError::Unsupported,
            ::scap::capturer::CapturerBuildError::PermissionNotGranted => {
                CaptureError::PermissionDenied
            }
        })?;

        capturer.start_capture();
        let [width, height] = capturer.get_output_frame_size();

        Ok(Self {
            capturer,
            width,
            height,
        })
    }
}

impl FrameSource for ScapSource {
    fn next_frame(&mut self) -> Result<RawFrame, CaptureError> {
        loop {
            let frame = self
                .capturer
                .get_next_frame()
                .map_err(|_| CaptureError::Stopped)?;

            match frame {
                Frame::YUVFrame(f) => {
                    if f.width == 0 {
                        continue;
                    }
                    return Ok(RawFrame::Nv12 {
                        width: f.width as u32,
                        height: f.height as u32,
                        y: f.luminance_bytes,
                        y_stride: f.luminance_stride as usize,
                        uv: f.chrominance_bytes,
                        uv_stride: f.chrominance_stride as usize,
                        ts: Instant::now(),
                    });
                }
                Frame::BGRA(f) => {
                    if f.width == 0 {
                        continue;
                    }
                    let stride = f.width as usize * 4;
                    return Ok(RawFrame::Bgra {
                        width: f.width as u32,
                        height: f.height as u32,
                        data: f.data,
                        stride,
                        ts: Instant::now(),
                    });
                }
                _ => return Err(CaptureError::Backend("unexpected frame type".to_string())),
            }
        }
    }

    fn size(&self) -> (u32, u32) {
        (self.width, self.height)
    }
}

impl Drop for ScapSource {
    fn drop(&mut self) {
        self.capturer.stop_capture();
    }
}
