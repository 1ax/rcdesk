//! GDI (`BitBlt`) screen capture: the fallback `FrameSource` for Windows
//! machines whose video driver cannot satisfy `windows-capture`'s minimum
//! Direct3D feature level (11_0) for Windows Graphics Capture -- legacy
//! WDDM 1.x drivers (seen on the owner's Windows 10 test bench, an ATI
//! Radeon HD 4600, Direct3D 10.1 only) and some virtual machines. See
//! `platform::windows::d3d::wgc_supported`, checked by `main.rs` before
//! picking a backend, and `docs/host-libs-api-notes.md` (scap section) for
//! why `windows-capture` 1.4.4 panics outright on such hardware instead of
//! returning an error.
//!
//! GDI works on any driver but is plain CPU-side `BitBlt`: no jank-free
//! DWM composition hand-off like WGC, no "yellow border" capture
//! indicator, and this source has to poll on a timer ([`super::FramePacer`])
//! instead of blocking for the next frame the way `ScapSource` does. Two
//! things keep the CPU cost down: the pacer caps the poll rate to the
//! requested fps, and [`super::frame_unchanged`] skips re-returning (and so
//! re-encoding) a frame that is byte-identical to the last one -- a static
//! screen produces no output here at all beyond the unavoidable `BitBlt`
//! poll.
//!
//! The system cursor is drawn by DWM outside of what `BitBlt` captures from
//! the screen DC in the common case (hardware cursor), so it does not show
//! up in these frames either -- consistent with `ScapSource`'s
//! `show_cursor: false` and the cursor shape being sent to the client
//! through a separate channel (`cursor::windows::WinCursorSource`).
//!
//! D45: in machine mode the capture thread follows the input desktop
//! (`platform::desktop`): when Windows switches to `Winlogon` (lock screen,
//! UAC) the thread is rebound and the GDI surface is recreated, since DCs
//! and DIB sections made on the old desktop are useless there. A `BitBlt`
//! failure while following is not fatal (it would stop the whole video
//! pipeline): the tick is skipped after a re-sync attempt.
//!
//! Compiles only on `windows-latest` in CI: there is no Windows machine
//! available to this executor run, so every signature/type below comes from
//! reading the `windows` 0.61.3 source fetched into the local registry
//! cache, not from memory -- see executor rule 3 (the same approach
//! `host/src/cursor/windows.rs` and `host/src/platform/windows/keyboard.rs`
//! document and use).

use std::ffi::c_void;
use std::mem::size_of;
use std::slice;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use windows::Win32::Graphics::Gdi::{
    BitBlt, CreateCompatibleDC, CreateDIBSection, DeleteDC, DeleteObject, GetDC, GetMonitorInfoW,
    ReleaseDC, SelectObject, BITMAPINFO, BITMAPINFOHEADER, BI_RGB, DIB_RGB_COLORS, HBITMAP, HDC,
    HGDIOBJ, HMONITOR, MONITORINFO, SRCCOPY,
};
use windows::Win32::UI::WindowsAndMessaging::{GetSystemMetrics, SM_CXSCREEN, SM_CYSCREEN};

use crate::platform;

use super::{frame_unchanged, CaptureError, FramePacer, FrameSource, RawFrame};

/// One DIB section plus the pointer to its pixel memory.
struct Dib {
    hbitmap: HBITMAP,
    /// Points into the DIB section backing `hbitmap`; valid for as long as
    /// `hbitmap` is (i.e. until `GdiSource` is dropped).
    pixels: *mut u8,
}

/// DCs and DIB sections of one capture surface; recreated when the thread
/// moves to another desktop (D45).
struct GdiSurface {
    hdc_screen: HDC,
    hdc_mem: HDC,
    /// Two DIB sections used in turn (see `next_frame` for the invariant).
    dibs: [Dib; 2],
    /// The bitmap `hdc_mem` had selected before our first `SelectObject`.
    old_bitmap: HGDIOBJ,
}

pub struct GdiSource {
    surface: GdiSurface,
    /// Index of the active section, the one the next `BitBlt` writes into.
    cur: usize,
    /// False until the first frame has been handed out; until then section
    /// `1 - cur` holds no valid previous frame to compare against.
    has_prev: bool,
    /// Top-left of the captured region in virtual-screen coordinates.
    origin: (i32, i32),
    width: u32,
    height: u32,
    pacer: FramePacer,
    stop: Arc<AtomicBool>,
    /// True until the first tick has run its unthrottled desktop sync.
    first_tick: bool,
    last_warn: Option<Instant>,
}

impl GdiSource {
    pub fn new(display_id: Option<u32>, fps: u32) -> Result<Self, CaptureError> {
        let (origin, width, height) = match display_id {
            None => {
                // Safety: `GetSystemMetrics` has no preconditions.
                let w = unsafe { GetSystemMetrics(SM_CXSCREEN) };
                let h = unsafe { GetSystemMetrics(SM_CYSCREEN) };
                ((0i32, 0i32), w.max(0) as u32, h.max(0) as u32)
            }
            Some(id) => {
                // `id` is an `HMONITOR` truncated to `u32`, the same
                // encoding `scap::get_all_targets` uses for `Display::id`
                // (see `scap-0.0.8/src/targets/win/mod.rs`:
                // `display.as_raw_hmonitor() as u32`) -- this module is
                // meant to be a drop-in alternative source for the same
                // `--display` ids `ScapSource` accepts.
                let hmonitor = HMONITOR(id as *mut c_void);
                let mut info = MONITORINFO {
                    cbSize: size_of::<MONITORINFO>() as u32,
                    ..Default::default()
                };
                // Safety: `info` is a correctly-sized, valid out-parameter;
                // an invalid/stale `hmonitor` is reported via the return
                // value, not undefined behaviour.
                let ok = unsafe { GetMonitorInfoW(hmonitor, &mut info) };
                if !ok.as_bool() {
                    return Err(CaptureError::Backend(format!("display {id} not found")));
                }
                let rect = info.rcMonitor;
                (
                    (rect.left, rect.top),
                    (rect.right - rect.left).max(0) as u32,
                    (rect.bottom - rect.top).max(0) as u32,
                )
            }
        };
        // Match `scap`'s own even-dimension clamping (odd width/height
        // breaks 4:2:0 chroma subsampling downstream in `to_i420`).
        let width = width - width % 2;
        let height = height - height % 2;
        let surface = GdiSurface::new(width, height)?;
        Ok(Self {
            surface,
            cur: 0,
            has_prev: false,
            origin,
            width,
            height,
            pacer: FramePacer::new(fps),
            stop: Arc::new(AtomicBool::new(false)),
            first_tick: true,
            last_warn: None,
        })
    }

    /// Replaces the surface after a desktop switch; the next frame is sent
    /// even if it looks like the previous one.
    fn recreate_surface(&mut self) -> Result<(), CaptureError> {
        self.surface = GdiSurface::new(self.width, self.height)?;
        self.cur = 0;
        self.has_prev = false;
        Ok(())
    }
}

impl GdiSurface {
    fn new(width: u32, height: u32) -> Result<Self, CaptureError> {
        // Safety: `GetDC(None)` asks for the DC of the whole screen; a null
        // return means "unavailable" per MSDN, not undefined behaviour.
        let hdc_screen = unsafe { GetDC(None) };
        if hdc_screen.is_invalid() {
            return Err(CaptureError::Backend("GetDC failed".to_string()));
        }

        // Safety: `hdc_screen` was just validated above.
        let hdc_mem = unsafe { CreateCompatibleDC(Some(hdc_screen)) };
        if hdc_mem.is_invalid() {
            // Safety: `hdc_screen` is a valid DC obtained above and not used
            // again on this path.
            unsafe {
                ReleaseDC(None, hdc_screen);
            }
            return Err(CaptureError::Backend(
                "CreateCompatibleDC failed".to_string(),
            ));
        }

        let header = BITMAPINFOHEADER {
            biSize: size_of::<BITMAPINFOHEADER>() as u32,
            biWidth: width as i32,
            // Negative height: top-down DIB, so `pixels` row order matches
            // what `RawFrame::Bgra` is expected to carry (no manual flip).
            biHeight: -(height as i32),
            biPlanes: 1,
            biBitCount: 32,
            biCompression: BI_RGB.0,
            ..Default::default()
        };
        let bitmap_info = BITMAPINFO {
            bmiHeader: header,
            ..Default::default()
        };

        // Safety: `hdc_screen` is valid and `bitmap_info` describes a 32bpp
        // top-down DIB sized `width` x `height`.
        let dib0 = match unsafe { create_dib(hdc_screen, &bitmap_info) } {
            Ok(dib) => dib,
            Err(err) => {
                // Safety: both were created above and not used again on
                // this path.
                unsafe {
                    let _ = DeleteDC(hdc_mem);
                    ReleaseDC(None, hdc_screen);
                }
                return Err(CaptureError::Backend(format!("CreateDIBSection: {err}")));
            }
        };
        // Safety: as above.
        let dib1 = match unsafe { create_dib(hdc_screen, &bitmap_info) } {
            Ok(dib) => dib,
            Err(err) => {
                // Safety: all three were created above and not used again on
                // this path.
                unsafe {
                    let _ = DeleteObject(dib0.hbitmap.into());
                    let _ = DeleteDC(hdc_mem);
                    ReleaseDC(None, hdc_screen);
                }
                return Err(CaptureError::Backend(format!("CreateDIBSection: {err}")));
            }
        };

        // Safety: `hdc_mem` and `dib0.hbitmap` were both just created above.
        let old_bitmap = unsafe { SelectObject(hdc_mem, dib0.hbitmap.into()) };
        if old_bitmap.is_invalid() {
            // Safety: all four were created above and not used again on
            // this path.
            unsafe {
                let _ = DeleteObject(dib1.hbitmap.into());
                let _ = DeleteObject(dib0.hbitmap.into());
                let _ = DeleteDC(hdc_mem);
                ReleaseDC(None, hdc_screen);
            }
            return Err(CaptureError::Backend("SelectObject failed".to_string()));
        }

        Ok(Self {
            hdc_screen,
            hdc_mem,
            dibs: [dib0, dib1],
            old_bitmap,
        })
    }
}

/// Creates one DIB section described by `bitmap_info`.
///
/// # Safety
/// `hdc_screen` must be a valid DC and `bitmap_info` a valid 32bpp
/// description.
unsafe fn create_dib(
    hdc_screen: HDC,
    bitmap_info: &BITMAPINFO,
) -> Result<Dib, windows::core::Error> {
    let mut pixels_ptr: *mut c_void = std::ptr::null_mut();
    // Safety: upheld by the caller; `pixels_ptr` is a valid out-parameter.
    let hbitmap = unsafe {
        CreateDIBSection(
            Some(hdc_screen),
            bitmap_info,
            DIB_RGB_COLORS,
            &mut pixels_ptr,
            None,
            0,
        )
    }?;
    Ok(Dib {
        hbitmap,
        pixels: pixels_ptr.cast(),
    })
}

impl FrameSource for GdiSource {
    fn next_frame(&mut self) -> Result<RawFrame, CaptureError> {
        loop {
            if self.stop.load(Ordering::Acquire) {
                return Err(CaptureError::Stopped);
            }
            self.pacer.wait();

            // D45: follow the input desktop (lock screen, UAC); no-op unless
            // machine mode enabled it. The first tick syncs unthrottled.
            let switched = if self.first_tick {
                self.first_tick = false;
                platform::desktop::sync_thread_now()
            } else {
                platform::desktop::sync_thread()
            };
            if switched {
                self.recreate_surface()?;
            }

            // Invariant: section `1 - cur` always holds the last frame handed
            // out; the active section `cur` is overwritten by every `BitBlt`.
            // So an unchanged (static) screen costs no copies at all, and a
            // changed one costs exactly one (`to_vec` below).
            //
            // Safety: `hdc_mem` is valid for the whole lifetime of `self` and
            // `dibs[cur]` is a live bitmap not selected elsewhere. Selecting
            // unconditionally is a cheap GDI call, simpler than tracking
            // which section is currently selected.
            unsafe {
                SelectObject(
                    self.surface.hdc_mem,
                    self.surface.dibs[self.cur].hbitmap.into(),
                );
            }

            // Safety: `hdc_mem` and `hdc_screen` are valid for the whole
            // lifetime of `self`. No `CAPTUREBLT` flag: this blits only
            // DWM's composited desktop image, which is the same "no cursor,
            // no layered click-through popups" behaviour `ScapSource`
            // already has with `show_cursor: false` -- decided by the
            // architect for slice 2.1c.
            let blt = unsafe {
                BitBlt(
                    self.surface.hdc_mem,
                    0,
                    0,
                    self.width as i32,
                    self.height as i32,
                    Some(self.surface.hdc_screen),
                    self.origin.0,
                    self.origin.1,
                    SRCCOPY,
                )
            };
            if let Err(err) = blt {
                if !platform::desktop::is_enabled() {
                    return Err(CaptureError::Backend(format!("BitBlt: {err}")));
                }
                // D45: `BitBlt` briefly fails while the input desktop switches
                // (e.g. to `Winlogon`); an error here would stop the whole
                // video pipeline, so re-sync and retry on the next tick.
                if platform::desktop::sync_thread_now() {
                    self.recreate_surface()?;
                } else {
                    let now = Instant::now();
                    if self
                        .last_warn
                        .is_none_or(|t| now.duration_since(t) >= Duration::from_secs(10))
                    {
                        self.last_warn = Some(now);
                        tracing::warn!("BitBlt failed, skipping tick: {err}");
                    }
                }
                continue;
            }

            let len = self.width as usize * self.height as usize * 4;
            // Safety: each `pixels` points into a DIB section that stays
            // alive (and sized for exactly `width * height * 4` bytes of
            // 32bpp pixels) for the whole lifetime of `self`; GDI calls are
            // synchronous, so `BitBlt` above has finished writing every
            // pixel by the time it returns control here. The inactive
            // section is not being written by anyone.
            let cur: &[u8] =
                unsafe { slice::from_raw_parts(self.surface.dibs[self.cur].pixels, len) };
            let prev: &[u8] =
                unsafe { slice::from_raw_parts(self.surface.dibs[1 - self.cur].pixels, len) };

            if self.has_prev && frame_unchanged(Some(prev), cur) {
                continue;
            }

            // The single copy per changed frame: `data` is what `RawFrame`
            // carries out. The section itself becomes the "previous frame"
            // by flipping `cur`.
            let data = cur.to_vec();
            self.has_prev = true;
            self.cur = 1 - self.cur;

            return Ok(RawFrame::Bgra {
                width: self.width,
                height: self.height,
                data,
                stride: self.width as usize * 4,
                ts: Instant::now(),
            });
        }
    }

    fn size(&self) -> (u32, u32) {
        (self.width, self.height)
    }
}

impl Drop for GdiSurface {
    fn drop(&mut self) {
        // Safety: reverses `new`'s construction order; every handle here
        // was created there and is not touched again after this.
        unsafe {
            SelectObject(self.hdc_mem, self.old_bitmap);
            let _ = DeleteObject(self.dibs[1].hbitmap.into());
            let _ = DeleteObject(self.dibs[0].hbitmap.into());
            let _ = DeleteDC(self.hdc_mem);
            ReleaseDC(None, self.hdc_screen);
        }
    }
}

// SAFETY: like `ScapSource`, `GdiSource` is only ever touched from the
// single dedicated capture thread that owns it for its entire lifetime
// (created, used and dropped there); we only need `Send` to move the
// freshly-built value into that thread once. The raw `pixels` pointers are
// never read or written from any other thread.
unsafe impl Send for GdiSource {}
