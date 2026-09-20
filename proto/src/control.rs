//! Messages sent over the `control` data channel (see ARCHITECTURE.md §5):
//! host -> client cursor shape/visibility, plus an application-level
//! ping/pong the client uses to measure its own end-to-end RTT (distinct
//! from the WebRTC-level RTT already shown in the stats overlay, see
//! ARCHITECTURE.md §4.2).

use serde::{Deserialize, Serialize};
use ts_rs::TS;

/// A single capturable display, as shown in the client's display picker
/// (slice 2.4). `width`/`height` are the display's size in its own
/// OS-global units (points on macOS, physical pixels on Windows) -- for
/// labelling in the UI only, not a frame/pixel size.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
#[ts(export)]
pub struct DisplayEntry {
    pub id: u32,
    pub title: String,
    pub width: u32,
    pub height: u32,
    pub primary: bool,
}

/// A single `control`-channel message, JSON-encoded on the wire (see
/// ARCHITECTURE.md §5).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
#[serde(tag = "type", rename_all = "snake_case")]
#[ts(export)]
pub enum ControlMessage {
    /// host -> client. The current shape of the system cursor. `rgba` is
    /// base64 (no line breaks) of raw RGBA bytes, `width * height * 4` of
    /// them, straight (not premultiplied) alpha -- not required, since the
    /// client only ever hands the bytes to the browser's own PNG encoder,
    /// which doesn't care either way. `scale` is how many times larger the
    /// image is than its logical size (1, or 2 on a Retina display);
    /// `hotspot_x`/`hotspot_y` are in logical points, matching `scale`.
    CursorShape {
        width: u32,
        height: u32,
        hotspot_x: f64,
        hotspot_y: f64,
        scale: f64,
        rgba: String,
    },
    /// host -> client: the system cursor is hidden (e.g. while typing).
    CursorHidden,
    /// client -> host; the host echoes back `Pong` with the same `ts` so the
    /// client can compute an application-level round trip (see `app.ts`'s
    /// overlay `app N ms`).
    Ping { ts: f64 },
    /// host -> client, in reply to `Ping`.
    Pong { ts: f64 },
    /// host -> client: the adaptation controller changed the encoder's rate target
    /// (slice 2.3). Shown in the client's stats overlay; informational only.
    Quality {
        bitrate_kbps: u32,
        fps: u32,
        reason: String,
    },
    /// host -> client: the list of displays the host can capture, and the id
    /// of the one currently being streamed (slice 2.4). Sent when the
    /// `control` channel opens and again after every display switch.
    Displays {
        displays: Vec<DisplayEntry>,
        current: u32,
    },
    /// client -> host: switch the streamed display to `id`.
    SelectDisplay { id: u32 },
    /// host -> client: whether the host can currently inject mouse/keyboard
    /// input on this session (slice 2.5a, closing debt D26). Sent once,
    /// right after `Displays`, when the `control` channel opens -- session
    /// start is the only point input availability is decided, so unlike
    /// `Displays` this never needs to be re-sent later. `available: false`
    /// means the session is view-only (e.g. the host is missing the macOS
    /// Accessibility permission, or was started with `serve --no-input`);
    /// `reason` is a human-readable explanation for display, or `None` when
    /// there's nothing to add (`available: true`).
    InputStatus {
        available: bool,
        reason: Option<String>,
    },
    /// host -> client: the Windows foreground window belongs to a process
    /// running at a higher integrity level than the host agent's own
    /// (slice 2.6e, e.g. Task Manager opened "as administrator") -- UIPI
    /// silently drops `SendInput` events aimed at it, so clicks/keys the
    /// client sends while this window is focused have no effect. **Not**
    /// the same thing as `InputStatus`: that one means there is no input
    /// injector at all for the whole session, and the client detaches
    /// input entirely. `InputBlocked` is a transient, window-by-window
    /// condition -- the client keeps sending input as usual (so the user
    /// can still click away from the blocking window) and only shows a
    /// warning banner. Sent whenever the host's foreground-window watcher
    /// (polled once a second, Windows only) observes a change; a session
    /// that never sends this is never blocked. `reason` is set to a
    /// human-readable explanation when `blocked` is `true`, `None` when
    /// `blocked` is `false`. The full-control mode (phase 4.1, host running
    /// as a service) doesn't have this limitation.
    InputBlocked {
        blocked: bool,
        reason: Option<String>,
    },
    /// host -> client: the host's clipboard text changed (slice 2.5b),
    /// polled every 250ms via an OS change counter (`NSPasteboard`
    /// `changeCount` / `GetClipboardSequenceNumber`, see `host::clipboard`).
    /// `text` is never longer than `host::clipboard::MAX_CLIPBOARD_BYTES`
    /// (200_000 UTF-8 bytes) -- anything bigger is dropped on the host with
    /// a `warn` (length only, never content). Sent on the `control` channel,
    /// never on `input`, since it isn't an input event; contrast
    /// `proto::input::InputMessage::ClipboardText`, the client -> host
    /// direction, which *does* need to travel on `input` (see that variant's
    /// doc comment for why).
    ClipboardText { text: String },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cursor_shape_round_trips_through_json_with_exact_shape() {
        let msg = ControlMessage::CursorShape {
            width: 16,
            height: 16,
            hotspot_x: 1.5,
            hotspot_y: 2.0,
            scale: 2.0,
            rgba: "QUJD".to_string(),
        };

        let json = serde_json::to_string(&msg).expect("serialize");
        assert_eq!(
            json,
            r#"{"type":"cursor_shape","width":16,"height":16,"hotspot_x":1.5,"hotspot_y":2.0,"scale":2.0,"rgba":"QUJD"}"#
        );

        let round_tripped: ControlMessage = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(round_tripped, msg);
    }

    #[test]
    fn cursor_hidden_round_trips_through_json() {
        let msg = ControlMessage::CursorHidden;

        let json = serde_json::to_string(&msg).expect("serialize");
        assert_eq!(json, r#"{"type":"cursor_hidden"}"#);

        let round_tripped: ControlMessage = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(round_tripped, msg);
    }

    #[test]
    fn ping_round_trips_through_json_with_exact_shape() {
        let msg = ControlMessage::Ping { ts: 1234.5 };

        let json = serde_json::to_string(&msg).expect("serialize");
        assert_eq!(json, r#"{"type":"ping","ts":1234.5}"#);

        let round_tripped: ControlMessage = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(round_tripped, msg);
    }

    #[test]
    fn pong_round_trips_through_json() {
        let msg = ControlMessage::Pong { ts: 42.0 };

        let json = serde_json::to_string(&msg).expect("serialize");
        assert_eq!(json, r#"{"type":"pong","ts":42.0}"#);

        let round_tripped: ControlMessage = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(round_tripped, msg);
    }

    #[test]
    fn quality_round_trips_through_json_with_exact_shape() {
        let msg = ControlMessage::Quality {
            bitrate_kbps: 3200,
            fps: 20,
            reason: "remb".to_string(),
        };

        let json = serde_json::to_string(&msg).expect("serialize");
        assert_eq!(
            json,
            r#"{"type":"quality","bitrate_kbps":3200,"fps":20,"reason":"remb"}"#
        );

        let round_tripped: ControlMessage = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(round_tripped, msg);
    }

    #[test]
    fn displays_round_trips_through_json_with_exact_shape() {
        let msg = ControlMessage::Displays {
            displays: vec![DisplayEntry {
                id: 1,
                title: "Built-in".to_string(),
                width: 1728,
                height: 1117,
                primary: true,
            }],
            current: 1,
        };

        let json = serde_json::to_string(&msg).expect("serialize");
        assert_eq!(
            json,
            r#"{"type":"displays","displays":[{"id":1,"title":"Built-in","width":1728,"height":1117,"primary":true}],"current":1}"#
        );

        let round_tripped: ControlMessage = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(round_tripped, msg);
    }

    #[test]
    fn select_display_round_trips_through_json_with_exact_shape() {
        let msg = ControlMessage::SelectDisplay { id: 2 };

        let json = serde_json::to_string(&msg).expect("serialize");
        assert_eq!(json, r#"{"type":"select_display","id":2}"#);

        let round_tripped: ControlMessage = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(round_tripped, msg);
    }

    #[test]
    fn input_status_unavailable_round_trips_through_json_with_exact_shape() {
        let msg = ControlMessage::InputStatus {
            available: false,
            reason: Some("no Accessibility permission".to_string()),
        };

        let json = serde_json::to_string(&msg).expect("serialize");
        assert_eq!(
            json,
            r#"{"type":"input_status","available":false,"reason":"no Accessibility permission"}"#
        );

        let round_tripped: ControlMessage = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(round_tripped, msg);
    }

    #[test]
    fn input_status_available_serializes_reason_as_json_null() {
        let msg = ControlMessage::InputStatus {
            available: true,
            reason: None,
        };

        let json = serde_json::to_string(&msg).expect("serialize");
        assert_eq!(
            json,
            r#"{"type":"input_status","available":true,"reason":null}"#
        );

        let round_tripped: ControlMessage = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(round_tripped, msg);
    }

    #[test]
    fn input_blocked_true_round_trips_through_json_with_exact_shape() {
        let msg = ControlMessage::InputBlocked {
            blocked: true,
            reason: Some(
                "Foreground window runs with administrator rights; input is blocked by Windows (UIPI)"
                    .to_string(),
            ),
        };

        let json = serde_json::to_string(&msg).expect("serialize");
        assert_eq!(
            json,
            r#"{"type":"input_blocked","blocked":true,"reason":"Foreground window runs with administrator rights; input is blocked by Windows (UIPI)"}"#
        );

        let round_tripped: ControlMessage = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(round_tripped, msg);
    }

    #[test]
    fn input_blocked_false_serializes_reason_as_json_null() {
        let msg = ControlMessage::InputBlocked {
            blocked: false,
            reason: None,
        };

        let json = serde_json::to_string(&msg).expect("serialize");
        assert_eq!(
            json,
            r#"{"type":"input_blocked","blocked":false,"reason":null}"#
        );

        let round_tripped: ControlMessage = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(round_tripped, msg);
    }

    #[test]
    fn clipboard_text_round_trips_through_json_with_exact_shape() {
        let msg = ControlMessage::ClipboardText {
            text: "hello from host".to_string(),
        };

        let json = serde_json::to_string(&msg).expect("serialize");
        assert_eq!(
            json,
            r#"{"type":"clipboard_text","text":"hello from host"}"#
        );

        let round_tripped: ControlMessage = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(round_tripped, msg);
    }
}
