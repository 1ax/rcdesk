//! Messages sent by the web client over the `input` and `pointer` data
//! channels (see ARCHITECTURE.md §5 and §7) to drive the host's mouse and
//! keyboard.
//!
//! `PointerMove` travels on the unreliable/unordered `pointer` channel (a
//! stale position is worthless once a newer one exists); everything else
//! travels on the reliable, ordered `input` channel.

use serde::{Deserialize, Serialize};
use ts_rs::TS;

/// A mouse button, named the way `enigo::Button` names them (see
/// `docs/host-libs-api-notes.md`), not by DOM `MouseEvent.button` index --
/// the client maps 0/1/2/3/4 to these before sending.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, TS)]
#[serde(rename_all = "snake_case")]
#[ts(export)]
pub enum PointerButton {
    Left,
    Middle,
    Right,
    Back,
    Forward,
}

/// A single input event from the client, JSON-encoded on the wire (see
/// ARCHITECTURE.md §5).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
#[serde(tag = "type", rename_all = "snake_case")]
#[ts(export)]
pub enum InputMessage {
    /// channel `pointer`. `x`/`y` are normalized `[0,1]` relative to the
    /// captured screen (see ARCHITECTURE.md §7); the host scales them to its
    /// own pixel coordinates.
    PointerMove { x: f64, y: f64 },
    /// channel `input`. `x`/`y` carry the same normalized position as
    /// `PointerMove` so the host can place the cursor before clicking even
    /// if the last `pointer` message hasn't arrived yet (the two channels
    /// are independent and unordered relative to each other).
    PointerButton {
        button: PointerButton,
        pressed: bool,
        x: f64,
        y: f64,
    },
    /// channel `input`. `dx`/`dy` are in "lines" (a positive `dy` scrolls
    /// down); `x`/`y` are the normalized pointer position, same reasoning as
    /// `PointerButton`.
    Wheel { dx: f64, dy: f64, x: f64, y: f64 },
    /// channel `input`. `code` is the W3C `KeyboardEvent.code` value
    /// ("KeyA", "ShiftLeft", "ArrowUp", ...) -- the physical key, not the
    /// character it produces (see ARCHITECTURE.md §7).
    Key { code: String, pressed: bool },
    /// channel `input`. The client lost input focus (window blur or the tab
    /// went hidden): release every key/button the host currently thinks is
    /// held, since no matching "up" event will ever arrive for them.
    ReleaseAll,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn key_round_trips_through_json_with_exact_shape() {
        let key = InputMessage::Key {
            code: "KeyA".to_string(),
            pressed: true,
        };

        let json = serde_json::to_string(&key).expect("serialize");
        assert_eq!(json, r#"{"type":"key","code":"KeyA","pressed":true}"#);

        let round_tripped: InputMessage = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(round_tripped, key);
    }

    #[test]
    fn pointer_button_round_trips_through_json_with_exact_shape() {
        let msg = InputMessage::PointerButton {
            button: PointerButton::Right,
            pressed: false,
            x: 0.25,
            y: 0.75,
        };

        let json = serde_json::to_string(&msg).expect("serialize");
        assert_eq!(
            json,
            r#"{"type":"pointer_button","button":"right","pressed":false,"x":0.25,"y":0.75}"#
        );

        let round_tripped: InputMessage = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(round_tripped, msg);
    }

    #[test]
    fn release_all_round_trips_through_json() {
        let msg = InputMessage::ReleaseAll;

        let json = serde_json::to_string(&msg).expect("serialize");
        assert_eq!(json, r#"{"type":"release_all"}"#);

        let round_tripped: InputMessage = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(round_tripped, msg);
    }
}
