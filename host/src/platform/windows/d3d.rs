//! Predicts whether Windows Graphics Capture (`scap`, via `windows-capture`
//! 1.4.4) can even start, without paying for `scap`'s own panic if it can't.
//!
//! `windows-capture`'s `create_d3d_device`
//! (`windows-capture-1.4.4/src/d3d11.rs:55`) calls `D3D11CreateDevice` and
//! turns a negotiated feature level below `D3D_FEATURE_LEVEL_11_0` into
//! `Error::FeatureLevelNotSatisfied` -- correctly, as a `Result`. But `scap`
//! 0.0.8 unwraps the future that eventually carries that error
//! (`scap-0.0.8/src/capturer/engine/win/mod.rs:120`:
//! `Capturer::start_free_threaded(st.to_owned()).unwrap()`), so on hardware
//! whose driver caps out below Direct3D 11 -- the owner's Windows 10 test
//! bench has an ATI Radeon HD 4600, Direct3D 10.1, a legacy WDDM 1.1 driver
//! -- `bench`/`serve` panic instead of erroring out. `main.rs` calls
//! `wgc_supported()` first and falls back to `capture::gdi::GdiSource` when
//! it returns `false`, so the panic is never reached.

use windows::Win32::Foundation::HMODULE;
use windows::Win32::Graphics::Direct3D::{
    D3D_DRIVER_TYPE_HARDWARE, D3D_FEATURE_LEVEL, D3D_FEATURE_LEVEL_10_0, D3D_FEATURE_LEVEL_10_1,
    D3D_FEATURE_LEVEL_11_0, D3D_FEATURE_LEVEL_11_1, D3D_FEATURE_LEVEL_9_1, D3D_FEATURE_LEVEL_9_2,
    D3D_FEATURE_LEVEL_9_3,
};
use windows::Win32::Graphics::Direct3D11::{
    D3D11CreateDevice, D3D11_CREATE_DEVICE_BGRA_SUPPORT, D3D11_SDK_VERSION,
};

/// `true` if the system can create a hardware Direct3D 11 device at feature
/// level 11_0 or above, i.e. Windows Graphics Capture should be able to
/// start. Mirrors `windows_capture::d3d11::create_d3d_device` exactly (same
/// driver type, flags, descending feature-level list and SDK version) so
/// its failure mode is predicted here first, without keeping (or leaking)
/// the device/context this call produces -- both output parameters are
/// skipped (`None`) since only the negotiated feature level is needed.
pub fn wgc_supported() -> bool {
    let feature_levels = [
        D3D_FEATURE_LEVEL_11_1,
        D3D_FEATURE_LEVEL_11_0,
        D3D_FEATURE_LEVEL_10_1,
        D3D_FEATURE_LEVEL_10_0,
        D3D_FEATURE_LEVEL_9_3,
        D3D_FEATURE_LEVEL_9_2,
        D3D_FEATURE_LEVEL_9_1,
    ];
    let mut level = D3D_FEATURE_LEVEL::default();

    // Safety: `None` for the adapter selects the default adapter (the same
    // call shape `windows-capture` itself makes with a bare `None`);
    // `level` is a valid, correctly-typed out-parameter. Neither the device
    // nor the device context is requested (`None` for both), so nothing
    // this call could produce needs releasing afterwards.
    let result = unsafe {
        D3D11CreateDevice(
            None,
            D3D_DRIVER_TYPE_HARDWARE,
            HMODULE::default(),
            D3D11_CREATE_DEVICE_BGRA_SUPPORT,
            Some(&feature_levels),
            D3D11_SDK_VERSION,
            None,
            Some(&mut level),
            None,
        )
    };

    match result {
        Ok(()) if level.0 >= D3D_FEATURE_LEVEL_11_0.0 => true,
        Ok(()) => {
            tracing::info!(
                feature_level = level.0,
                "Direct3D 11 device is below the feature level 11_0 windows-capture requires; \
                 falling back to GDI capture"
            );
            false
        }
        Err(err) => {
            tracing::info!(
                error = %err,
                "failed to create a Direct3D 11 device; falling back to GDI capture"
            );
            false
        }
    }
}
