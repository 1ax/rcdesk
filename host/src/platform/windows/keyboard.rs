//! Sends key presses/releases via `SendInput`, bypassing `enigo`.
//!
//! `enigo` 0.6.1's Windows `raw()` translates the scancode to a virtual key
//! via `MapVirtualKeyW(scan, MAPVK_VSC_TO_VK_EX)` and only sets
//! `KEYEVENTF_EXTENDEDKEY` for virtual keys in its own incomplete table (see
//! `is_extended_key` in `enigo`'s `win_impl.rs`); it does not accept an
//! `0xE0` prefix on the scancode itself. `input::keymap`'s Windows table
//! encodes the extended-key prefix directly in the scancode value
//! (`0xE0xx`), so this module sends `SendInput` itself instead of going
//! through `enigo::raw`.
//!
//! Compiles only on `windows-latest` in CI: there is no Windows machine
//! available to this executor run, so every signature/type below comes
//! from reading the `windows` 0.61.3 source fetched into the local
//! registry cache (`cargo fetch --target x86_64-pc-windows-msvc`), not from
//! memory -- see executor rule 3 (the same approach `host/src/cursor/windows.rs`
//! documents and uses).

use std::mem::size_of;

use windows::Win32::UI::Input::KeyboardAndMouse::{
    MapVirtualKeyW, SendInput, INPUT, INPUT_0, INPUT_KEYBOARD, KEYBDINPUT, KEYEVENTF_EXTENDEDKEY,
    KEYEVENTF_KEYUP, KEYEVENTF_SCANCODE, KEYEVENTF_UNICODE, MAPVK_VSC_TO_VK_EX, VIRTUAL_KEY,
};

use crate::input::keymap::scan_parts;

/// Sends a key press/release by PS/2 Scan Code Set 1 scancode (`0xE0xx` =
/// extended, see `input::keymap::scan_parts`).
pub fn send_scancode(scan: u16, pressed: bool) -> Result<(), windows::core::Error> {
    let (low, extended) = scan_parts(scan);

    // Safety: `MapVirtualKeyW` has no preconditions beyond passing a valid
    // map type, which `MAPVK_VSC_TO_VK_EX` is; a scancode with no
    // corresponding virtual key returns 0, which is not an error here --
    // `wVk` is ignored by `SendInput` when `KEYEVENTF_SCANCODE` is set and
    // is filled in only for the benefit of applications that inspect it.
    let vk = unsafe { MapVirtualKeyW(u32::from(scan), MAPVK_VSC_TO_VK_EX) };

    let mut flags = KEYEVENTF_SCANCODE;
    if extended {
        flags |= KEYEVENTF_EXTENDEDKEY;
    }
    if !pressed {
        flags |= KEYEVENTF_KEYUP;
    }

    let input = INPUT {
        r#type: INPUT_KEYBOARD,
        Anonymous: INPUT_0 {
            ki: KEYBDINPUT {
                wVk: VIRTUAL_KEY(vk as u16),
                wScan: low,
                dwFlags: flags,
                time: 0,
                dwExtraInfo: 0,
            },
        },
    };

    // Safety: `input` is a single, fully-initialized `INPUT` of the
    // `INPUT_KEYBOARD` variant; `SendInput` reads it and does not retain the
    // pointer past the call.
    let sent = unsafe { SendInput(&[input], size_of::<INPUT>() as i32) };
    if sent != 1 {
        return Err(windows::core::Error::from_win32());
    }
    Ok(())
}

/// Types `text` as Unicode input (`KEYEVENTF_UNICODE`), bypassing the
/// keyboard layout. Each UTF-16 code unit becomes a down + up pair
/// (surrogate pairs go as two consecutive units, the standard path). All
/// events are sent in ONE `SendInput` call so a focus change cannot split
/// the text. See `proto::input::InputMessage::Text`.
pub fn send_unicode(text: &str) -> Result<(), windows::core::Error> {
    let mut inputs: Vec<INPUT> = Vec::new();
    for unit in text.encode_utf16() {
        for flags in [KEYEVENTF_UNICODE, KEYEVENTF_UNICODE | KEYEVENTF_KEYUP] {
            inputs.push(INPUT {
                r#type: INPUT_KEYBOARD,
                Anonymous: INPUT_0 {
                    ki: KEYBDINPUT {
                        wVk: VIRTUAL_KEY(0),
                        wScan: unit,
                        dwFlags: flags,
                        time: 0,
                        dwExtraInfo: 0,
                    },
                },
            });
        }
    }
    if inputs.is_empty() {
        return Ok(());
    }

    // Safety: `inputs` is a slice of fully-initialized `INPUT`s of the
    // `INPUT_KEYBOARD` variant; `SendInput` reads them and does not retain
    // the pointer past the call.
    let sent = unsafe { SendInput(&inputs, size_of::<INPUT>() as i32) };
    if (sent as usize) < inputs.len() {
        return Err(windows::core::Error::from_win32());
    }
    Ok(())
}
