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

/// One STUN/TURN server, as sent to a host or client so it can build its own
/// `RTCPeerConnection` configuration (see ARCHITECTURE.md §11 and
/// `server/src/ice.rs`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
#[ts(export)]
pub struct IceServer {
    pub urls: Vec<String>,
    pub username: Option<String>,
    pub credential: Option<String>,
}

/// Постоянные креды устройства (слайс 3.1): сервер выдаёт их при первой
/// регистрации, хост сохраняет у себя и предъявляет при каждой следующей,
/// получая тот же `host_id` вместо нового случайного.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
#[ts(export)]
pub struct DeviceCredentials {
    pub device_id: String,
    pub secret: String,
}

/// Одно устройство в списке владельца (слайс 3.1). `name` — как его
/// назвал сам хост (имя машины), `alias` — переименование владельцем,
/// `online`/`busy` — сиюминутное состояние из реестра соединений.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
#[ts(export)]
pub struct DeviceEntry {
    pub device_id: String,
    pub name: String,
    pub alias: Option<String>,
    pub online: bool,
    pub busy: bool,
    pub last_seen_at: i64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
#[serde(tag = "type", rename_all = "snake_case")]
#[ts(export)]
pub enum SignalMessage {
    /// First message from any client on connect.
    Hello { role: Role, version: String },
    /// host -> server. `device` carries the persistent device credentials
    /// (slice 3.1) issued by a previous `Registered`, if the host has any
    /// saved; `#[serde(default)]` so an older host that predates 3.1 (whose
    /// `HostRegister` has no `device` field) stays compatible with a newer
    /// server -- same reasoning as `PeerJoined.ice_servers` below.
    HostRegister {
        name: String,
        #[serde(default)]
        device: Option<DeviceCredentials>,
    },
    /// server -> host. `device` carries freshly issued credentials (slice
    /// 3.1) only on a host's very first registration; on a successful
    /// re-registration with already-known credentials it's `None` -- the
    /// host already has what it needs saved.
    Registered {
        host_id: String,
        pin: String,
        ice_servers: Vec<IceServer>,
        #[serde(default)]
        device: Option<DeviceCredentials>,
    },
    /// client -> server
    Join { pin: String },
    /// server -> client
    Joined {
        session_id: String,
        host_name: String,
        ice_servers: Vec<IceServer>,
    },
    /// server -> host. `ice_servers` are freshly minted for this session
    /// (same call as `Joined`'s), so a host that has been running for a
    /// while (2.6a: it reconnects and stays up) gets un-expired TURN creds
    /// per session instead of relying on the ones from its own `Registered`,
    /// which can be up to `RCDESK_TURN_TTL_SECS` old (2.6b). `#[serde(default)]`
    /// so a new host stays compatible with an older server that doesn't send
    /// this field: it falls back to an empty vector.
    PeerJoined {
        session_id: String,
        #[serde(default)]
        ice_servers: Vec<IceServer>,
    },
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
    /// client -> server, необязательное сообщение сразу после `Hello`:
    /// `token` — сохранённый браузером токен владельца, если он есть.
    ClientAuth {
        #[serde(default)]
        token: Option<String>,
    },
    /// server -> client, ответ на `ClientAuth`: актуальный токен владельца
    /// (тот же, если присланный опознан, иначе свежевыданный — его надо
    /// сохранить) и его текущий список устройств.
    Authenticated {
        token: String,
        devices: Vec<DeviceEntry>,
    },
    /// client -> server: перечитать список устройств.
    ListDevices,
    /// server -> client: текущий список устройств владельца.
    Devices { devices: Vec<DeviceEntry> },
    /// client -> server: подключиться к своему устройству без PIN.
    ConnectDevice { device_id: String },
    /// client -> server: переименовать своё устройство (`alias = None` —
    /// снять переименование). В ответ сервер шлёт обновлённый `Devices`.
    RenameDevice {
        device_id: String,
        alias: Option<String>,
    },
    /// client -> server: убрать устройство из своего списка. В ответ сервер
    /// шлёт обновлённый `Devices`.
    ForgetDevice { device_id: String },
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

    #[test]
    fn peer_joined_round_trips_through_json_with_ice_servers() {
        let peer_joined = SignalMessage::PeerJoined {
            session_id: "abc123".to_string(),
            ice_servers: vec![IceServer {
                urls: vec!["stun:stun.l.google.com:19302".to_string()],
                username: None,
                credential: None,
            }],
        };

        let json = serde_json::to_string(&peer_joined).expect("serialize");
        let round_tripped: SignalMessage = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(round_tripped, peer_joined);
    }

    #[test]
    fn peer_joined_without_ice_servers_field_deserializes_to_empty_vec() {
        let json = r#"{"type":"peer_joined","session_id":"abc123"}"#;

        let msg: SignalMessage = serde_json::from_str(json).expect("deserialize");
        assert_eq!(
            msg,
            SignalMessage::PeerJoined {
                session_id: "abc123".to_string(),
                ice_servers: Vec::new(),
            }
        );
    }

    #[test]
    fn host_register_without_device_field_deserializes_to_none() {
        let json = r#"{"type":"host_register","name":"My Mac"}"#;

        let msg: SignalMessage = serde_json::from_str(json).expect("deserialize");
        assert_eq!(
            msg,
            SignalMessage::HostRegister {
                name: "My Mac".to_string(),
                device: None,
            }
        );
    }

    #[test]
    fn client_auth_without_token_field_deserializes_to_none() {
        let json = r#"{"type":"client_auth"}"#;

        let msg: SignalMessage = serde_json::from_str(json).expect("deserialize");
        assert_eq!(msg, SignalMessage::ClientAuth { token: None });
    }

    #[test]
    fn authenticated_with_devices_round_trips_through_json() {
        let authenticated = SignalMessage::Authenticated {
            token: "tok123".to_string(),
            devices: vec![DeviceEntry {
                device_id: "dev123".to_string(),
                name: "My Mac".to_string(),
                alias: Some("Work Mac".to_string()),
                online: true,
                busy: false,
                last_seen_at: 1234,
            }],
        };

        let json = serde_json::to_string(&authenticated).expect("serialize");
        let round_tripped: SignalMessage = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(round_tripped, authenticated);
    }

    #[test]
    fn registered_with_device_round_trips_through_json() {
        let registered = SignalMessage::Registered {
            host_id: "host123".to_string(),
            pin: "123456".to_string(),
            ice_servers: Vec::new(),
            device: Some(DeviceCredentials {
                device_id: "dev123".to_string(),
                secret: "supersecret".to_string(),
            }),
        };

        let json = serde_json::to_string(&registered).expect("serialize");
        let round_tripped: SignalMessage = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(round_tripped, registered);
    }
}
