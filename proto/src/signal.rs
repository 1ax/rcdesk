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

/// Чего клиент хочет от `EmailCodeConfirm` (сервер сверяет это с наличием
/// аккаунта с такой почтой).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
#[serde(rename_all = "snake_case")]
#[ts(export)]
pub enum EmailCodePurpose {
    /// Регистрация: почта должна быть свободна.
    Register,
    /// Сброс пароля: аккаунт с такой почтой должен существовать.
    Reset,
}

/// Причина, по которой хост завершил сеанс `Bye` (слайс 4.6b).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
#[serde(rename_all = "snake_case")]
#[ts(export)]
pub enum ByeReason {
    /// Экран хоста недоступен: дисплей спит или отключён, захватывать нечего.
    DisplayUnavailable,
    /// Видеоконвейер хоста (захват экрана или кодер) остановился с ошибкой; подробности в логе хоста.
    VideoFailed,
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

/// Один браузер владельца в списке (слайс 3.7a). `id` — идентификатор
/// токена (не сам токен и не его хеш), `current` — токен этого соединения.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
#[ts(export)]
pub struct BrowserEntry {
    pub id: String,
    pub user_agent: String,
    pub created_at: i64,
    pub last_used_at: i64,
    pub current: bool,
}

/// Один passkey владельца в списке (слайс 3.7b). `id` — идентификатор
/// записи на сервере (не credential id и не данные ключа).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
#[ts(export)]
pub struct PasskeyEntry {
    pub id: String,
    pub user_agent: String,
    pub created_at: i64,
    pub last_used_at: i64,
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
        /// D44: the host can accept `PinChanged`, so the server may rotate
        /// its PIN (after a `Join` and by TTL). `#[serde(default)]`: an older
        /// host sends nothing (= `false`) and keeps a static PIN.
        #[serde(default)]
        pin_rotation: bool,
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
    /// server -> host (D44): the host has a new PIN, issued after a
    /// successful `Join` or by TTL. Only sent to hosts that registered with
    /// `pin_rotation: true`.
    PinChanged { pin: String },
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
    /// forwarded by server to the other side of the session. `auth` (slice
    /// 3.2e) is the host's proof that this offer's DTLS fingerprint is
    /// bound to the OPAQUE session key from login -- `HMAC-SHA256(session_key,
    /// "rcdesk-dtls-v1|offer|" + fingerprint)`, base64 (URL-safe, no
    /// padding). `Some` only when the host has an access password set (a
    /// session key exists); `None` otherwise, same as before this slice.
    /// `#[serde(default)]` so an older host/server that predates 3.2e (whose
    /// `Offer` has no `auth` field) stays compatible with a newer peer.
    Offer {
        session_id: String,
        sdp: String,
        #[serde(default)]
        auth: Option<String>,
    },
    /// forwarded by server to the other side of the session. `auth` (slice
    /// 3.2e) is the client's matching proof for its answer's DTLS
    /// fingerprint, same construction as `Offer.auth` but with role
    /// `"answer"`. `#[serde(default)]` for the same compatibility reason as
    /// `Offer.auth`.
    Answer {
        session_id: String,
        sdp: String,
        #[serde(default)]
        auth: Option<String>,
    },
    /// forwarded by server to the other side of the session
    Ice {
        session_id: String,
        candidate: IceCandidate,
    },
    /// either side; server forwards to the other and closes the session.
    /// `reason` (slice 4.6b) tells the peer why the host ended the session;
    /// `None` is the plain end and is omitted from the JSON, so older sides
    /// without the field keep working in both directions.
    Bye {
        session_id: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        reason: Option<ByeReason>,
    },
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
        /// server -> client (D47): сервер — LAN-хаб: аккаунтов, passkey и
        /// почты нет, клиент прячет их UI. `#[serde(default)]`: старый сервер
        /// поле не шлёт -> `false`.
        #[serde(default)]
        lan_hub: bool,
    },
    /// client -> server: перечитать список устройств.
    ListDevices,
    /// server -> client: текущий список устройств владельца.
    Devices { devices: Vec<DeviceEntry> },
    /// client -> server (слайс 3.7a): перечитать список браузеров владельца.
    ListBrowsers,
    /// server -> client: браузеры (токены) владельца, недавно
    /// использованные первыми.
    Browsers { browsers: Vec<BrowserEntry> },
    /// client -> server: отозвать токен браузера `id` этого владельца. В
    /// ответ сервер шлёт обновлённый `Browsers`; если `id` — токен самого
    /// соединения, это то же, что `SignOut`.
    RevokeBrowser { id: String },
    /// client -> server: выйти — удалить токен этого соединения; сервер
    /// забывает владельца и отвечает `SignedOut`.
    SignOut,
    /// server -> client: ответ на `SignOut`.
    SignedOut,
    /// client -> server (слайс 3.7b): перечитать список passkey владельца.
    ListPasskeys,
    /// server -> client: passkey владельца, недавно использованные первыми.
    Passkeys { passkeys: Vec<PasskeyEntry> },
    /// client -> server: начать регистрацию passkey для текущего владельца.
    /// Сервер отвечает `PasskeyRegisterOptions`.
    PasskeyRegisterStart,
    /// server -> client: параметры `navigator.credentials.create()`.
    /// `options` — JSON-строка (`CreationChallengeResponse` webauthn-rs) как
    /// есть; proto её содержимого не знает.
    PasskeyRegisterOptions { options: String },
    /// client -> server: ответ аутентификатора. `credential` — JSON-строка
    /// (`RegisterPublicKeyCredential` webauthn-rs) как есть. В ответ сервер
    /// шлёт обновлённый `Passkeys`.
    PasskeyRegisterFinish { credential: String },
    /// client -> server: начать вход по passkey без логина (владелец не
    /// нужен). Сервер отвечает `PasskeyLoginOptions`.
    PasskeyLoginStart,
    /// server -> client: параметры `navigator.credentials.get()`. `options`
    /// — JSON-строка (`RequestChallengeResponse` webauthn-rs) как есть.
    PasskeyLoginOptions { options: String },
    /// client -> server: ответ аутентификатора. `credential` — JSON-строка
    /// (`PublicKeyCredential` webauthn-rs) как есть. В ответ сервер шлёт
    /// `Authenticated` с новым токеном владельца аккаунта.
    PasskeyLoginFinish { credential: String },
    /// client -> server: удалить passkey `id` этого владельца. В ответ
    /// сервер шлёт обновлённый `Passkeys`.
    DeletePasskey { id: String },
    /// client -> server (слайс 3.8a): прислать 6-значный код на почту.
    /// Сервер отвечает `EmailCodeSent` независимо от того, есть аккаунт с
    /// этой почтой или нет.
    EmailCodeRequest { email: String },
    /// server -> client: код отправлен (или запрос принят) -- ответ на
    /// `EmailCodeRequest`.
    EmailCodeSent,
    /// client -> server: код из письма и новый пароль. Если почта уже
    /// принадлежит аккаунту, это сброс пароля и вход в аккаунт; иначе --
    /// регистрация: почта и пароль привязываются к ТЕКУЩЕМУ владельцу
    /// соединения. `purpose` фиксирует намерение клиента: `Reset` без аккаунта
    /// -- ошибка `no such account`, `Register` при занятой почте -- `email
    /// taken` (ничего не меняется). `None` (поле отсутствует) -- прежнее
    /// поведение по наличию аккаунта, только для вкладки старой версии.
    /// Ответ: `Authenticated`, затем `Account`.
    EmailCodeConfirm {
        email: String,
        code: String,
        password: String,
        #[serde(default)]
        purpose: Option<EmailCodePurpose>,
    },
    /// client -> server: вход по почте и паролю. Ответ: `Authenticated`,
    /// затем `Account`.
    PasswordLogin { email: String, password: String },
    /// client -> server: сменить почту аккаунта текущего владельца на
    /// `email`. `code` -- код, присланный на НОВЫЙ адрес обычным
    /// `EmailCodeRequest`; `password` -- ТЕКУЩИЙ пароль аккаунта. Ответ при
    /// успехе: `Account` с новой почтой (сессии не трогаются).
    EmailChange {
        email: String,
        code: String,
        password: String,
    },
    /// client -> server: перечитать аккаунт владельца (нужен владелец).
    /// Сервер отвечает `Account`.
    GetAccount,
    /// server -> client: почта аккаунта владельца; `None` -- почты нет.
    Account { email: Option<String> },
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
    /// host -> server (журнал сеансов): кумулятивная статистика трафика
    /// сеанса `session_id` с его начала (не дельты). `relay` — выбранная
    /// ICE-пара идёт через TURN-релей хотя бы с одной стороны;
    /// `final_report` — последний отчёт, сеанс завершён. Сервер ничего не
    /// отвечает. `#[serde(default)]` на флагах: отчёт без них — обычный,
    /// не релейный.
    SessionStats {
        session_id: String,
        #[ts(type = "number")]
        bytes_sent: u64,
        #[ts(type = "number")]
        bytes_received: u64,
        #[serde(default)]
        relay: bool,
        #[serde(default)]
        final_report: bool,
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
    fn session_stats_round_trips_and_has_snake_case_tag() {
        let stats = SignalMessage::SessionStats {
            session_id: "s1".to_string(),
            bytes_sent: 10,
            bytes_received: 20,
            relay: true,
            final_report: false,
        };
        let json = serde_json::to_string(&stats).expect("serialize");
        assert_eq!(
            json,
            r#"{"type":"session_stats","session_id":"s1","bytes_sent":10,"bytes_received":20,"relay":true,"final_report":false}"#
        );
        let parsed: SignalMessage = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(parsed, stats);
    }

    #[test]
    fn session_stats_flags_default_to_false() {
        let parsed: SignalMessage = serde_json::from_str(
            r#"{"type":"session_stats","session_id":"s1","bytes_sent":1,"bytes_received":2}"#,
        )
        .expect("deserialize");
        assert_eq!(
            parsed,
            SignalMessage::SessionStats {
                session_id: "s1".to_string(),
                bytes_sent: 1,
                bytes_received: 2,
                relay: false,
                final_report: false,
            }
        );
    }

    /// Slice 4.6b: `Bye` without `reason` keeps the old wire form.
    #[test]
    fn bye_without_reason_omits_field_and_parses_old_form() {
        let bye = SignalMessage::Bye {
            session_id: "x".to_string(),
            reason: None,
        };
        let json = serde_json::to_string(&bye).expect("serialize");
        assert_eq!(json, r#"{"type":"bye","session_id":"x"}"#);
        let parsed: SignalMessage =
            serde_json::from_str(r#"{"type":"bye","session_id":"x"}"#).expect("deserialize");
        assert_eq!(parsed, bye);
    }

    #[test]
    fn bye_with_reason_round_trips_through_json() {
        let bye = SignalMessage::Bye {
            session_id: "x".to_string(),
            reason: Some(ByeReason::DisplayUnavailable),
        };
        let json = serde_json::to_string(&bye).expect("serialize");
        assert_eq!(
            json,
            r#"{"type":"bye","session_id":"x","reason":"display_unavailable"}"#
        );
        let parsed: SignalMessage = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(parsed, bye);
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
                pin_rotation: false,
            }
        );
    }

    #[test]
    fn host_register_with_session_id_round_trips_through_json() {
        let host_register = SignalMessage::HostRegister {
            name: "My Mac".to_string(),
            device: None,
            session_id: Some("sess-live".to_string()),
            pin_rotation: false,
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
                pin_rotation: false,
            }
        );
    }

    /// Slice 3.2e: `Offer.auth` is new, so a peer that predates this slice
    /// (or simply an unauthenticated session, where the host never sets it)
    /// must still decode -- `#[serde(default)]` falls back to `None`.
    #[test]
    fn offer_without_auth_field_deserializes_to_none() {
        let json = r#"{"type":"offer","session_id":"sess-1","sdp":"v=0"}"#;

        let msg: SignalMessage = serde_json::from_str(json).expect("deserialize");
        assert_eq!(
            msg,
            SignalMessage::Offer {
                session_id: "sess-1".to_string(),
                sdp: "v=0".to_string(),
                auth: None,
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
            lan_hub: true,
        };

        let json = serde_json::to_string(&authenticated).expect("serialize");
        let round_tripped: SignalMessage = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(round_tripped, authenticated);
    }

    #[test]
    fn authenticated_without_lan_hub_field_deserializes_to_false() {
        let json = r#"{"type":"authenticated","token":"t","devices":[]}"#;

        let msg: SignalMessage = serde_json::from_str(json).expect("deserialize");
        assert_eq!(
            msg,
            SignalMessage::Authenticated {
                token: "t".to_string(),
                devices: Vec::new(),
                lan_hub: false,
            }
        );
    }

    #[test]
    fn browsers_messages_round_trip_through_json() {
        let messages = vec![
            SignalMessage::ListBrowsers,
            SignalMessage::Browsers {
                browsers: vec![BrowserEntry {
                    id: "tok1".to_string(),
                    user_agent: "Mozilla/5.0".to_string(),
                    created_at: 100,
                    last_used_at: 200,
                    current: true,
                }],
            },
            SignalMessage::RevokeBrowser {
                id: "tok2".to_string(),
            },
            SignalMessage::SignOut,
            SignalMessage::SignedOut,
        ];
        for msg in messages {
            let json = serde_json::to_string(&msg).expect("serialize");
            let round_tripped: SignalMessage = serde_json::from_str(&json).expect("deserialize");
            assert_eq!(round_tripped, msg);
        }
    }

    #[test]
    fn passkey_messages_round_trip_through_json() {
        let messages = vec![
            SignalMessage::ListPasskeys,
            SignalMessage::Passkeys {
                passkeys: vec![PasskeyEntry {
                    id: "pk1".to_string(),
                    user_agent: "Mozilla/5.0".to_string(),
                    created_at: 100,
                    last_used_at: 200,
                }],
            },
            SignalMessage::PasskeyRegisterStart,
            SignalMessage::PasskeyRegisterOptions {
                options: r#"{"publicKey":{}}"#.to_string(),
            },
            SignalMessage::PasskeyRegisterFinish {
                credential: r#"{"id":"x"}"#.to_string(),
            },
            SignalMessage::PasskeyLoginStart,
            SignalMessage::PasskeyLoginOptions {
                options: r#"{"publicKey":{}}"#.to_string(),
            },
            SignalMessage::PasskeyLoginFinish {
                credential: r#"{"id":"x"}"#.to_string(),
            },
            SignalMessage::DeletePasskey {
                id: "pk1".to_string(),
            },
        ];
        for msg in messages {
            let json = serde_json::to_string(&msg).expect("serialize");
            let round_tripped: SignalMessage = serde_json::from_str(&json).expect("deserialize");
            assert_eq!(round_tripped, msg);
        }
    }

    #[test]
    fn passkey_messages_have_the_expected_wire_shape() {
        let msg: SignalMessage =
            serde_json::from_str(r#"{"type":"passkey_login_start"}"#).expect("deserialize");
        assert_eq!(msg, SignalMessage::PasskeyLoginStart);
        let msg: SignalMessage =
            serde_json::from_str(r#"{"type":"delete_passkey","id":"x"}"#).expect("deserialize");
        assert_eq!(
            msg,
            SignalMessage::DeletePasskey {
                id: "x".to_string()
            }
        );
    }

    #[test]
    fn account_messages_round_trip_through_json() {
        let messages = vec![
            SignalMessage::EmailCodeRequest {
                email: "a@b.co".to_string(),
            },
            SignalMessage::EmailCodeSent,
            SignalMessage::EmailCodeConfirm {
                email: "a@b.co".to_string(),
                code: "123456".to_string(),
                password: "secret-pass".to_string(),
                purpose: None,
            },
            SignalMessage::EmailCodeConfirm {
                email: "a@b.co".to_string(),
                code: "123456".to_string(),
                password: "secret-pass".to_string(),
                purpose: Some(EmailCodePurpose::Register),
            },
            SignalMessage::EmailCodeConfirm {
                email: "a@b.co".to_string(),
                code: "123456".to_string(),
                password: "secret-pass".to_string(),
                purpose: Some(EmailCodePurpose::Reset),
            },
            SignalMessage::PasswordLogin {
                email: "a@b.co".to_string(),
                password: "secret-pass".to_string(),
            },
            SignalMessage::EmailChange {
                email: "new@b.co".to_string(),
                code: "123456".to_string(),
                password: "secret-pass".to_string(),
            },
            SignalMessage::GetAccount,
            SignalMessage::Account {
                email: Some("a@b.co".to_string()),
            },
            SignalMessage::Account { email: None },
        ];
        for msg in messages {
            let json = serde_json::to_string(&msg).expect("serialize");
            let round_tripped: SignalMessage = serde_json::from_str(&json).expect("deserialize");
            assert_eq!(round_tripped, msg);
        }
    }

    #[test]
    fn email_code_confirm_without_purpose_deserializes_to_none() {
        let msg: SignalMessage = serde_json::from_str(
            r#"{"type":"email_code_confirm","email":"a@b.co","code":"123456","password":"pw"}"#,
        )
        .expect("deserialize");
        assert_eq!(
            msg,
            SignalMessage::EmailCodeConfirm {
                email: "a@b.co".to_string(),
                code: "123456".to_string(),
                password: "pw".to_string(),
                purpose: None,
            }
        );
        let msg: SignalMessage = serde_json::from_str(
            r#"{"type":"email_code_confirm","email":"a@b.co","code":"1","password":"pw","purpose":"reset"}"#,
        )
        .expect("deserialize");
        assert!(matches!(
            msg,
            SignalMessage::EmailCodeConfirm {
                purpose: Some(EmailCodePurpose::Reset),
                ..
            }
        ));
    }

    #[test]
    fn account_messages_have_the_expected_wire_shape() {
        let msg: SignalMessage =
            serde_json::from_str(r#"{"type":"get_account"}"#).expect("deserialize");
        assert_eq!(msg, SignalMessage::GetAccount);
        let msg: SignalMessage =
            serde_json::from_str(r#"{"type":"password_login","email":"a@b.co","password":"p"}"#)
                .expect("deserialize");
        assert_eq!(
            msg,
            SignalMessage::PasswordLogin {
                email: "a@b.co".to_string(),
                password: "p".to_string()
            }
        );
        let json = serde_json::to_string(&SignalMessage::Account { email: None }).expect("ser");
        assert_eq!(json, r#"{"type":"account","email":null}"#);
    }

    #[test]
    fn sign_out_has_the_expected_wire_shape() {
        let msg: SignalMessage =
            serde_json::from_str(r#"{"type":"sign_out"}"#).expect("deserialize");
        assert_eq!(msg, SignalMessage::SignOut);
        let msg: SignalMessage =
            serde_json::from_str(r#"{"type":"revoke_browser","id":"x"}"#).expect("deserialize");
        assert_eq!(
            msg,
            SignalMessage::RevokeBrowser {
                id: "x".to_string()
            }
        );
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

    #[test]
    fn host_register_without_pin_rotation_field_deserializes_to_false() {
        let json = r#"{"type":"host_register","name":"My Mac"}"#;
        let msg: SignalMessage = serde_json::from_str(json).expect("deserialize");
        match msg {
            SignalMessage::HostRegister { pin_rotation, .. } => assert!(!pin_rotation),
            other => panic!("unexpected message: {other:?}"),
        }
    }

    #[test]
    fn pin_changed_serializes_to_exact_json() {
        let json = serde_json::to_string(&SignalMessage::PinChanged {
            pin: "123456".to_string(),
        })
        .expect("serialize");
        assert_eq!(json, r#"{"type":"pin_changed","pin":"123456"}"#);
    }
}
