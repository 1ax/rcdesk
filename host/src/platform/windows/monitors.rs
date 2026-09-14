//! Enumerates capturable displays via raw `EnumDisplayMonitors`/
//! `GetMonitorInfoW`, instead of `scap::get_all_targets` (used by
//! `capture::list_displays` on macOS): `scap::get_all_targets` also
//! enumerates windows, and its Windows window-listing path has
//! `unwrap()`/`expect()` calls that can panic outright rather than return an
//! error (see `docs/host-libs-api-notes.md`); on top of that, the GDI
//! capture path this module's output feeds (`--capture gdi`, used on the
//! owner's Windows 10 test bench, an ATI Radeon HD 4600 with no Direct3D 11)
//! never exercises `scap` at all, so going through it here just to list
//! displays would pull in a dependency this path otherwise avoids entirely.
//!
//! Compiles only on `windows-latest` in CI: there is no Windows machine
//! available to this executor run, so every signature/type below comes from
//! reading the `windows` 0.61.3 source fetched into the local registry
//! cache, not from memory -- see executor rule 3 (the same approach
//! `capture::gdi`, `cursor::windows` and `platform::windows::keyboard`
//! document and use). The `MONITORINFOEXW`/`cbSize` cast follows the same
//! pattern as `windows-capture` 1.4.4's `Monitor::device_name`
//! (`windows-capture-1.4.4/src/monitor.rs`).

use std::mem::size_of;

use windows::Win32::Foundation::{BOOL, LPARAM, RECT, TRUE};
use windows::Win32::Graphics::Gdi::{
    EnumDisplayMonitors, GetMonitorInfoW, HDC, HMONITOR, MONITORINFO, MONITORINFOEXW,
};
use windows::Win32::UI::WindowsAndMessaging::MONITORINFOF_PRIMARY;

use crate::capture::{CaptureError, DisplayInfo};

/// Lists capturable displays. Unlike `capture::list_displays` on macOS, this
/// never checks a screen-recording permission: Windows has no such gate for
/// enumerating monitors (only for the actual capture backends).
pub fn list() -> Result<Vec<DisplayInfo>, CaptureError> {
    let mut handles: Vec<HMONITOR> = Vec::new();
    // Safety: `handles` is a valid, live `Vec` for the whole (synchronous)
    // call; the callback below only ever runs during this call, on this
    // thread, and only pushes into the vector it's handed through `dwdata`.
    unsafe {
        EnumDisplayMonitors(
            None,
            None,
            Some(enum_monitors_callback),
            LPARAM(std::ptr::addr_of_mut!(handles) as isize),
        )
    }
    .ok()
    .map_err(|err| CaptureError::Backend(format!("EnumDisplayMonitors: {err}")))?;

    handles.into_iter().map(monitor_info).collect()
}

/// Turns one `HMONITOR` from `list`'s callback into a `DisplayInfo` via
/// `GetMonitorInfoW`.
fn monitor_info(hmonitor: HMONITOR) -> Result<DisplayInfo, CaptureError> {
    let mut info = MONITORINFOEXW {
        monitorInfo: MONITORINFO {
            // Sized for the extended struct (not just `MONITORINFO`) so
            // `GetMonitorInfoW` knows to also fill in `szDevice`.
            cbSize: size_of::<MONITORINFOEXW>() as u32,
            ..Default::default()
        },
        ..Default::default()
    };
    // Safety: `info` is a correctly-sized (`cbSize` set above), valid
    // out-parameter; `hmonitor` came from `EnumDisplayMonitors` above, and
    // an invalid/stale handle is reported via the return value, not
    // undefined behaviour. `GetMonitorInfoW` takes `*mut MONITORINFO`; the
    // cast is safe because `MONITORINFOEXW` starts with an embedded
    // `MONITORINFO` field at offset 0 (`#[repr(C)]`), the same layout
    // `windows-capture`'s `device_name()` relies on.
    let ok = unsafe { GetMonitorInfoW(hmonitor, std::ptr::addr_of_mut!(info).cast()) };
    if !ok.as_bool() {
        return Err(CaptureError::Backend(format!(
            "GetMonitorInfoW failed for monitor {:?}",
            hmonitor.0
        )));
    }

    let rect = info.monitorInfo.rcMonitor;
    let title = String::from_utf16_lossy(
        &info
            .szDevice
            .iter()
            .take_while(|&&ch| ch != 0)
            .copied()
            .collect::<Vec<u16>>(),
    );

    Ok(DisplayInfo {
        // Same truncation `scap::get_all_targets` uses for `Display::id` on
        // Windows (`display.as_raw_hmonitor() as u32`, see
        // `scap-0.0.8/src/targets/win/mod.rs`) and `capture::gdi::GdiSource`
        // already assumes when parsing `--display`, so ids from this list
        // match ids `GdiSource::new`/`ScapSource::new` accept.
        id: hmonitor.0 as u32,
        title,
        x: rect.left,
        y: rect.top,
        width: (rect.right - rect.left).max(0) as u32,
        height: (rect.bottom - rect.top).max(0) as u32,
        primary: info.monitorInfo.dwFlags & MONITORINFOF_PRIMARY != 0,
    })
}

// Callback for `EnumDisplayMonitors`: appends the monitor handle to the
// `Vec<HMONITOR>` passed through `dwdata` and asks to keep enumerating.
unsafe extern "system" fn enum_monitors_callback(
    monitor: HMONITOR,
    _hdc: HDC,
    _rect: *mut RECT,
    dwdata: LPARAM,
) -> BOOL {
    // Safety: `dwdata` is the `*mut Vec<HMONITOR>` `list` passed into
    // `EnumDisplayMonitors`, valid for the duration of that call.
    let handles = unsafe { &mut *(dwdata.0 as *mut Vec<HMONITOR>) };
    handles.push(monitor);
    TRUE
}
