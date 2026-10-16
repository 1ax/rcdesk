//! `scap`-backed `FrameSource` for macOS (NV12/`YUVFrame` via ScreenCaptureKit)
//! and Windows (BGRA via Windows Graphics Capture).
//!
//! The terminal running this crate's tests/CI has no screen-recording
//! permission, so nothing here is exercised automatically: `ScapSource::new`
//! always checks `scap::has_permission()` first and returns a plain error
//! instead of panicking or calling `scap::request_permission()` (there is
//! nobody to click the TCC dialog in an unattended/terminal session). Live
//! capture is verified manually by whoever has the permission granted.
//!
//! Если система сама остановила поток (`didStopWithError`), `next_frame`
//! перезапускает захват внутри того же `ScapSource` (не чаще `RESTART_LIMIT`
//! раз за `RESTART_WINDOW`), если размер экрана не изменился.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::RecvTimeoutError;
use std::sync::Arc;
use std::time::{Duration, Instant};

use ::scap::capturer::{Capturer, Options, Resolution};
use ::scap::frame::{Frame, FrameType};
use ::scap::{get_all_targets, has_permission, is_supported, Target};

use super::{CaptureError, FrameSource, RawFrame};

/// How long one `get_next_frame_timeout` waits before the loop re-checks the
/// stop flag. This is the worst-case cost of stopping a static-screen capture
/// (up to 250 ms); with a live screen frames arrive well inside it, so frame
/// latency is unaffected.
const RECV_TIMEOUT: Duration = Duration::from_millis(250);

/// Не более `RESTART_LIMIT` перезапусков захвата за скользящее окно
/// `RESTART_WINDOW`: дальше считаем, что система стабильно рвёт поток.
const RESTART_LIMIT: usize = 3;
const RESTART_WINDOW: Duration = Duration::from_secs(60);

/// Бюджет перезапусков; время передаётся аргументом (для тестов).
struct RestartBudget {
    marks: Vec<Instant>,
}

impl RestartBudget {
    fn new() -> Self {
        Self { marks: Vec::new() }
    }

    /// Отметка старше или равная окну выбрасывается; если осталось меньше
    /// `RESTART_LIMIT` -- записывает `now` и возвращает true.
    fn try_take(&mut self, now: Instant) -> bool {
        self.marks
            .retain(|&mark| now.saturating_duration_since(mark) < RESTART_WINDOW);
        if self.marks.len() < RESTART_LIMIT {
            self.marks.push(now);
            true
        } else {
            false
        }
    }
}

/// Сколько ждать возвращения дисплея после остановки потока (I7).
const DISPLAY_RETURN_TIMEOUT: Duration = Duration::from_secs(5);
/// Шаг опроса при ожидании дисплея.
const DISPLAY_RETURN_STEP: Duration = Duration::from_millis(250);

/// Ошибка «дисплей не найден» из `build_capturer`.
fn is_display_missing(err: &CaptureError) -> bool {
    matches!(err, CaptureError::Backend(m)
        if (m.starts_with("display ") && m.ends_with(" not found"))
            || m == "no capturable display found")
}

/// Повторяет `build`, пока она падает с «дисплей не найден», не дольше
/// `timeout` (шаг `step`, ожидание через `sleep`). Прочие ошибки -- сразу.
/// По таймауту возвращает исходную ошибку.
fn retry_while_display_missing<T>(
    mut build: impl FnMut() -> Result<T, CaptureError>,
    timeout: Duration,
    step: Duration,
    mut sleep: impl FnMut(Duration),
) -> Result<T, CaptureError> {
    let mut waited = Duration::ZERO;
    loop {
        match build() {
            Err(err) if is_display_missing(&err) && waited < timeout => {
                sleep(step);
                waited += step;
            }
            other => return other,
        }
    }
}

pub struct ScapSource {
    capturer: Capturer,
    width: u32,
    height: u32,
    stop: Arc<AtomicBool>,
    display_id: Option<u32>,
    fps: u32,
    exclude_windows: Vec<u32>,
    restarts: RestartBudget,
}

/// Собирает и запускает `Capturer`, возвращает его и размер кадра.
fn build_capturer(
    display_id: Option<u32>,
    fps: u32,
    exclude_windows: &[u32],
) -> Result<(Capturer, u32, u32), CaptureError> {
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
    Ok((capturer, width, height))
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
        let (capturer, width, height) = build_capturer(display_id, fps, exclude_windows)?;
        Ok(Self {
            capturer,
            width,
            height,
            stop: Arc::new(AtomicBool::new(false)),
            display_id,
            fps,
            exclude_windows: exclude_windows.to_vec(),
            restarts: RestartBudget::new(),
        })
    }
}

impl ScapSource {
    /// Перезапуск захвата после остановки потока системой. Старый `Capturer`
    /// просто отбрасывается: Drop у него нет (стоп уже случился сам).
    fn restart_capture(&mut self) -> Result<(), CaptureError> {
        tracing::warn!("screen capture stream stopped by the system, restarting");
        if !self.restarts.try_take(Instant::now()) {
            tracing::error!(
                limit = RESTART_LIMIT,
                window_s = RESTART_WINDOW.as_secs(),
                "screen capture keeps stopping, giving up"
            );
            return Err(CaptureError::Backend(
                "screen capture keeps stopping".into(),
            ));
        }
        let (display_id, fps) = (self.display_id, self.fps);
        let exclude_windows = &self.exclude_windows;
        // Guard держится до конца функции (на не-macOS -- ZST без Drop).
        let mut wake_guard = None;
        let (capturer, width, height) = retry_while_display_missing(
            || {
                let res = build_capturer(display_id, fps, exclude_windows);
                if matches!(&res, Err(e) if is_display_missing(e)) && wake_guard.is_none() {
                    tracing::warn!("display gone after capture stop, waking and waiting");
                    wake_guard = Some(crate::platform::display_wake::wake());
                }
                res
            },
            DISPLAY_RETURN_TIMEOUT,
            DISPLAY_RETURN_STEP,
            std::thread::sleep,
        )?;
        if (width, height) != (self.width, self.height) {
            tracing::error!(
                old_width = self.width,
                old_height = self.height,
                new_width = width,
                new_height = height,
                "screen size changed after capture restart, stopping"
            );
            return Err(CaptureError::Backend(
                "screen size changed after capture restart".into(),
            ));
        }
        self.capturer = capturer;
        tracing::info!(
            attempt = self.restarts.marks.len(),
            "screen capture restarted"
        );
        Ok(())
    }
}

impl FrameSource for ScapSource {
    fn next_frame(&mut self) -> Result<RawFrame, CaptureError> {
        loop {
            if self.stop.load(Ordering::Acquire) {
                return Err(CaptureError::Stopped);
            }
            let frame = match self.capturer.get_next_frame_timeout(RECV_TIMEOUT) {
                Ok(frame) => frame,
                Err(RecvTimeoutError::Timeout) => {
                    if self.capturer.stream_failed() {
                        self.restart_capture()?;
                    }
                    continue;
                }
                Err(RecvTimeoutError::Disconnected) => return Err(CaptureError::Stopped),
            };

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

    fn stop_signal(&self) -> Option<Arc<AtomicBool>> {
        Some(Arc::clone(&self.stop))
    }
}

impl Drop for ScapSource {
    fn drop(&mut self) {
        if let Err(err) = self.capturer.try_stop_capture() {
            tracing::debug!(error = %err, "screen capture was already stopped");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn missing() -> CaptureError {
        CaptureError::Backend("display 3 not found".into())
    }

    #[test]
    fn retry_succeeds_when_display_returns_on_third_attempt() {
        let mut calls = 0;
        let mut slept = 0;
        let res = retry_while_display_missing(
            || {
                calls += 1;
                if calls < 3 {
                    Err(missing())
                } else {
                    Ok(calls)
                }
            },
            Duration::from_secs(5),
            Duration::from_millis(250),
            |_| slept += 1,
        );
        assert_eq!(res.unwrap(), 3);
        assert_eq!(slept, 2);
    }

    #[test]
    fn retry_gives_original_error_after_timeout() {
        let mut calls = 0;
        let res: Result<(), _> = retry_while_display_missing(
            || {
                calls += 1;
                Err(missing())
            },
            Duration::from_secs(1),
            Duration::from_millis(250),
            |_| {},
        );
        assert!(matches!(res, Err(ref e) if is_display_missing(e)));
        assert_eq!(calls, 5);
    }

    #[test]
    fn retry_returns_other_errors_immediately() {
        let mut calls = 0;
        let res: Result<(), _> = retry_while_display_missing(
            || {
                calls += 1;
                Err(CaptureError::PermissionDenied)
            },
            Duration::from_secs(5),
            Duration::from_millis(250),
            |_| panic!("must not sleep"),
        );
        assert!(matches!(res, Err(CaptureError::PermissionDenied)));
        assert_eq!(calls, 1);
    }

    #[test]
    fn budget_allows_three_then_denies() {
        let mut b = RestartBudget::new();
        let t0 = Instant::now();
        assert!(b.try_take(t0));
        assert!(b.try_take(t0 + Duration::from_secs(1)));
        assert!(b.try_take(t0 + Duration::from_secs(2)));
        assert!(!b.try_take(t0 + Duration::from_secs(3)));
    }

    #[test]
    fn budget_recovers_after_window() {
        let mut b = RestartBudget::new();
        let t0 = Instant::now();
        for _ in 0..3 {
            assert!(b.try_take(t0));
        }
        assert!(!b.try_take(t0 + Duration::from_secs(59)));
        assert!(b.try_take(t0 + RESTART_WINDOW + Duration::from_secs(1)));
    }

    #[test]
    fn budget_mark_equal_to_window_is_dropped() {
        // Правило: отметка старше или равная окну выбрасывается.
        let mut b = RestartBudget::new();
        let t0 = Instant::now();
        for _ in 0..3 {
            assert!(b.try_take(t0));
        }
        assert!(b.try_take(t0 + RESTART_WINDOW));
    }
}
