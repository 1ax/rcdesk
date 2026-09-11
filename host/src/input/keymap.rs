//! Maps a W3C `KeyboardEvent.code` (the physical key the browser reports,
//! e.g. `"KeyA"`, `"ArrowLeft"`) to the platform's raw keycode for
//! `enigo::Keyboard::raw` (see `docs/host-libs-api-notes.md`'s `enigo`
//! section and ARCHITECTURE.md §7): on macOS this is a `kVK_*` CGKeyCode, on
//! Windows a PS/2 Scan Code Set 1 scancode. Layout (which character a key
//! produces) is not this module's concern -- `raw` sends the physical key,
//! and the OS applies whatever layout is active, exactly like a physical
//! keyboard would.

/// Translates a `KeyboardEvent.code` string to the current platform's raw
/// keycode. `None` for codes with no mapping on this platform (including
/// every code, always, on platforms other than macOS/Windows).
pub fn to_keycode(code: &str) -> Option<u16> {
    platform::to_keycode(code)
}

#[cfg(target_os = "macos")]
mod platform {
    /// `kVK_*` CGKeyCodes (see `HIToolbox/Events.h`). Values confirmed
    /// against the well-known Carbon keycode table; ARCHITECTURE.md's owner
    /// spot-checked `KeyA`/`Enter`/`ArrowLeft`/`MetaLeft`/`Numpad1` in
    /// `docs/host-libs-api-notes.md` before this table was written.
    pub(super) fn to_keycode(code: &str) -> Option<u16> {
        Some(match code {
            "KeyA" => 0x00,
            "KeyS" => 0x01,
            "KeyD" => 0x02,
            "KeyF" => 0x03,
            "KeyH" => 0x04,
            "KeyG" => 0x05,
            "KeyZ" => 0x06,
            "KeyX" => 0x07,
            "KeyC" => 0x08,
            "KeyV" => 0x09,
            "IntlBackslash" => 0x0A,
            "KeyB" => 0x0B,
            "KeyQ" => 0x0C,
            "KeyW" => 0x0D,
            "KeyE" => 0x0E,
            "KeyR" => 0x0F,
            "KeyY" => 0x10,
            "KeyT" => 0x11,
            "Digit1" => 0x12,
            "Digit2" => 0x13,
            "Digit3" => 0x14,
            "Digit4" => 0x15,
            "Digit6" => 0x16,
            "Digit5" => 0x17,
            "Equal" => 0x18,
            "Digit9" => 0x19,
            "Digit7" => 0x1A,
            "Minus" => 0x1B,
            "Digit8" => 0x1C,
            "Digit0" => 0x1D,
            "BracketRight" => 0x1E,
            "KeyO" => 0x1F,
            "KeyU" => 0x20,
            "BracketLeft" => 0x21,
            "KeyI" => 0x22,
            "KeyP" => 0x23,
            "Enter" => 0x24,
            "KeyL" => 0x25,
            "KeyJ" => 0x26,
            "Quote" => 0x27,
            "KeyK" => 0x28,
            "Semicolon" => 0x29,
            "Backslash" => 0x2A,
            "Comma" => 0x2B,
            "Slash" => 0x2C,
            "KeyN" => 0x2D,
            "KeyM" => 0x2E,
            "Period" => 0x2F,
            "Tab" => 0x30,
            "Space" => 0x31,
            "Backquote" => 0x32,
            "Backspace" => 0x33,
            "Escape" => 0x35,
            "MetaRight" => 0x36,
            "MetaLeft" => 0x37,
            "ShiftLeft" => 0x38,
            "CapsLock" => 0x39,
            "AltLeft" => 0x3A,
            "ControlLeft" => 0x3B,
            "ShiftRight" => 0x3C,
            "AltRight" => 0x3D,
            "ControlRight" => 0x3E,
            "NumpadDecimal" => 0x41,
            "NumpadMultiply" => 0x43,
            "NumpadAdd" => 0x45,
            "NumpadDivide" => 0x4B,
            "NumpadEnter" => 0x4C,
            "NumpadSubtract" => 0x4E,
            "Numpad0" => 0x52,
            "Numpad1" => 0x53,
            "Numpad2" => 0x54,
            "Numpad3" => 0x55,
            "Numpad4" => 0x56,
            "Numpad5" => 0x57,
            "Numpad6" => 0x58,
            "Numpad7" => 0x59,
            "Numpad8" => 0x5B,
            "Numpad9" => 0x5C,
            "F5" => 0x60,
            "F6" => 0x61,
            "F7" => 0x62,
            "F3" => 0x63,
            "F8" => 0x64,
            "F9" => 0x65,
            "F11" => 0x67,
            "F13" => 0x69,
            "F16" => 0x6A,
            "F14" => 0x6B,
            "F10" => 0x6D,
            "F12" => 0x6F,
            "F15" => 0x71,
            "Insert" => 0x72, // kVK_Help; macOS keyboards have no true Insert key.
            "Home" => 0x73,
            "PageUp" => 0x74,
            "Delete" => 0x75, // kVK_ForwardDelete
            "F4" => 0x76,
            "End" => 0x77,
            "F2" => 0x78,
            "PageDown" => 0x79,
            "F1" => 0x7A,
            "ArrowLeft" => 0x7B,
            "ArrowRight" => 0x7C,
            "ArrowDown" => 0x7D,
            "ArrowUp" => 0x7E,
            _ => return None,
        })
    }

    #[cfg(test)]
    mod tests {
        use super::to_keycode;

        #[test]
        fn maps_the_spot_checked_keys() {
            assert_eq!(to_keycode("KeyA"), Some(0x00));
            assert_eq!(to_keycode("Enter"), Some(0x24));
            assert_eq!(to_keycode("ArrowLeft"), Some(0x7B));
            assert_eq!(to_keycode("MetaLeft"), Some(0x37));
            assert_eq!(to_keycode("Numpad1"), Some(0x53));
        }

        #[test]
        fn unknown_code_maps_to_none() {
            assert_eq!(to_keycode("Unknown"), None);
        }
    }
}

#[cfg(target_os = "windows")]
mod platform {
    /// PS/2 Scan Code Set 1 scancodes (the set Windows `SendInput` with
    /// `KEYEVENTF_SCANCODE` expects). A handful of navigation/modifier keys
    /// share their base scan byte with a numpad key (e.g. `ArrowLeft` and
    /// `Numpad4` are both `0x4B`); on real hardware and in `enigo::raw`
    /// those are disambiguated by the "extended key" flag, which `enigo`
    /// derives itself from the scancode -> virtual-key translation (see
    /// `docs/host-libs-api-notes.md`). `NumpadEnter` shares `Enter`'s
    /// scancode (`0x1C`) the same way, but `enigo` 0.6.1's own source notes
    /// that virtual key isn't in its extended-key table yet (`is_extended_key`
    /// in `win_impl.rs`, "TODO: ... ENTER key in the numeric keypad ...
    /// missing"), so `NumpadEnter` currently behaves like plain `Enter` --
    /// an upstream limitation, not a bug in this table.
    pub(super) fn to_keycode(code: &str) -> Option<u16> {
        Some(match code {
            "Escape" => 0x01,
            "Digit1" => 0x02,
            "Digit2" => 0x03,
            "Digit3" => 0x04,
            "Digit4" => 0x05,
            "Digit5" => 0x06,
            "Digit6" => 0x07,
            "Digit7" => 0x08,
            "Digit8" => 0x09,
            "Digit9" => 0x0A,
            "Digit0" => 0x0B,
            "Minus" => 0x0C,
            "Equal" => 0x0D,
            "Backspace" => 0x0E,
            "Tab" => 0x0F,
            "KeyQ" => 0x10,
            "KeyW" => 0x11,
            "KeyE" => 0x12,
            "KeyR" => 0x13,
            "KeyT" => 0x14,
            "KeyY" => 0x15,
            "KeyU" => 0x16,
            "KeyI" => 0x17,
            "KeyO" => 0x18,
            "KeyP" => 0x19,
            "BracketLeft" => 0x1A,
            "BracketRight" => 0x1B,
            "Enter" => 0x1C,
            "NumpadEnter" => 0x1C,
            "ControlLeft" => 0x1D,
            "ControlRight" => 0x1D,
            "KeyA" => 0x1E,
            "KeyS" => 0x1F,
            "KeyD" => 0x20,
            "KeyF" => 0x21,
            "KeyG" => 0x22,
            "KeyH" => 0x23,
            "KeyJ" => 0x24,
            "KeyK" => 0x25,
            "KeyL" => 0x26,
            "Semicolon" => 0x27,
            "Quote" => 0x28,
            "Backquote" => 0x29,
            "ShiftLeft" => 0x2A,
            "Backslash" => 0x2B,
            "KeyZ" => 0x2C,
            "KeyX" => 0x2D,
            "KeyC" => 0x2E,
            "KeyV" => 0x2F,
            "KeyB" => 0x30,
            "KeyN" => 0x31,
            "KeyM" => 0x32,
            "Comma" => 0x33,
            "Period" => 0x34,
            "Slash" => 0x35,
            "NumpadDivide" => 0x35,
            "ShiftRight" => 0x36,
            "NumpadMultiply" => 0x37,
            "AltLeft" => 0x38,
            "AltRight" => 0x38,
            "Space" => 0x39,
            "CapsLock" => 0x3A,
            "F1" => 0x3B,
            "F2" => 0x3C,
            "F3" => 0x3D,
            "F4" => 0x3E,
            "F5" => 0x3F,
            "F6" => 0x40,
            "F7" => 0x41,
            "F8" => 0x42,
            "F9" => 0x43,
            "F10" => 0x44,
            "Home" => 0x47,
            "Numpad7" => 0x47,
            "ArrowUp" => 0x48,
            "Numpad8" => 0x48,
            "PageUp" => 0x49,
            "Numpad9" => 0x49,
            "NumpadSubtract" => 0x4A,
            "ArrowLeft" => 0x4B,
            "Numpad4" => 0x4B,
            "Numpad5" => 0x4C,
            "ArrowRight" => 0x4D,
            "Numpad6" => 0x4D,
            "NumpadAdd" => 0x4E,
            "End" => 0x4F,
            "Numpad1" => 0x4F,
            "ArrowDown" => 0x50,
            "Numpad2" => 0x50,
            "PageDown" => 0x51,
            "Numpad3" => 0x51,
            "Insert" => 0x52,
            "Numpad0" => 0x52,
            "Delete" => 0x53,
            "NumpadDecimal" => 0x53,
            "IntlBackslash" => 0x56,
            "F11" => 0x57,
            "F12" => 0x58,
            "MetaLeft" => 0x5B,
            "MetaRight" => 0x5C,
            _ => return None,
        })
    }

    #[cfg(test)]
    mod tests {
        use super::to_keycode;

        #[test]
        fn maps_the_spot_checked_keys() {
            assert_eq!(to_keycode("KeyA"), Some(0x1E));
            assert_eq!(to_keycode("Enter"), Some(0x1C));
            assert_eq!(to_keycode("ArrowLeft"), Some(0x4B));
            assert_eq!(to_keycode("MetaLeft"), Some(0x5B));
            assert_eq!(to_keycode("Numpad1"), Some(0x4F));
        }

        #[test]
        fn unknown_code_maps_to_none() {
            assert_eq!(to_keycode("Unknown"), None);
        }
    }
}

#[cfg(not(any(target_os = "macos", target_os = "windows")))]
mod platform {
    /// No real `Injector` backend exists on this platform (see
    /// `crate::input::NoopInjector`), so there's nothing meaningful to map
    /// to -- every code is unmapped.
    pub(super) fn to_keycode(_code: &str) -> Option<u16> {
        None
    }

    #[cfg(test)]
    mod tests {
        use super::to_keycode;

        #[test]
        fn every_code_is_unmapped() {
            assert_eq!(to_keycode("KeyA"), None);
            assert_eq!(to_keycode("Enter"), None);
        }
    }
}
