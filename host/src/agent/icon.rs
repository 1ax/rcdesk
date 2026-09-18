//! Draws the tray/menu-bar icon in code -- no bundled image resources, no
//! new asset pipeline, just a small monitor silhouette whose fill/indicator
//! differs by `IconState`.
//!
//! macOS menu bar icons are conventionally monochrome "template" images:
//! black pixels with the shape carried entirely in the alpha channel, and
//! the system re-tints them for light/dark mode and highlight state (see
//! `TrayIconBuilder::with_icon_as_template`, which `agent_main` sets `true`
//! for exactly this reason). Windows has no such convention -- the taskbar
//! shows the icon's own RGB verbatim on both light and dark backgrounds --
//! so this draws flat, saturated colors there instead. `render_icon` picks
//! between the two styles with `cfg!(target_os = "macos")` rather than a
//! runtime flag: there is exactly one real answer per build, the same
//! reasoning `capture`/`cursor`/`clipboard`'s per-OS backend selection uses.

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

/// Renders `state` as a `size` x `size` RGBA buffer (row-major, 4 bytes per
/// pixel, straight alpha) -- a monitor silhouette (bezel + stand) whose
/// screen fill is state-dependent. See the module doc comment for the
/// macOS-template vs. Windows-color split.
pub fn render_icon(state: IconState, size: u32) -> Vec<u8> {
    let s = i64::from(size);
    let mut buf = vec![0u8; (size as usize) * (size as usize) * 4];
    let is_macos = cfg!(target_os = "macos");

    // Monitor bezel: inset rectangle, roughly 4:3, with a small stand below
    // it. All bounds are fractions of `size` so this scales to any
    // reasonable tray icon size (16px menu bar icons up to Windows' larger
    // taskbar sizes) without hard-coded pixel offsets.
    let left = s / 6;
    let right = s - s / 6;
    let top = s / 6;
    let bottom = top + (right - left) * 3 / 4;
    let stroke = (s / 12).max(1);

    let put = |buf: &mut [u8], x: i64, y: i64, rgba: (u8, u8, u8, u8)| {
        if x < 0 || y < 0 || x >= s || y >= s {
            return;
        }
        let idx = ((y * s + x) * 4) as usize;
        buf[idx] = rgba.0;
        buf[idx + 1] = rgba.1;
        buf[idx + 2] = rgba.2;
        buf[idx + 3] = rgba.3;
    };

    let bezel_color: (u8, u8, u8, u8) = if is_macos {
        (0, 0, 0, 255)
    } else {
        (90, 90, 90, 255)
    };
    let screen_color: (u8, u8, u8, u8) = match (state, is_macos) {
        // macOS: shape/alpha only -- flat black at three different alpha
        // levels, faint (idle) -> solid (session) -> almost invisible
        // (offline, "screen is off").
        (IconState::Idle, true) => (0, 0, 0, 90),
        (IconState::Session, true) => (0, 0, 0, 220),
        (IconState::Offline, true) => (0, 0, 0, 25),
        // Windows: real color, opaque so it reads on any taskbar theme.
        (IconState::Idle, false) => (190, 190, 190, 255),
        (IconState::Session, false) => (40, 170, 70, 255),
        (IconState::Offline, false) => (220, 130, 30, 255),
    };

    for y in top..bottom {
        for x in left..right {
            let on_bezel = x < left + stroke
                || x >= right - stroke
                || y < top + stroke
                || y >= bottom - stroke;
            put(
                &mut buf,
                x,
                y,
                if on_bezel { bezel_color } else { screen_color },
            );
        }
    }

    // Stand, shared shape across all three states.
    let stand_w = (right - left) / 3;
    let stand_x = (left + right) / 2 - stand_w / 2;
    for y in bottom..(bottom + stroke).min(s) {
        for x in stand_x..(stand_x + stand_w) {
            put(&mut buf, x, y, bezel_color);
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
        for state in [IconState::Idle, IconState::Session, IconState::Offline] {
            let buf = render_icon(state, 22);
            assert!(
                buf.as_chunks::<4>().0.iter().any(|px| px[3] == 255),
                "{state:?} icon has no fully opaque pixel (the bezel should always be)"
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
}
