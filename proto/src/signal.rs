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
    /// `session_id` (slice 3.2a/D35) is the id of this host's still-live P2P
    /// session, if it has one -- set when the host reconnects to signaling
    /// (2.6a's backoff, a server restart) while a session survives the gap
    /// (slice 3.5a: the session itself is peer-to-peer and outlives the
    /// signaling socket). Lets the server re-attach that session to the new
    /// connection, or at least mark the device busy, instead of treating the
    /// host as freshly idle. `#[serde(default)]` so an older host that
    /// predates 3.2a (whose `HostRegister` has no `session_id` field) stays
    /// compatible with a newer server.
    HostRegister {
        name: String,
        #[serde(default)]
        device: Option<DeviceCredentials>,
        #[serde(default)]
        session_id: Option<String>,
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
    /// server -> client. `device_id` (slice 3.5b) is the persistent id of
    /// the device the client just joined (see `DeviceCredentials`/`3.1`) --
    /// `Some` whenever the host is registered as a persistent device (true
    /// for every host since 3.1), `None` only for compatibility with a
    /// hypothetical host that has no device id at all. The client keeps it
    /// to reconnect to the same device after a lost session (slice 3.5b)
    /// without asking the owner for a PIN again. `#[serde(default)]` so an
    /// older server that predates 3.5b (whose `Joined` has no `device_id`
    /// field) stays compatible with a newer client.
    Joined {
        session_id: String,
        host_name: String,
        ice_servers: Vec<IceServer>,
        #[serde(default)]
        device_id: Option<String>,
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
    /// host -> client (slice 3.2c), sent right after `PeerJoined` when the
    /// host has an access password set (`access::AccessStore::load`
    /// returns `Some`): the client must complete an OPAQUE login (`PakeStart`
    /// / `PakeResponse` / `PakeFinish` below) before the host will offer a
    /// session. The server only relays this, same as `Offer`/`Ice`.
    AuthRequired { session_id: String },
    /// client -> host (slice 3.2c), forwarded by the server. `payload` is
    /// the client's serialized OPAQUE `CredentialRequest`, base64
    /// (URL-safe, no padding -- see `host::access::AccessRecordFile`'s doc
    /// comment for why that alphabet). Opaque to the server and to this
    /// message's own type: it never decodes it, only relays it.
    PakeStart { session_id: String, payload: String },
    /// host -> client (slice 3.2c), forwarded by the server: the host's
    /// serialized OPAQUE `CredentialResponse` to the client's `PakeStart`,
    /// base64 (URL-safe, no padding), in reply to `PakeStart`.
    PakeResponse { session_id: String, payload: String },
    /// client -> host (slice 3.2c), forwarded by the server: the client's
    /// serialized OPAQUE `CredentialFinalization`, base64 (URL-safe, no
    /// padding), completing the login started by `PakeStart`.
    PakeFinish { session_id: String, payload: String },
    /// host -> client (slice 3.2c), forwarded by the server: the login
    /// attempt was rejected. `retry_after_secs` is `None` when this
    /// particular attempt was simply wrong (the client may retry right
    /// away with a new `PakeStart`) and `Some(n)` when the host has locked
    /// out further attempts for `n` seconds after too many failures in a
    /// row. `#[serde(default)]` so a client that only checks for the
    /// field's presence on decode still works if a future host omits it.
    AuthFailed {
        session_id: String,
        #[serde(default)]
        retry_after_secs: Option<u32>,
    },
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
                session_id: None,
            }
        );
    }

    #[test]
    fn host_register_with_session_id_round_trips_through_json() {
        let host_register = SignalMessage::HostRegister {
            name: "My Mac".to_string(),
            device: None,
            session_id: Some("sess-live".to_string()),
        };

        let json = serde_json::to_string(&host_register).expect("serialize");
        let round_tripped: SignalMessage = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(round_tripped, host_register);
    }

    #[test]
    fn host_register_without_session_id_field_deserializes_to_none() {
        let json = r#"{"type":"host_register","name":"My Mac","device":null}"#;

        let msg: SignalMessage = serde_json::from_str(json).expect("deserialize");
        assert_eq!(
            msg,
            SignalMessage::HostRegister {
                name: "My Mac".to_string(),
                device: None,
                session_id: None,
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
    fn joined_with_device_id_round_trips_through_json() {
        let joined = SignalMessage::Joined {
            session_id: "sess-1".to_string(),
            host_name: "My Mac".to_string(),
            ice_servers: Vec::new(),
            device_id: Some("dev123".to_string()),
        };

        let json = serde_json::to_string(&joined).expect("serialize");
        let round_tripped: SignalMessage = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(round_tripped, joined);
    }

    #[test]
    fn joined_without_device_id_field_deserializes_to_none() {
        let json =
            r#"{"type":"joined","session_id":"sess-1","host_name":"My Mac","ice_servers":[]}"#;

        let msg: SignalMessage = serde_json::from_str(json).expect("deserialize");
        assert_eq!(
            msg,
            SignalMessage::Joined {
                session_id: "sess-1".to_string(),
                host_name: "My Mac".to_string(),
                ice_servers: Vec::new(),
                device_id: None,
            }
        );
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

    #[test]
    fn pake_start_round_trips_through_json() {
        let pake_start = SignalMessage::PakeStart {
            session_id: "sess-1".to_string(),
            payload: "YWJjMTIz".to_string(),
        };

        let json = serde_json::to_string(&pake_start).expect("serialize");
        let round_tripped: SignalMessage = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(round_tripped, pake_start);
    }

    #[test]
    fn auth_failed_with_retry_after_secs_round_trips_through_json() {
        let auth_failed = SignalMessage::AuthFailed {
            session_id: "sess-1".to_string(),
            retry_after_secs: Some(30),
        };

        let json = serde_json::to_string(&auth_failed).expect("serialize");
        let round_tripped: SignalMessage = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(round_tripped, auth_failed);
    }

    #[test]
    fn auth_failed_without_retry_after_secs_field_deserializes_to_none() {
        let json = r#"{"type":"auth_failed","session_id":"sess-1"}"#;

        let msg: SignalMessage = serde_json::from_str(json).expect("deserialize");
        assert_eq!(
            msg,
            SignalMessage::AuthFailed {
                session_id: "sess-1".to_string(),
                retry_after_secs: None,
            }
        );
    }
}
