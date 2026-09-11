use serde::{Deserialize, Serialize};
use ts_rs::TS;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
#[serde(rename_all = "snake_case")]
#[ts(export)]
pub enum Role {
    Host,
    Client,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
#[ts(export)]
pub struct IceCandidate {
    pub candidate: String,
    pub sdp_mid: Option<String>,
    pub sdp_mline_index: Option<u16>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
#[serde(tag = "type", rename_all = "snake_case")]
#[ts(export)]
pub enum SignalMessage {
    /// First message from any client on connect.
    Hello { role: Role, version: String },
    /// host -> server
    HostRegister { name: String },
    /// server -> host
    Registered { host_id: String, pin: String },
    /// client -> server
    Join { pin: String },
    /// server -> client
    Joined {
        session_id: String,
        host_name: String,
    },
    /// server -> host
    PeerJoined { session_id: String },
    /// forwarded by server to the other side of the session
    Offer { session_id: String, sdp: String },
    /// forwarded by server to the other side of the session
    Answer { session_id: String, sdp: String },
    /// forwarded by server to the other side of the session
    Ice {
        session_id: String,
        candidate: IceCandidate,
    },
    /// either side; server forwards to the other and closes the session
    Bye { session_id: String },
    /// server -> either side
    Error { message: String },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hello_round_trips_through_json() {
        let hello = SignalMessage::Hello {
            role: Role::Host,
            version: "0.1.0".to_string(),
        };

        let json = serde_json::to_string(&hello).expect("serialize");
        assert_eq!(json, r#"{"type":"hello","role":"host","version":"0.1.0"}"#);

        let round_tripped: SignalMessage = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(round_tripped, hello);
    }

    #[test]
    fn ice_round_trips_through_json_with_none_sdp_mid() {
        let ice = SignalMessage::Ice {
            session_id: "abc123".to_string(),
            candidate: IceCandidate {
                candidate: "candidate:1 1 UDP 2130706431 10.0.0.1 12345 typ host".to_string(),
                sdp_mid: None,
                sdp_mline_index: Some(0),
            },
        };

        let json = serde_json::to_string(&ice).expect("serialize");
        assert_eq!(
            json,
            r#"{"type":"ice","session_id":"abc123","candidate":{"candidate":"candidate:1 1 UDP 2130706431 10.0.0.1 12345 typ host","sdp_mid":null,"sdp_mline_index":0}}"#
        );

        let round_tripped: SignalMessage = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(round_tripped, ice);
    }
}
