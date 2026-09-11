//! Messages sent over the `control` data channel (see ARCHITECTURE.md §5):
//! host -> client cursor shape/visibility, plus an application-level
//! ping/pong the client uses to measure its own end-to-end RTT (distinct
//! from the WebRTC-level RTT already shown in the stats overlay, see
//! ARCHITECTURE.md §4.2).

use serde::{Deserialize, Serialize};
use ts_rs::TS;

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
}
