//! Windows system cursor shape via `GetCursorInfo`/`GetIconInfo`/
//! `GetDIBits`.
//!
//! Compiles only on `windows-latest` in CI: there is no Windows machine
//! available to this executor run, so every signature/type below comes
//! from reading the `windows` 0.61.3 source fetched into the local
//! registry cache (`cargo fetch --target x86_64-pc-windows-msvc`), not from
//! memory -- see executor rule 3.

use std::mem::size_of;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use windows::Win32::Graphics::Gdi::{
    CreateCompatibleDC, DeleteDC, DeleteObject, GetDIBits, GetObjectW, BITMAP, BITMAPINFO,
    BITMAPINFOHEADER, BI_RGB, DIB_RGB_COLORS, HBITMAP,
};
use windows::Win32::UI::WindowsAndMessaging::{
    GetCursorInfo, GetIconInfo, CURSORINFO, CURSOR_SHOWING, HICON, ICONINFO,
};

use super::{CursorImage, CursorSource, CursorState};

/// Minimum interval between "failed to read cursor" debug logs.
const LOG_INTERVAL: Duration = Duration::from_secs(1);

pub struct WinCursorSource {
    last_log: Mutex<Option<Instant>>,
}

impl WinCursorSource {
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

impl Default for WinCursorSource {
    fn default() -> Self {
        Self::new()
    }
}

impl CursorSource for WinCursorSource {
    fn current(&mut self) -> Option<CursorState> {
        // D45: поток курсора следует за входным рабочим столом (Winlogon/UAC).
        crate::platform::desktop::sync_thread();
        let mut info = CURSORINFO {
            cbSize: size_of::<CURSORINFO>() as u32,
            ..Default::default()
        };
        // Safety: `info` is a correctly-sized, valid out-parameter.
        if let Err(err) = unsafe { GetCursorInfo(&mut info) } {
            self.log_failure(&format!("GetCursorInfo: {err}"));
            return None;
        }

        if info.flags.0 & CURSOR_SHOWING.0 == 0 || info.hCursor.is_invalid() {
            return Some(CursorState::Hidden);
        }

        // `HCURSOR` and `HICON` share the same underlying Win32 handle type
        // (the C API even `typedef`s one to the other); `GetIconInfo`
        // accepts either interchangeably.
        let hicon = HICON(info.hCursor.0);
        let mut icon_info = ICONINFO::default();
        // Safety: `hicon` was just obtained from `GetCursorInfo`, `icon_info`
        // is a valid out-parameter.
        if let Err(err) = unsafe { GetIconInfo(hicon, &mut icon_info) } {
            self.log_failure(&format!("GetIconInfo: {err}"));
            return None;
        }

        let result = if !icon_info.hbmColor.is_invalid() {
            self.color_cursor_image(
                icon_info.hbmColor,
                icon_info.hbmMask,
                icon_info.xHotspot,
                icon_info.yHotspot,
            )
        } else {
            self.mono_cursor_image(icon_info.hbmMask, icon_info.xHotspot, icon_info.yHotspot)
        };

        // MSDN: the mask/color bitmaps `GetIconInfo` allocates are owned by
        // the caller, who must delete them.
        // Safety: both handles (when valid) were just allocated by
        // `GetIconInfo` above and are not used again after this.
        unsafe {
            if !icon_info.hbmMask.is_invalid() {
                let _ = DeleteObject(icon_info.hbmMask.into());
            }
            if !icon_info.hbmColor.is_invalid() {
                let _ = DeleteObject(icon_info.hbmColor.into());
            }
        }

        result
    }
}

impl WinCursorSource {
    /// Modern cursors: a 32bpp color bitmap, generally already carrying a
    /// real (straight) alpha channel.
    fn color_cursor_image(
        &self,
        hbm: HBITMAP,
        hbm_mask: HBITMAP,
        hotspot_x: u32,
        hotspot_y: u32,
    ) -> Option<CursorState> {
        let mut bmp = BITMAP::default();
        // Safety: `hbm` is a valid bitmap handle from `GetIconInfo`, `bmp` a
        // correctly-sized out-parameter.
        let written = unsafe {
            GetObjectW(
                hbm.into(),
                size_of::<BITMAP>() as i32,
                Some(std::ptr::addr_of_mut!(bmp).cast()),
            )
        };
        if written == 0 {
            self.log_failure("GetObjectW(hbmColor)");
            return None;
        }
        let (width, height) = (bmp.bmWidth, bmp.bmHeight);
        if width <= 0 || height <= 0 {
            self.log_failure("hbmColor has non-positive dimensions");
            return None;
        }

        let header = BITMAPINFOHEADER {
            biSize: size_of::<BITMAPINFOHEADER>() as u32,
            biWidth: width,
            // Negative height: request the DIB top-down, matching the
            // row order `CursorImage::rgba` is documented to use.
            biHeight: -height,
            biPlanes: 1,
            biBitCount: 32,
            biCompression: BI_RGB.0,
            ..Default::default()
        };
        let mut bitmap_info = BITMAPINFO {
            bmiHeader: header,
            ..Default::default()
        };

        let mut buf = vec![0u8; width as usize * height as usize * 4];
        // Safety: `hdc` below is a freshly created memory DC, `hbm` a valid
        // bitmap, `buf` sized for exactly `height` scanlines of `width * 4`
        // bytes (32bpp) each, matching `bitmap_info`.
        let hdc = unsafe { CreateCompatibleDC(None) };
        if hdc.is_invalid() {
            self.log_failure("CreateCompatibleDC");
            return None;
        }
        let lines = unsafe {
            GetDIBits(
                hdc,
                hbm,
                0,
                height as u32,
                Some(buf.as_mut_ptr().cast()),
                &mut bitmap_info,
                DIB_RGB_COLORS,
            )
        };
        // Safety: `hdc` was created by `CreateCompatibleDC` just above and
        // is not used again after this.
        unsafe {
            let _ = DeleteDC(hdc);
        }
        if lines == 0 {
            self.log_failure("GetDIBits(hbmColor)");
            return None;
        }

        // 32bpp DIBs are BGRA in memory (little-endian); swap R/B to RGBA.
        for px in buf.as_chunks_mut::<4>().0 {
            px.swap(0, 2);
        }

        // V-7: all-zero alpha means a legacy cursor without a real alpha
        // channel; take transparency from the AND mask instead.
        if buf.as_chunks::<4>().0.iter().all(|px| px[3] == 0) {
            self.apply_mask_alpha(hbm_mask, width, height, &mut buf);
        }

        Some(CursorState::Shape(CursorImage {
            width: width as u32,
            height: height as u32,
            hotspot_x: f64::from(hotspot_x),
            hotspot_y: f64::from(hotspot_y),
            scale: 1.0,
            rgba: buf,
        }))
    }

    /// Reads the first `rows` scanlines of a 1bpp mask bitmap as a top-down
    /// DIB. Returns the bytes and the row stride (rows are padded to 4-byte
    /// boundaries). `None` (already logged) when the read fails.
    fn read_mask_bits(&self, hbm_mask: HBITMAP, width: i32, rows: i32) -> Option<(Vec<u8>, usize)> {
        let header = BITMAPINFOHEADER {
            biSize: size_of::<BITMAPINFOHEADER>() as u32,
            biWidth: width,
            biHeight: -rows,
            biPlanes: 1,
            biBitCount: 1,
            biCompression: BI_RGB.0,
            ..Default::default()
        };
        let mut bitmap_info = BITMAPINFO {
            bmiHeader: header,
            ..Default::default()
        };

        // 1bpp DIB rows are padded to 4-byte (32-bit) boundaries.
        let stride = (width as u32).div_ceil(32) as usize * 4;
        let mut buf = vec![0u8; stride * rows as usize];

        // Safety: `hdc` is a freshly created memory DC, `hbm_mask` a valid
        // bitmap, `buf` sized for exactly `rows` scanlines of `stride`
        // bytes (1bpp) each, matching `bitmap_info`.
        let hdc = unsafe { CreateCompatibleDC(None) };
        if hdc.is_invalid() {
            self.log_failure("CreateCompatibleDC");
            return None;
        }
        let lines = unsafe {
            GetDIBits(
                hdc,
                hbm_mask,
                0,
                rows as u32,
                Some(buf.as_mut_ptr().cast()),
                &mut bitmap_info,
                DIB_RGB_COLORS,
            )
        };
        // Safety: `hdc` was created above and is not used again.
        unsafe {
            let _ = DeleteDC(hdc);
        }
        if lines == 0 {
            self.log_failure("GetDIBits(hbmMask)");
            return None;
        }
        Some((buf, stride))
    }

    /// V-7: legacy colour cursors (e.g. converted 24-bit ones) come back
    /// from the 32bpp DIB with an all-zero alpha channel, which would render
    /// fully transparent. Rebuilds alpha from the AND mask (bit 1 =
    /// transparent). For colour cursors the mask is just the AND part
    /// (`height` rows); if it reports `2 * height`, the top half is used.
    /// Leaves `rgba` untouched if the mask can't be read.
    fn apply_mask_alpha(&self, hbm_mask: HBITMAP, width: i32, height: i32, rgba: &mut [u8]) {
        if hbm_mask.is_invalid() {
            return;
        }
        let mut bmp = BITMAP::default();
        // Safety: `hbm_mask` is a valid bitmap handle from `GetIconInfo`,
        // `bmp` a correctly-sized out-parameter.
        let written = unsafe {
            GetObjectW(
                hbm_mask.into(),
                size_of::<BITMAP>() as i32,
                Some(std::ptr::addr_of_mut!(bmp).cast()),
            )
        };
        if written == 0 {
            self.log_failure("GetObjectW(hbmMask) for alpha");
            return;
        }
        if bmp.bmBitsPixel != 1 || bmp.bmWidth < width || bmp.bmHeight < height {
            self.log_failure("hbmMask unusable for alpha");
            return;
        }
        let Some((mask, stride)) = self.read_mask_bits(hbm_mask, width, height) else {
            return;
        };
        for y in 0..height as usize {
            for x in 0..width as usize {
                let and_bit = (mask[y * stride + x / 8] >> (7 - (x % 8))) & 1 == 1;
                rgba[(y * width as usize + x) * 4 + 3] = if and_bit { 0 } else { 255 };
            }
        }
    }

    /// Legacy monochrome cursors (`hbmColor` null): `hbmMask` packs an AND
    /// mask (top half) and an XOR mask (bottom half), 1 bit per pixel. This
    /// reconstructs a black/white image with alpha derived from the AND
    /// mask; the rare AND=1/XOR=1 ("invert the background") combination is
    /// approximated as opaque black, since there is no "invert" in RGBA.
    fn mono_cursor_image(
        &self,
        hbm_mask: HBITMAP,
        hotspot_x: u32,
        hotspot_y: u32,
    ) -> Option<CursorState> {
        let mut bmp = BITMAP::default();
        // Safety: `hbm_mask` is a valid bitmap handle from `GetIconInfo`,
        // `bmp` a correctly-sized out-parameter.
        let written = unsafe {
            GetObjectW(
                hbm_mask.into(),
                size_of::<BITMAP>() as i32,
                Some(std::ptr::addr_of_mut!(bmp).cast()),
            )
        };
        if written == 0 {
            self.log_failure("GetObjectW(hbmMask)");
            return None;
        }
        let width = bmp.bmWidth;
        let mask_height = bmp.bmHeight;
        if width <= 0 || mask_height <= 0 || mask_height % 2 != 0 {
            self.log_failure("hbmMask has unexpected dimensions");
            return None;
        }
        let height = mask_height / 2;

        let (buf, stride) = self.read_mask_bits(hbm_mask, width, mask_height)?;

        let bit_at = |x: usize, y: usize| -> bool {
            let byte = buf[y * stride + x / 8];
            (byte >> (7 - (x % 8))) & 1 == 1
        };

        let (width_u, height_u) = (width as usize, height as usize);
        let mut rgba = vec![0u8; width_u * height_u * 4];
        for y in 0..height_u {
            for x in 0..width_u {
                let and_bit = bit_at(x, y);
                let xor_bit = bit_at(x, y + height_u);
                let (color, alpha) = match (and_bit, xor_bit) {
                    (true, false) => (0u8, 0u8),     // screen shows through
                    (false, false) => (0u8, 255u8),  // opaque black
                    (false, true) => (255u8, 255u8), // opaque white
                    (true, true) => (0u8, 255u8),    // invert -- approximated as opaque black
                };
                let i = (y * width_u + x) * 4;
                rgba[i] = color;
                rgba[i + 1] = color;
                rgba[i + 2] = color;
                rgba[i + 3] = alpha;
            }
        }

        Some(CursorState::Shape(CursorImage {
            width: width as u32,
            height: height as u32,
            hotspot_x: f64::from(hotspot_x),
            hotspot_y: f64::from(hotspot_y),
            scale: 1.0,
            rgba,
        }))
    }
}
