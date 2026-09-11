//! The real `Injector`: macOS (CGEvent) via `enigo`, Windows mouse/wheel via
//! `enigo` and keys via `SendInput` directly (see
//! `docs/host-libs-api-notes.md`'s `enigo` section).
//!
//! macOS requires the "Universal Access" (Accessibility) TCC permission;
//! without it, events are silently not delivered (see `docs/dev-run.md`).
//! There is nobody to click a permission dialog in a headless/terminal
//! session, so `Settings::open_prompt_to_get_permissions` is left `false`.
//!
//! On Windows, key events bypass `enigo::raw`: `input::keymap`'s Windows
//! table encodes the "extended key" scancode prefix as `0xE0xx`, but
//! `enigo` 0.6.1's `raw()` derives the extended-key flag itself from an
//! incomplete virtual-key table and doesn't accept that prefix (see
//! `docs/host-libs-api-notes.md`). `crate::platform::windows::keyboard`
//! sends the scancode as-is via `SendInput` instead.

use ::enigo::{Axis, Button, Coordinate, Direction, Enigo, Keyboard, Mouse, Settings};

use proto::input::PointerButton;

use super::Injector;

pub struct EnigoInjector {
    enigo: Enigo,
}

impl EnigoInjector {
    pub fn new() -> anyhow::Result<Self> {
        let settings = Settings {
            open_prompt_to_get_permissions: false,
            release_keys_when_dropped: true,
            ..Default::default()
        };
        let enigo =
            Enigo::new(&settings).map_err(|err| anyhow::anyhow!("failed to init enigo: {err}"))?;
        Ok(Self { enigo })
    }
}

fn to_enigo_button(button: PointerButton) -> Button {
    match button {
        PointerButton::Left => Button::Left,
        PointerButton::Middle => Button::Middle,
        PointerButton::Right => Button::Right,
        PointerButton::Back => Button::Back,
        PointerButton::Forward => Button::Forward,
    }
}

fn direction(pressed: bool) -> Direction {
    if pressed {
        Direction::Press
    } else {
        Direction::Release
    }
}

impl Injector for EnigoInjector {
    fn pointer_move(&mut self, x: i32, y: i32) {
        if let Err(err) = self.enigo.move_mouse(x, y, Coordinate::Abs) {
            tracing::warn!(?err, x, y, "failed to move pointer");
        }
    }

    fn button(&mut self, button: PointerButton, pressed: bool) {
        if let Err(err) = self
            .enigo
            .button(to_enigo_button(button), direction(pressed))
        {
            tracing::warn!(?err, ?button, pressed, "failed to send pointer button");
        }
    }

    fn wheel(&mut self, dx: i32, dy: i32) {
        // Two independent calls: `enigo::Mouse::scroll` takes one axis at a
        // time, and a single wheel event rarely carries both a horizontal
        // and vertical component anyway.
        if dy != 0 {
            if let Err(err) = self.enigo.scroll(dy, Axis::Vertical) {
                tracing::warn!(?err, dy, "failed to scroll vertically");
            }
        }
        if dx != 0 {
            if let Err(err) = self.enigo.scroll(dx, Axis::Horizontal) {
                tracing::warn!(?err, dx, "failed to scroll horizontally");
            }
        }
    }

    #[cfg(target_os = "windows")]
    fn key(&mut self, keycode: u16, pressed: bool) {
        if let Err(err) = crate::platform::windows::keyboard::send_scancode(keycode, pressed) {
            tracing::warn!(?err, keycode, pressed, "failed to send key event");
        }
    }

    #[cfg(not(target_os = "windows"))]
    fn key(&mut self, keycode: u16, pressed: bool) {
        if let Err(err) = self.enigo.raw(keycode, direction(pressed)) {
            tracing::warn!(?err, keycode, pressed, "failed to send key event");
        }
    }

    fn screen_size(&self) -> (i32, i32) {
        self.enigo.main_display().unwrap_or_else(|err| {
            tracing::warn!(
                ?err,
                "failed to query main display size, defaulting to 1920x1080"
            );
            (1920, 1080)
        })
    }
}
