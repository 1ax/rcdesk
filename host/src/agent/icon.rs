//! Tray/menu-bar icon: the rcdesk logo, as a prebuilt raw-RGBA base image
//! (`icon/mono-44.rgba` on macOS, `icon/color-32.rgba` elsewhere) embedded
//! with `include_bytes!`. The files come from `installer/logo.svg` via
//! `python3 installer/make-icons.py` (see `icon/README.md`); do not edit
//! them by hand.
//!
//! macOS menu bar icons are conventionally monochrome "template" images:
//! black pixels with the shape carried entirely in the alpha channel, and
//! the system re-tints them for light/dark mode and highlight state (see
//! `TrayIconBuilder::with_icon_as_template`, which `agent_main` sets `true`
//! for exactly this reason). The three states there differ only in alpha
//! (full / 0.55 / 0.25). Windows has no such convention -- the taskbar shows
//! the icon's own RGB verbatim -- so the logo keeps its colors for `Session`
//! and is turned gray for `Idle` / `Offline` (the latter also half
//! transparent). `render_icon` picks the base with `cfg!(target_os =
//! "macos")` rather than a runtime flag: there is exactly one real answer
//! per build. Other targets (Linux CI builds the library) use the color base.

/// Which of the tray icon's three looks to draw. See `menu::menu_model`'s
/// doc comment for how `signaling::AgentStatus` maps onto this.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IconState {
    /// Registered with the signaling server, no session running.
    Idle,
    /// A session is active.
    Session,
    /// Disconnected or reconnecting.
    Offline,
}

#[cfg(target_os = "macos")]
const BASE: &[u8] = include_bytes!("icon/mono-44.rgba");
#[cfg(target_os = "macos")]
const BASE_SIZE: usize = 44;
#[cfg(not(target_os = "macos"))]
const BASE: &[u8] = include_bytes!("icon/color-32.rgba");
#[cfg(not(target_os = "macos"))]
const BASE_SIZE: usize = 32;

/// Scales `px` by `factor` (0.0..=1.0), rounding to nearest.
fn scale_u8(v: u8, factor: f32) -> u8 {
    (f32::from(v) * factor).round().clamp(0.0, 255.0) as u8
}

/// Renders `state` as a `size` x `size` RGBA buffer (row-major, 4 bytes per
/// pixel, straight alpha): the logo base image, nearest-neighbor scaled when
/// `size` differs from the base, with the state look applied. See the module
/// doc comment for the macOS-template vs. Windows-color split.
pub fn render_icon(state: IconState, size: u32) -> Vec<u8> {
    let size = size as usize;
    let is_macos = cfg!(target_os = "macos");
    let mut buf = Vec::with_capacity(size * size * 4);
    for y in 0..size {
        let sy = y * BASE_SIZE / size;
        for x in 0..size {
            let sx = x * BASE_SIZE / size;
            let i = (sy * BASE_SIZE + sx) * 4;
            let (r, g, b, a) = (BASE[i], BASE[i + 1], BASE[i + 2], BASE[i + 3]);
            let px = if is_macos {
                let k = match state {
                    IconState::Session => 1.0,
                    IconState::Idle => 0.55,
                    IconState::Offline => 0.25,
                };
                [r, g, b, scale_u8(a, k)]
            } else {
                match state {
                    IconState::Session => [r, g, b, a],
                    IconState::Idle | IconState::Offline => {
                        // Rec. 601 luma.
                        let luma =
                            (0.299 * f32::from(r) + 0.587 * f32::from(g) + 0.114 * f32::from(b))
                                .round()
                                .clamp(0.0, 255.0) as u8;
                        let k = if state == IconState::Offline {
                            0.5
                        } else {
                            1.0
                        };
                        [luma, luma, luma, scale_u8(a, k)]
                    }
                }
            };
            buf.extend_from_slice(&px);
        }
    }
    buf
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn buffer_length_matches_size_squared_times_four() {
        for size in [16, 22, 32] {
            let buf = render_icon(IconState::Idle, size);
            assert_eq!(buf.len(), (size as usize) * (size as usize) * 4);
        }
    }

    #[test]
    fn has_opaque_pixels() {
        // Only `Session` keeps the base alpha; `Idle`/`Offline` are dimmed on
        // purpose, so they only need to be visible (non-zero alpha).
        let buf = render_icon(IconState::Session, 22);
        assert!(
            buf.as_chunks::<4>().0.iter().any(|px| px[3] == 255),
            "Session icon has no fully opaque pixel"
        );
        for state in [IconState::Idle, IconState::Offline] {
            let buf = render_icon(state, 22);
            assert!(
                buf.as_chunks::<4>().0.iter().any(|px| px[3] > 0),
                "{state:?} icon is fully transparent"
            );
        }
    }

    #[test]
    fn the_three_states_render_different_buffers() {
        let idle = render_icon(IconState::Idle, 22);
        let session = render_icon(IconState::Session, 22);
        let offline = render_icon(IconState::Offline, 22);

        assert_ne!(idle, session);
        assert_ne!(idle, offline);
        assert_ne!(session, offline);
    }

    #[test]
    fn base_size_render_matches_base_length_and_is_not_empty() {
        let buf = render_icon(IconState::Session, BASE_SIZE as u32);
        assert_eq!(BASE.len(), BASE_SIZE * BASE_SIZE * 4);
        assert_eq!(buf.len(), BASE.len());
        assert!(buf.as_chunks::<4>().0.iter().any(|px| px[3] > 0));
    }
}
