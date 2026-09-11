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
//! Compiles only on `windows-latest` in CI: there is no Windows machine
//! available to this executor run, so every signature/type below comes from
//! reading the `windows` 0.61.3 source fetched into the local registry
//! cache, not from memory -- see executor rule 3 (the same approach
//! `host/src/cursor/windows.rs` and `host/src/platform/windows/keyboard.rs`
//! document and use).

use std::ffi::c_void;
use std::mem::size_of;
use std::slice;
use std::time::Instant;

use windows::Win32::Graphics::Gdi::{
    BitBlt, CreateCompatibleDC, CreateDIBSection, DeleteDC, DeleteObject, GetDC, GetMonitorInfoW,
    ReleaseDC, SelectObject, BITMAPINFO, BITMAPINFOHEADER, BI_RGB, DIB_RGB_COLORS, HBITMAP, HDC,
    HGDIOBJ, HMONITOR, MONITORINFO, SRCCOPY,
};
use windows::Win32::UI::WindowsAndMessaging::{GetSystemMetrics, SM_CXSCREEN, SM_CYSCREEN};

use super::{frame_unchanged, CaptureError, FramePacer, FrameSource, RawFrame};

pub struct GdiSource {
    hdc_screen: HDC,
    hdc_mem: HDC,
    hbitmap: HBITMAP,
    old_bitmap: HGDIOBJ,
    /// Points into the DIB section backing `hbitmap`; valid for as long as
    /// `hbitmap` is (i.e. for the lifetime of `self`).
    pixels: *mut u8,
    /// Top-left of the captured region in virtual-screen coordinates.
    origin: (i32, i32),
    width: u32,
    height: u32,
    pacer: FramePacer,
    prev: Option<Vec<u8>>,
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

        let mut pixels_ptr: *mut c_void = std::ptr::null_mut();
        // Safety: `hdc_screen` is valid, `bitmap_info` describes a 32bpp
        // top-down DIB sized `width` x `height`, `pixels_ptr` is a valid
        // out-parameter.
        let hbitmap = match unsafe {
            CreateDIBSection(
                Some(hdc_screen),
                &bitmap_info,
                DIB_RGB_COLORS,
                &mut pixels_ptr,
                None,
                0,
            )
        } {
            Ok(hbitmap) => hbitmap,
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

        // Safety: `hdc_mem` and `hbitmap` were both just created above.
        let old_bitmap = unsafe { SelectObject(hdc_mem, hbitmap.into()) };
        if old_bitmap.is_invalid() {
            // Safety: all three were created above and not used again on
            // this path.
            unsafe {
                let _ = DeleteObject(hbitmap.into());
                let _ = DeleteDC(hdc_mem);
                ReleaseDC(None, hdc_screen);
            }
            return Err(CaptureError::Backend("SelectObject failed".to_string()));
        }

        Ok(Self {
            hdc_screen,
            hdc_mem,
            hbitmap,
            old_bitmap,
            pixels: pixels_ptr.cast(),
            origin,
            width,
            height,
            pacer: FramePacer::new(fps),
            prev: None,
        })
    }
}

impl FrameSource for GdiSource {
    fn next_frame(&mut self) -> Result<RawFrame, CaptureError> {
        loop {
            self.pacer.wait();

            // Safety: `hdc_mem` and `hdc_screen` are valid for the whole
            // lifetime of `self`. No `CAPTUREBLT` flag: this blits only
            // DWM's composited desktop image, which is the same "no cursor,
            // no layered click-through popups" behaviour `ScapSource`
            // already has with `show_cursor: false` -- decided by the
            // architect for slice 2.1c.
            unsafe {
                BitBlt(
                    self.hdc_mem,
                    0,
                    0,
                    self.width as i32,
                    self.height as i32,
                    Some(self.hdc_screen),
                    self.origin.0,
                    self.origin.1,
                    SRCCOPY,
                )
            }
            .map_err(|err| CaptureError::Backend(format!("BitBlt: {err}")))?;

            let len = self.width as usize * self.height as usize * 4;
            // Safety: `self.pixels` points into the DIB section backing
            // `self.hbitmap`, which stays selected into `self.hdc_mem` (and
            // therefore alive and sized for exactly `width * height * 4`
            // bytes of 32bpp pixels) for the whole lifetime of `self`; GDI
            // calls are synchronous, so `BitBlt` above has finished writing
            // every pixel by the time it returns control here.
            let cur: &[u8] = unsafe { slice::from_raw_parts(self.pixels, len) };

            if frame_unchanged(self.prev.as_deref(), cur) {
                continue;
            }

            // One copy per changed frame: `data` is what `RawFrame` carries
            // out, `prev` needs its own owned copy to compare the next
            // `BitBlt` result against (the DIB's bytes change in place on
            // every `BitBlt`, so nothing here can be borrowed across
            // iterations).
            let data = cur.to_vec();
            self.prev = Some(data.clone());

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

impl Drop for GdiSource {
    fn drop(&mut self) {
        // Safety: reverses `new`'s construction order; every handle here
        // was created there and is not touched again after this.
        unsafe {
            SelectObject(self.hdc_mem, self.old_bitmap);
            let _ = DeleteObject(self.hbitmap.into());
            let _ = DeleteDC(self.hdc_mem);
            ReleaseDC(None, self.hdc_screen);
        }
    }
}

// SAFETY: like `ScapSource`, `GdiSource` is only ever touched from the
// single dedicated capture thread that owns it for its entire lifetime
// (created, used and dropped there); we only need `Send` to move the
// freshly-built value into that thread once. The raw `pixels` pointer is
// never read or written from any other thread.
unsafe impl Send for GdiSource {}
