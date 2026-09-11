//! Opts the process into per-monitor-v2 DPI awareness.
//!
//! Without this, Windows treats the process as DPI-unaware and virtualizes
//! it at any scale factor other than 100%: `GetSystemMetrics(SM_CXSCREEN /
//! SM_CYSCREEN)` reports the *logical* screen size and GDI
//! (`capture::gdi::GdiSource`'s `BitBlt`) hands back the desktop scaled
//! down to that logical size -- a blurry 1536x864 instead of a crisp
//! 1920x1080 at 125%. Mouse injection stays consistent either way (`enigo`
//! sizes the screen and scales absolute coordinates from the same
//! `SM_CXSCREEN` value), so this is about capture resolution, not about
//! capture and input disagreeing. Calling this once, before any
//! GDI/`GetSystemMetrics` call, makes both report physical pixels.

use windows::Win32::UI::HiDpi::{
    SetProcessDpiAwarenessContext, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2,
};

/// Best-effort: an `Err` here means DPI awareness was already declared (a
/// manifest, or a second call) or the OS predates per-monitor-v2 -- neither
/// is a reason to fail startup, so this only logs and moves on.
pub fn set_dpi_aware() {
    // Safety: `SetProcessDpiAwarenessContext` has no preconditions beyond
    // passing a valid `DPI_AWARENESS_CONTEXT` constant, which
    // `DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2` is.
    if let Err(err) =
        unsafe { SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2) }
    {
        tracing::debug!(
            error = %err,
            "failed to set per-monitor-v2 DPI awareness (already set, or unsupported OS)"
        );
    }
}
