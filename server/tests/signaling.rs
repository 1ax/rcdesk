use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use proto::signal::{ByeReason, DeviceCredentials, IceCandidate, Role, SignalMessage};
use rcdesk_server::app::{Liveness, NEW_DEVICE_BURST, NEW_OWNER_BURST};
use rcdesk_server::db::Db;
use rcdesk_server::ice::IceConfig;
use rcdesk_server::registry::Registry;
use rcdesk_server::ws::MAX_MESSAGE_BYTES;
use tokio::net::{TcpListener, TcpStream};
use tokio::time::timeout;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::{connect_async, MaybeTlsStream, WebSocketStream};

type WsStream = WebSocketStream<MaybeTlsStream<TcpStream>>;

const TIMEOUT: Duration = Duration::from_secs(2);
/// Used where a test asserts the server stays *silent* (slice 3.5a: no
/// `Bye` for a signaling-only disconnect, no `Error` for a stale `Bye`) --
/// short because we're waiting for a negative to hold, not for an event.
const SILENCE_TIMEOUT: Duration = Duration::from_millis(300);

/// Boots the signaling app on an OS-assigned port and returns its ws:// URL.
async fn spawn_server() -> String {
    spawn_server_with_db().await.0
}

/// Like `spawn_server`, but also hands back a handle to the server's
/// database, for tests that need state the wire protocol can't create
/// (a second token for the same owner).
async fn spawn_server_with_db() -> (String, Db) {
    spawn_full(IceConfig::from_env(), Liveness::default()).await
}

/// Like `spawn_server`, with explicit ICE config and keepalive parameters.
async fn spawn_server_with(ice: IceConfig, liveness: Liveness) -> String {
    spawn_full(ice, liveness).await.0
}

async fn spawn_full(ice: IceConfig, liveness: Liveness) -> (String, Db) {
    spawn_full_with_global(
        ice,
        liveness,
        rcdesk_server::ip_limit::GlobalLimiter::new(
            rcdesk_server::app::PIN_GLOBAL_BURST,
            rcdesk_server::app::PIN_GLOBAL_REFILL,
        ),
    )
    .await
}

async fn spawn_full_with_global(
    ice: IceConfig,
    liveness: Liveness,
    global: rcdesk_server::ip_limit::GlobalLimiter,
) -> (String, Db) {
    spawn_with_mailer(
        ice,
        liveness,
        global,
        std::sync::Arc::new(rcdesk_server::mail::DisabledMailer),
    )
    .await
}

async fn spawn_with_mailer(
    ice: IceConfig,
    liveness: Liveness,
    global: rcdesk_server::ip_limit::GlobalLimiter,
    mailer: rcdesk_server::mail::SharedMailer,
) -> (String, Db) {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind ephemeral port");
    let addr = listener.local_addr().expect("local addr");
    let registry = Registry::new();
    let db = Db::in_memory().expect("open in-memory db");
    let db_handle = db.clone();
    let webauthn = rcdesk_server::passkeys::build_webauthn("localhost", "http://localhost:5173")
        .expect("build webauthn");
    rcdesk_server::app::spawn_pin_rotation(registry.clone(), liveness.pin_ttl);
    tokio::spawn(async move {
        axum::serve(
            listener,
            rcdesk_server::app::app_with_limits(
                registry, ice, db, webauthn, mailer, liveness, global,
            )
            .into_make_service_with_connect_info::<std::net::SocketAddr>(),
        )
        .await
        .expect("serve");
    });
    (format!("ws://{addr}/ws"), db_handle)
}

async fn connect(url: &str) -> WsStream {
    let (stream, _response) = timeout(TIMEOUT, connect_async(url))
        .await
        .expect("connect timed out")
        .expect("connect failed");
    stream
}

async fn send(ws: &mut WsStream, msg: &SignalMessage) {
    let json = serde_json::to_string(msg).expect("serialize signal message");
    timeout(TIMEOUT, ws.send(Message::Text(json.into())))
        .await
        .expect("send timed out")
        .expect("send failed");
}

/// Reads the next `SignalMessage`, skipping WS control frames.
async fn recv(ws: &mut WsStream) -> SignalMessage {
    loop {
        let item = timeout(TIMEOUT, ws.next())
            .await
            .expect("recv timed out")
            .expect("stream ended before a message arrived")
            .expect("websocket transport error");
        match item {
            Message::Text(text) => {
                return serde_json::from_str(&text).expect("parse signal message")
            }
            Message::Ping(_) | Message::Pong(_) => continue,
            other => panic!("unexpected non-text frame: {other:?}"),
        }
    }
}

/// Asserts no message arrives on `ws` within `dur`. Used to check the
/// server stays silent about something (see `SILENCE_TIMEOUT`).
async fn expect_silence(ws: &mut WsStream, dur: Duration) {
    if let Ok(item) = timeout(dur, ws.next()).await {
        match item {
            Some(Ok(Message::Ping(_))) | Some(Ok(Message::Pong(_))) => {}
            Some(Ok(Message::Text(text))) => panic!("expected silence, got {text}"),
            Some(Ok(other)) => panic!("expected silence, got {other:?}"),
            Some(Err(err)) => panic!("expected silence, got transport error {err:?}"),
            None => {}
        }
    }
}

async fn hello(ws: &mut WsStream, role: Role) {
    send(
        ws,
        &SignalMessage::Hello {
            role,
            version: "0.1.0".to_string(),
        },
    )
    .await;
}

/// Sends `ClientAuth { token }` and returns the `Authenticated` response's
/// own token and device list.
async fn client_auth(
    ws: &mut WsStream,
    token: Option<String>,
) -> (String, Vec<proto::signal::DeviceEntry>) {
    send(ws, &SignalMessage::ClientAuth { token }).await;
    match recv(ws).await {
        SignalMessage::Authenticated { token, devices } => (token, devices),
        other => panic!("expected authenticated, got {other:?}"),
    }
}

/// Registers a host and joins it with one client. Returns the connections,
/// the shared session id, and the host's PIN (for tests that need to attempt
/// a second join).
async fn host_and_joined_client(url: &str) -> (WsStream, WsStream, String, String) {
    let mut host = connect(url).await;
    hello(&mut host, Role::Host).await;
    send(
        &mut host,
        &SignalMessage::HostRegister {
            name: "Test Host".to_string(),
            device: None,
            session_id: None,
            pin_rotation: false,
        },
    )
    .await;
    let pin = match recv(&mut host).await {
        SignalMessage::Registered { pin, .. } => pin,
        other => panic!("expected registered, got {other:?}"),
    };

    let mut client = connect(url).await;
    hello(&mut client, Role::Client).await;
    send(&mut client, &SignalMessage::Join { pin: pin.clone() }).await;
    let session_id = match recv(&mut client).await {
        SignalMessage::Joined { session_id, .. } => session_id,
        other => panic!("expected joined, got {other:?}"),
    };
    match recv(&mut host).await {
        SignalMessage::PeerJoined { .. } => {}
        other => panic!("expected peer_joined, got {other:?}"),
    }

    (host, client, session_id, pin)
}

// (a) host registers, receives a 6-digit PIN.
#[tokio::test]
async fn host_registers_and_receives_six_digit_pin() {
    let url = spawn_server().await;
    let mut host = connect(&url).await;
    hello(&mut host, Role::Host).await;
    send(
        &mut host,
        &SignalMessage::HostRegister {
            name: "My Mac".to_string(),
            device: None,
            session_id: None,
            pin_rotation: false,
        },
    )
    .await;

    match recv(&mut host).await {
        SignalMessage::Registered {
            host_id,
            pin,
            ice_servers,
            ..
        } => {
            assert_eq!(host_id.len(), 16);
            assert_eq!(pin.len(), 6);
            assert!(pin.chars().all(|c| c.is_ascii_digit()));
            assert!(!ice_servers.is_empty());
            assert!(ice_servers[0].urls[0].starts_with("stun:"));
        }
        other => panic!("expected registered, got {other:?}"),
    }
}

// (b) client joins with the right PIN: gets Joined with the host name, host
// gets PeerJoined with the same session id.
#[tokio::test]
async fn client_join_succeeds_and_host_is_notified() {
    let url = spawn_server().await;
    let mut host = connect(&url).await;
    hello(&mut host, Role::Host).await;
    send(
        &mut host,
        &SignalMessage::HostRegister {
            name: "My Mac".to_string(),
            device: None,
            session_id: None,
            pin_rotation: false,
        },
    )
    .await;
    let (pin, host_id) = match recv(&mut host).await {
        SignalMessage::Registered { pin, host_id, .. } => (pin, host_id),
        other => panic!("expected registered, got {other:?}"),
    };

    let mut client = connect(&url).await;
    hello(&mut client, Role::Client).await;
    send(&mut client, &SignalMessage::Join { pin }).await;

    let (session_id_client, joined_ice_servers) = match recv(&mut client).await {
        SignalMessage::Joined {
            session_id,
            host_name,
            ice_servers,
            device_id,
        } => {
            assert_eq!(host_name, "My Mac");
            assert!(!ice_servers.is_empty());
            assert!(ice_servers[0].urls[0].starts_with("stun:"));
            // Slice 3.5b: `Joined.device_id` is the host's persistent id
            // (every registered host is one since slice 3.1), so the client
            // can reconnect to the same device without a PIN later.
            assert_eq!(device_id, Some(host_id));
            (session_id, ice_servers)
        }
        other => panic!("expected joined, got {other:?}"),
    };

    match recv(&mut host).await {
        SignalMessage::PeerJoined {
            session_id,
            ice_servers,
        } => {
            assert_eq!(session_id, session_id_client);
            assert!(!ice_servers.is_empty());
            assert_eq!(ice_servers, joined_ice_servers);
        }
        other => panic!("expected peer_joined, got {other:?}"),
    }
}

// (c) Offer/Answer/Ice are forwarded between the two sides of a session.
/// Slice 4.6b: `Bye.reason` from the host reaches the client as is, and a
/// reasonless `Bye` from the client reaches the host without one.
#[tokio::test]
async fn bye_reason_is_forwarded_as_is() {
    let url = spawn_server().await;
    let (mut host, mut client, session_id, _pin) = host_and_joined_client(&url).await;
    send(
        &mut host,
        &SignalMessage::Bye {
            session_id: session_id.clone(),
            reason: Some(ByeReason::DisplayUnavailable),
        },
    )
    .await;
    match recv(&mut client).await {
        SignalMessage::Bye {
            session_id: sid,
            reason,
        } => {
            assert_eq!(sid, session_id);
            assert_eq!(reason, Some(ByeReason::DisplayUnavailable));
        }
        other => panic!("expected bye, got {other:?}"),
    }

    let (mut host, mut client, session_id, _pin) = host_and_joined_client(&url).await;
    send(
        &mut client,
        &SignalMessage::Bye {
            session_id: session_id.clone(),
            reason: None,
        },
    )
    .await;
    match recv(&mut host).await {
        SignalMessage::Bye { reason, .. } => assert_eq!(reason, None),
        other => panic!("expected bye, got {other:?}"),
    }
}

#[tokio::test]
async fn offer_answer_and_ice_are_forwarded_between_session_peers() {
    let url = spawn_server().await;
    let (mut host, mut client, session_id, _pin) = host_and_joined_client(&url).await;

    send(
        &mut host,
        &SignalMessage::Offer {
            session_id: session_id.clone(),
            sdp: "offer-sdp".to_string(),
            // Slice 3.2e: `auth` is relayed opaquely, same as `sdp` -- the
            // server never interprets it, only forwards it.
            auth: Some("t".to_string()),
        },
    )
    .await;
    match recv(&mut client).await {
        SignalMessage::Offer {
            session_id: sid,
            sdp,
            auth,
        } => {
            assert_eq!(sid, session_id);
            assert_eq!(sdp, "offer-sdp");
            assert_eq!(auth, Some("t".to_string()));
        }
        other => panic!("expected offer, got {other:?}"),
    }

    send(
        &mut client,
        &SignalMessage::Answer {
            session_id: session_id.clone(),
            sdp: "answer-sdp".to_string(),
            auth: None,
        },
    )
    .await;
    match recv(&mut host).await {
        SignalMessage::Answer {
            session_id: sid,
            sdp,
            auth,
        } => {
            assert_eq!(sid, session_id);
            assert_eq!(sdp, "answer-sdp");
            assert_eq!(auth, None);
        }
        other => panic!("expected answer, got {other:?}"),
    }

    let candidate = IceCandidate {
        candidate: "candidate:1 1 UDP 2130706431 10.0.0.1 12345 typ host".to_string(),
        sdp_mid: Some("0".to_string()),
        sdp_mline_index: Some(0),
    };

    send(
        &mut host,
        &SignalMessage::Ice {
            session_id: session_id.clone(),
            candidate: candidate.clone(),
        },
    )
    .await;
    match recv(&mut client).await {
        SignalMessage::Ice {
            session_id: sid,
            candidate: c,
        } => {
            assert_eq!(sid, session_id);
            assert_eq!(c, candidate);
        }
        other => panic!("expected ice, got {other:?}"),
    }

    send(
        &mut client,
        &SignalMessage::Ice {
            session_id: session_id.clone(),
            candidate: candidate.clone(),
        },
    )
    .await;
    match recv(&mut host).await {
        SignalMessage::Ice {
            session_id: sid,
            candidate: c,
        } => {
            assert_eq!(sid, session_id);
            assert_eq!(c, candidate);
        }
        other => panic!("expected ice, got {other:?}"),
    }
}

// Slice 3.2c: AuthRequired/PakeStart/AuthFailed are forwarded between the
// two sides of a session, same as Offer/Ice -- the server never interprets
// their `payload`.
#[tokio::test]
async fn auth_required_pake_start_and_auth_failed_are_forwarded_between_session_peers() {
    let url = spawn_server().await;
    let (mut host, mut client, session_id, _pin) = host_and_joined_client(&url).await;

    send(
        &mut host,
        &SignalMessage::AuthRequired {
            session_id: session_id.clone(),
        },
    )
    .await;
    match recv(&mut client).await {
        SignalMessage::AuthRequired { session_id: sid } => assert_eq!(sid, session_id),
        other => panic!("expected auth_required, got {other:?}"),
    }

    send(
        &mut client,
        &SignalMessage::PakeStart {
            session_id: session_id.clone(),
            payload: "client-credential-request".to_string(),
        },
    )
    .await;
    match recv(&mut host).await {
        SignalMessage::PakeStart {
            session_id: sid,
            payload,
        } => {
            assert_eq!(sid, session_id);
            assert_eq!(payload, "client-credential-request");
        }
        other => panic!("expected pake_start, got {other:?}"),
    }

    send(
        &mut host,
        &SignalMessage::AuthFailed {
            session_id: session_id.clone(),
            retry_after_secs: Some(30),
        },
    )
    .await;
    match recv(&mut client).await {
        SignalMessage::AuthFailed {
            session_id: sid,
            retry_after_secs,
        } => {
            assert_eq!(sid, session_id);
            assert_eq!(retry_after_secs, Some(30));
        }
        other => panic!("expected auth_failed, got {other:?}"),
    }
}

// (d) unknown PIN -> Error{"unknown pin"}.
#[tokio::test]
async fn join_with_unknown_pin_returns_error() {
    let url = spawn_server().await;
    let mut client = connect(&url).await;
    hello(&mut client, Role::Client).await;
    send(
        &mut client,
        &SignalMessage::Join {
            pin: "000000".to_string(),
        },
    )
    .await;

    match recv(&mut client).await {
        SignalMessage::Error { message } => assert_eq!(message, "unknown pin"),
        other => panic!("expected error, got {other:?}"),
    }
}

// (e) a second client trying to join an already-busy host -> Error{"host busy"}.
#[tokio::test]
async fn second_client_on_busy_host_gets_error() {
    let url = spawn_server().await;
    let (_host, _client, _session_id, pin) = host_and_joined_client(&url).await;

    let mut second_client = connect(&url).await;
    hello(&mut second_client, Role::Client).await;
    send(&mut second_client, &SignalMessage::Join { pin }).await;

    match recv(&mut second_client).await {
        SignalMessage::Error { message } => assert_eq!(message, "host busy"),
        other => panic!("expected error, got {other:?}"),
    }
}

async fn register_plain_host(url: &str) -> (WsStream, String) {
    let mut host = connect(url).await;
    hello(&mut host, Role::Host).await;
    send(
        &mut host,
        &SignalMessage::HostRegister {
            name: "Test Host".to_string(),
            device: None,
            session_id: None,
            pin_rotation: false,
        },
    )
    .await;
    let pin = match recv(&mut host).await {
        SignalMessage::Registered { pin, .. } => pin,
        other => panic!("expected registered, got {other:?}"),
    };
    (host, pin)
}

#[tokio::test]
async fn client_in_session_cannot_join_a_second_host() {
    let url = spawn_server().await;
    let (_host1, pin1) = register_plain_host(&url).await;
    let (_host2, pin2) = register_plain_host(&url).await;

    let mut client = connect(&url).await;
    hello(&mut client, Role::Client).await;
    send(&mut client, &SignalMessage::Join { pin: pin1 }).await;
    match recv(&mut client).await {
        SignalMessage::Joined { .. } => {}
        other => panic!("expected joined, got {other:?}"),
    }
    send(&mut client, &SignalMessage::Join { pin: pin2.clone() }).await;
    match recv(&mut client).await {
        SignalMessage::Error { message } => assert_eq!(message, "already in session"),
        other => panic!("expected error, got {other:?}"),
    }

    let mut other_client = connect(&url).await;
    hello(&mut other_client, Role::Client).await;
    send(&mut other_client, &SignalMessage::Join { pin: pin2 }).await;
    match recv(&mut other_client).await {
        SignalMessage::Joined { .. } => {}
        other => panic!("expected joined, got {other:?}"),
    }
}

// (f) client disconnects -> the P2P session is left running, so the host
// gets no `Bye` (slice 3.5a) -- but the registry still frees the host, so a
// new client can join with the same PIN. Joining may need a retry or two:
// the server processes the closed socket asynchronously, so there's a short
// window right after `drop` where the host still looks busy.
#[tokio::test]
async fn client_disconnect_frees_host_without_sending_bye() {
    let url = spawn_server().await;
    let (mut host, client, _session_id, pin) = host_and_joined_client(&url).await;

    drop(client);

    expect_silence(&mut host, SILENCE_TIMEOUT).await;

    let mut new_client = connect(&url).await;
    hello(&mut new_client, Role::Client).await;
    loop {
        send(&mut new_client, &SignalMessage::Join { pin: pin.clone() }).await;
        match recv(&mut new_client).await {
            SignalMessage::Joined { .. } => break,
            SignalMessage::Error { message } if message == "host busy" => {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
            other => panic!("expected joined or host busy, got {other:?}"),
        }
    }
}

// (g) the first message on a connection must be Hello, otherwise the server
// replies with an Error and closes the connection.
#[tokio::test]
async fn first_message_must_be_hello() {
    let url = spawn_server().await;
    let mut ws = connect(&url).await;
    send(
        &mut ws,
        &SignalMessage::Join {
            pin: "123456".to_string(),
        },
    )
    .await;

    match recv(&mut ws).await {
        SignalMessage::Error { message } => assert_eq!(message, "expected hello"),
        other => panic!("expected error, got {other:?}"),
    }

    match timeout(TIMEOUT, ws.next()).await.expect("recv timed out") {
        None => {}
        Some(Ok(Message::Close(_))) => {}
        Some(Err(_)) => {}
        other => panic!("expected the connection to close, got {other:?}"),
    }
}

// (h) a host that registers without device credentials (slice 3.1) is
// issued fresh ones, and its host_id is exactly the issued device_id.
#[tokio::test]
async fn host_without_device_credentials_is_issued_fresh_ones() {
    let url = spawn_server().await;
    let mut host = connect(&url).await;
    hello(&mut host, Role::Host).await;
    send(
        &mut host,
        &SignalMessage::HostRegister {
            name: "My Mac".to_string(),
            device: None,
            session_id: None,
            pin_rotation: false,
        },
    )
    .await;

    match recv(&mut host).await {
        SignalMessage::Registered {
            host_id, device, ..
        } => {
            let device = device.expect("first registration issues device credentials");
            assert_eq!(host_id, device.device_id);
        }
        other => panic!("expected registered, got {other:?}"),
    }
}

// (i) presenting previously issued device credentials reconnects under the
// same host_id, and Registered.device is None (nothing new to save).
#[tokio::test]
async fn reconnecting_with_issued_credentials_reuses_host_id() {
    let url = spawn_server().await;

    let mut host = connect(&url).await;
    hello(&mut host, Role::Host).await;
    send(
        &mut host,
        &SignalMessage::HostRegister {
            name: "My Mac".to_string(),
            device: None,
            session_id: None,
            pin_rotation: false,
        },
    )
    .await;
    let (host_id, credentials) = match recv(&mut host).await {
        SignalMessage::Registered {
            host_id, device, ..
        } => (
            host_id,
            device.expect("first registration issues device credentials"),
        ),
        other => panic!("expected registered, got {other:?}"),
    };
    drop(host);

    let mut host2 = connect(&url).await;
    hello(&mut host2, Role::Host).await;
    send(
        &mut host2,
        &SignalMessage::HostRegister {
            name: "My Mac".to_string(),
            device: Some(credentials),
            session_id: None,
            pin_rotation: false,
        },
    )
    .await;

    match recv(&mut host2).await {
        SignalMessage::Registered {
            host_id: reused_host_id,
            device,
            ..
        } => {
            assert_eq!(reused_host_id, host_id);
            assert!(device.is_none());
        }
        other => panic!("expected registered, got {other:?}"),
    }
}

// (j) presenting the right device_id but the wrong secret is rejected.
#[tokio::test]
async fn reconnecting_with_wrong_secret_is_rejected() {
    let url = spawn_server().await;

    let mut host = connect(&url).await;
    hello(&mut host, Role::Host).await;
    send(
        &mut host,
        &SignalMessage::HostRegister {
            name: "My Mac".to_string(),
            device: None,
            session_id: None,
            pin_rotation: false,
        },
    )
    .await;
    let credentials = match recv(&mut host).await {
        SignalMessage::Registered { device, .. } => {
            device.expect("first registration issues device credentials")
        }
        other => panic!("expected registered, got {other:?}"),
    };
    drop(host);

    let mut host2 = connect(&url).await;
    hello(&mut host2, Role::Host).await;
    send(
        &mut host2,
        &SignalMessage::HostRegister {
            name: "My Mac".to_string(),
            device: Some(DeviceCredentials {
                device_id: credentials.device_id,
                secret: format!("{}-wrong", credentials.secret),
            }),
            session_id: None,
            pin_rotation: false,
        },
    )
    .await;

    match recv(&mut host2).await {
        SignalMessage::Error { message } => assert_eq!(message, "invalid device credentials"),
        other => panic!("expected error, got {other:?}"),
    }
}

// (k) presenting a device_id the server has never heard of is rejected --
// e.g. the server's database was recreated.
#[tokio::test]
async fn reconnecting_with_unknown_device_id_is_rejected() {
    let url = spawn_server().await;

    let mut host = connect(&url).await;
    hello(&mut host, Role::Host).await;
    send(
        &mut host,
        &SignalMessage::HostRegister {
            name: "My Mac".to_string(),
            device: Some(DeviceCredentials {
                device_id: "no-such-device".to_string(),
                secret: "whatever".to_string(),
            }),
            session_id: None,
            pin_rotation: false,
        },
    )
    .await;

    match recv(&mut host).await {
        SignalMessage::Error { message } => assert_eq!(message, "unknown device"),
        other => panic!("expected error, got {other:?}"),
    }
}

// (l) a second connection presenting the same device credentials displaces
// the first: the first gets Error{"replaced by a new connection"}, and the
// client of its active session gets no `Bye` (slice 3.5a: the P2P session
// is left running) -- the new connection still registers under the same
// host_id.
#[tokio::test]
async fn re_registering_same_device_displaces_old_connection_without_bye_to_its_client() {
    let url = spawn_server().await;

    let mut host1 = connect(&url).await;
    hello(&mut host1, Role::Host).await;
    send(
        &mut host1,
        &SignalMessage::HostRegister {
            name: "Test Host".to_string(),
            device: None,
            session_id: None,
            pin_rotation: false,
        },
    )
    .await;
    let (host_id1, pin1, credentials) = match recv(&mut host1).await {
        SignalMessage::Registered {
            host_id,
            pin,
            device,
            ..
        } => (
            host_id,
            pin,
            device.expect("first registration issues device credentials"),
        ),
        other => panic!("expected registered, got {other:?}"),
    };

    let mut client = connect(&url).await;
    hello(&mut client, Role::Client).await;
    send(&mut client, &SignalMessage::Join { pin: pin1.clone() }).await;
    match recv(&mut client).await {
        SignalMessage::Joined { .. } => {}
        other => panic!("expected joined, got {other:?}"),
    };
    match recv(&mut host1).await {
        SignalMessage::PeerJoined { .. } => {}
        other => panic!("expected peer_joined, got {other:?}"),
    }

    let mut host2 = connect(&url).await;
    hello(&mut host2, Role::Host).await;
    send(
        &mut host2,
        &SignalMessage::HostRegister {
            name: "Test Host".to_string(),
            device: Some(credentials),
            session_id: None,
            pin_rotation: false,
        },
    )
    .await;

    match recv(&mut host1).await {
        SignalMessage::Error { message } => assert_eq!(message, "replaced by a new connection"),
        other => panic!("expected error, got {other:?}"),
    }
    expect_silence(&mut client, SILENCE_TIMEOUT).await;
    match recv(&mut host2).await {
        SignalMessage::Registered {
            host_id, device, ..
        } => {
            assert_eq!(host_id, host_id1);
            assert!(device.is_none());
        }
        other => panic!("expected registered, got {other:?}"),
    }
}

// (m) a client with no saved token sends ClientAuth{token: None} and gets
// back a fresh, non-empty token with an empty device list.
#[tokio::test]
async fn client_auth_without_token_issues_token_with_empty_devices() {
    let url = spawn_server().await;
    let mut client = connect(&url).await;
    hello(&mut client, Role::Client).await;

    let (token, devices) = client_auth(&mut client, None).await;

    assert!(!token.is_empty());
    assert!(devices.is_empty());
}

// (n) presenting a previously issued token on a new connection is
// recognized: same token comes back.
#[tokio::test]
async fn client_auth_with_known_token_returns_same_token() {
    let url = spawn_server().await;
    let mut client = connect(&url).await;
    hello(&mut client, Role::Client).await;
    let (token, _devices) = client_auth(&mut client, None).await;
    drop(client);

    let mut client2 = connect(&url).await;
    hello(&mut client2, Role::Client).await;
    let (token2, _devices2) = client_auth(&mut client2, Some(token.clone())).await;

    assert_eq!(token2, token);
}

// (o) an unrecognized token is not resurrected: a different owner and token
// are issued instead.
#[tokio::test]
async fn client_auth_with_unknown_token_issues_a_different_token() {
    let url = spawn_server().await;
    let mut client = connect(&url).await;
    hello(&mut client, Role::Client).await;

    let (token, _devices) = client_auth(&mut client, Some("not-a-real-token".to_string())).await;

    assert_ne!(token, "not-a-real-token");
}

// (p) after ClientAuth, joining by PIN links the device to the owner: on a
// new connection, ClientAuth with the same token lists that device_id,
// online (the host is still connected).
#[tokio::test]
async fn joining_by_pin_after_client_auth_links_device_to_owner() {
    let url = spawn_server().await;

    let mut host = connect(&url).await;
    hello(&mut host, Role::Host).await;
    send(
        &mut host,
        &SignalMessage::HostRegister {
            name: "Test Host".to_string(),
            device: None,
            session_id: None,
            pin_rotation: false,
        },
    )
    .await;
    let (host_id, pin) = match recv(&mut host).await {
        SignalMessage::Registered {
            host_id,
            pin,
            device,
            ..
        } => {
            assert!(device.is_some());
            (host_id, pin)
        }
        other => panic!("expected registered, got {other:?}"),
    };

    let mut client = connect(&url).await;
    hello(&mut client, Role::Client).await;
    let (token, _devices) = client_auth(&mut client, None).await;
    send(&mut client, &SignalMessage::Join { pin }).await;
    match recv(&mut client).await {
        SignalMessage::Joined { .. } => {}
        other => panic!("expected joined, got {other:?}"),
    }
    match recv(&mut host).await {
        SignalMessage::PeerJoined { .. } => {}
        other => panic!("expected peer_joined, got {other:?}"),
    }
    drop(client);

    let mut client2 = connect(&url).await;
    hello(&mut client2, Role::Client).await;
    let (_token2, devices) = client_auth(&mut client2, Some(token)).await;

    assert_eq!(devices.len(), 1);
    assert_eq!(devices[0].device_id, host_id);
    assert!(devices[0].online);
}

// (q) ConnectDevice on a device linked to the authenticated owner starts a
// session: the client gets Joined, the host gets PeerJoined with the same
// session_id.
#[tokio::test]
async fn connect_device_starts_a_session_for_a_linked_device() {
    let url = spawn_server().await;

    let mut host = connect(&url).await;
    hello(&mut host, Role::Host).await;
    send(
        &mut host,
        &SignalMessage::HostRegister {
            name: "Test Host".to_string(),
            device: None,
            session_id: None,
            pin_rotation: false,
        },
    )
    .await;
    let (host_id, pin) = match recv(&mut host).await {
        SignalMessage::Registered { host_id, pin, .. } => (host_id, pin),
        other => panic!("expected registered, got {other:?}"),
    };

    // First, link the device to an owner by joining once via PIN, then end
    // that session so the host is free for `ConnectDevice`.
    let mut linking_client = connect(&url).await;
    hello(&mut linking_client, Role::Client).await;
    let (token, _devices) = client_auth(&mut linking_client, None).await;
    send(&mut linking_client, &SignalMessage::Join { pin }).await;
    let session_id = match recv(&mut linking_client).await {
        SignalMessage::Joined { session_id, .. } => session_id,
        other => panic!("expected joined, got {other:?}"),
    };
    match recv(&mut host).await {
        SignalMessage::PeerJoined { .. } => {}
        other => panic!("expected peer_joined, got {other:?}"),
    }
    send(
        &mut linking_client,
        &SignalMessage::Bye {
            session_id: session_id.clone(),
            reason: None,
        },
    )
    .await;
    match recv(&mut host).await {
        SignalMessage::Bye {
            session_id: sid, ..
        } => assert_eq!(sid, session_id),
        other => panic!("expected bye, got {other:?}"),
    }
    drop(linking_client);

    let mut client = connect(&url).await;
    hello(&mut client, Role::Client).await;
    client_auth(&mut client, Some(token)).await;
    send(
        &mut client,
        &SignalMessage::ConnectDevice {
            device_id: host_id.clone(),
        },
    )
    .await;

    let new_session_id = match recv(&mut client).await {
        SignalMessage::Joined {
            session_id,
            device_id,
            ..
        } => {
            // Slice 3.5b: `ConnectDevice`'s `Joined` carries back the same
            // `device_id` the client asked for.
            assert_eq!(device_id, Some(host_id));
            session_id
        }
        other => panic!("expected joined, got {other:?}"),
    };
    match recv(&mut host).await {
        SignalMessage::PeerJoined { session_id, .. } => assert_eq!(session_id, new_session_id),
        other => panic!("expected peer_joined, got {other:?}"),
    }
}

// (r) ConnectDevice for a device_id not linked to the authenticated owner ->
// Error{"device not linked"}.
#[tokio::test]
async fn connect_device_for_unlinked_device_returns_error() {
    let url = spawn_server().await;
    let mut client = connect(&url).await;
    hello(&mut client, Role::Client).await;
    client_auth(&mut client, None).await;

    send(
        &mut client,
        &SignalMessage::ConnectDevice {
            device_id: "no-such-device".to_string(),
        },
    )
    .await;

    match recv(&mut client).await {
        SignalMessage::Error { message } => assert_eq!(message, "device not linked"),
        other => panic!("expected error, got {other:?}"),
    }
}

// (s) ConnectDevice for a linked device whose host has since disconnected ->
// Error{"device offline"}.
#[tokio::test]
async fn connect_device_for_offline_device_returns_error() {
    let url = spawn_server().await;

    let mut host = connect(&url).await;
    hello(&mut host, Role::Host).await;
    send(
        &mut host,
        &SignalMessage::HostRegister {
            name: "Test Host".to_string(),
            device: None,
            session_id: None,
            pin_rotation: false,
        },
    )
    .await;
    let (host_id, pin) = match recv(&mut host).await {
        SignalMessage::Registered { host_id, pin, .. } => (host_id, pin),
        other => panic!("expected registered, got {other:?}"),
    };

    let mut client = connect(&url).await;
    hello(&mut client, Role::Client).await;
    client_auth(&mut client, None).await;
    send(&mut client, &SignalMessage::Join { pin }).await;
    match recv(&mut client).await {
        SignalMessage::Joined { .. } => {}
        other => panic!("expected joined, got {other:?}"),
    }
    match recv(&mut host).await {
        SignalMessage::PeerJoined { .. } => {}
        other => panic!("expected peer_joined, got {other:?}"),
    }

    // Dropping the host no longer sends the client a `Bye` (slice 3.5a), so
    // there's nothing to wait on for the server to notice the closed socket
    // and remove the host from the registry. Retry `ConnectDevice` until
    // that's happened -- while it's still in flight the host looks busy
    // (its stale session hasn't been cleared yet), not offline.
    drop(host);
    let message = loop {
        send(
            &mut client,
            &SignalMessage::ConnectDevice {
                device_id: host_id.clone(),
            },
        )
        .await;
        match recv(&mut client).await {
            SignalMessage::Error { message } if message == "host busy" => {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
            SignalMessage::Error { message } => break message,
            other => panic!("expected error, got {other:?}"),
        }
    };
    assert_eq!(message, "device offline");
}

// (t) ListDevices and ConnectDevice before any ClientAuth -> Error{"not
// authenticated"}.
#[tokio::test]
async fn list_devices_and_connect_device_without_client_auth_return_not_authenticated() {
    let url = spawn_server().await;
    let mut client = connect(&url).await;
    hello(&mut client, Role::Client).await;

    send(&mut client, &SignalMessage::ListDevices).await;
    match recv(&mut client).await {
        SignalMessage::Error { message } => assert_eq!(message, "not authenticated"),
        other => panic!("expected error, got {other:?}"),
    }

    send(
        &mut client,
        &SignalMessage::ConnectDevice {
            device_id: "whatever".to_string(),
        },
    )
    .await;
    match recv(&mut client).await {
        SignalMessage::Error { message } => assert_eq!(message, "not authenticated"),
        other => panic!("expected error, got {other:?}"),
    }
}

// (u) RenameDevice changes the alias in the returned Devices list;
// ForgetDevice then removes the device from it.
#[tokio::test]
async fn rename_device_updates_alias_and_forget_device_removes_it() {
    let url = spawn_server().await;

    let mut host = connect(&url).await;
    hello(&mut host, Role::Host).await;
    send(
        &mut host,
        &SignalMessage::HostRegister {
            name: "Test Host".to_string(),
            device: None,
            session_id: None,
            pin_rotation: false,
        },
    )
    .await;
    let (host_id, pin) = match recv(&mut host).await {
        SignalMessage::Registered { host_id, pin, .. } => (host_id, pin),
        other => panic!("expected registered, got {other:?}"),
    };

    let mut client = connect(&url).await;
    hello(&mut client, Role::Client).await;
    client_auth(&mut client, None).await;
    send(&mut client, &SignalMessage::Join { pin }).await;
    match recv(&mut client).await {
        SignalMessage::Joined { .. } => {}
        other => panic!("expected joined, got {other:?}"),
    }
    match recv(&mut host).await {
        SignalMessage::PeerJoined { .. } => {}
        other => panic!("expected peer_joined, got {other:?}"),
    }

    send(
        &mut client,
        &SignalMessage::RenameDevice {
            device_id: host_id.clone(),
            alias: Some("Work Mac".to_string()),
        },
    )
    .await;
    match recv(&mut client).await {
        SignalMessage::Devices { devices } => {
            assert_eq!(devices.len(), 1);
            assert_eq!(devices[0].alias, Some("Work Mac".to_string()));
        }
        other => panic!("expected devices, got {other:?}"),
    }

    send(
        &mut client,
        &SignalMessage::ForgetDevice { device_id: host_id },
    )
    .await;
    match recv(&mut client).await {
        SignalMessage::Devices { devices } => assert!(devices.is_empty()),
        other => panic!("expected devices, got {other:?}"),
    }
}

// (v) `Bye` for a session_id the server doesn't know -- from a host or from
// a client -- is a silent no-op, not `Error{"not in session"}` (slice 3.5a:
// a stale `Bye` sent after a signaling reconnect, e.g. for a session that
// already ended some other way, shouldn't look like a protocol violation).
#[tokio::test]
async fn bye_for_unknown_session_from_host_or_client_is_a_silent_no_op() {
    let url = spawn_server().await;

    let mut host = connect(&url).await;
    hello(&mut host, Role::Host).await;
    send(
        &mut host,
        &SignalMessage::HostRegister {
            name: "Test Host".to_string(),
            device: None,
            session_id: None,
            pin_rotation: false,
        },
    )
    .await;
    match recv(&mut host).await {
        SignalMessage::Registered { .. } => {}
        other => panic!("expected registered, got {other:?}"),
    }
    send(
        &mut host,
        &SignalMessage::Bye {
            session_id: "no-such-session".to_string(),
            reason: None,
        },
    )
    .await;
    expect_silence(&mut host, SILENCE_TIMEOUT).await;

    let mut client = connect(&url).await;
    hello(&mut client, Role::Client).await;
    send(
        &mut client,
        &SignalMessage::Bye {
            session_id: "no-such-session".to_string(),
            reason: None,
        },
    )
    .await;
    expect_silence(&mut client, SILENCE_TIMEOUT).await;
}

// (w) slice 3.2a (D35): a host reconnecting to signaling while it still has
// a live P2P session (a `session_id` the server itself has no `SessionEntry`
// for -- the same "detached" shape a real signaling-only reconnect leaves
// behind, slice 3.5a) marks the device busy: the linked owner's device list
// shows `busy: true`, a second client can't `ConnectDevice` to it, and only
// the host's own `Bye` for that session frees it again.
#[tokio::test]
async fn host_reconnect_with_live_session_id_marks_device_busy_until_bye() {
    let url = spawn_server().await;

    let mut host = connect(&url).await;
    hello(&mut host, Role::Host).await;
    send(
        &mut host,
        &SignalMessage::HostRegister {
            name: "Test Host".to_string(),
            device: None,
            session_id: None,
            pin_rotation: false,
        },
    )
    .await;
    let (host_id, pin, credentials) = match recv(&mut host).await {
        SignalMessage::Registered {
            host_id,
            pin,
            device,
            ..
        } => (
            host_id,
            pin,
            device.expect("first registration issues device credentials"),
        ),
        other => panic!("expected registered, got {other:?}"),
    };

    // Link the device to an owner by joining once via PIN, then end that
    // session normally so the host is free again -- same setup as
    // `connect_device_starts_a_session_for_a_linked_device`.
    let mut owner = connect(&url).await;
    hello(&mut owner, Role::Client).await;
    let (token, _devices) = client_auth(&mut owner, None).await;
    send(&mut owner, &SignalMessage::Join { pin }).await;
    let session_id = match recv(&mut owner).await {
        SignalMessage::Joined { session_id, .. } => session_id,
        other => panic!("expected joined, got {other:?}"),
    };
    match recv(&mut host).await {
        SignalMessage::PeerJoined { .. } => {}
        other => panic!("expected peer_joined, got {other:?}"),
    }
    send(
        &mut owner,
        &SignalMessage::Bye {
            session_id,
            reason: None,
        },
    )
    .await;
    match recv(&mut host).await {
        SignalMessage::Bye { .. } => {}
        other => panic!("expected bye, got {other:?}"),
    }
    drop(owner);
    drop(host);

    // The host reconnects to signaling reporting the P2P session it still
    // has live ("s-live") -- this fresh registry entry has no `SessionEntry`
    // for it, same as a real signaling-only reconnect (slice 3.5a) would
    // leave behind before the P2P side later reports back in.
    let mut host2 = connect(&url).await;
    hello(&mut host2, Role::Host).await;
    send(
        &mut host2,
        &SignalMessage::HostRegister {
            name: "Test Host".to_string(),
            device: Some(credentials),
            session_id: Some("s-live".to_string()),
            pin_rotation: false,
        },
    )
    .await;
    match recv(&mut host2).await {
        SignalMessage::Registered {
            host_id: reused_host_id,
            ..
        } => assert_eq!(reused_host_id, host_id),
        other => panic!("expected registered, got {other:?}"),
    }

    let mut viewer = connect(&url).await;
    hello(&mut viewer, Role::Client).await;
    let (_token2, devices) = client_auth(&mut viewer, Some(token.clone())).await;
    assert_eq!(devices.len(), 1);
    assert_eq!(devices[0].device_id, host_id);
    assert!(devices[0].busy, "device must be busy after re-attaching");

    let mut second_client = connect(&url).await;
    hello(&mut second_client, Role::Client).await;
    client_auth(&mut second_client, Some(token.clone())).await;
    send(
        &mut second_client,
        &SignalMessage::ConnectDevice {
            device_id: host_id.clone(),
        },
    )
    .await;
    match recv(&mut second_client).await {
        SignalMessage::Error { message } => assert_eq!(message, "host busy"),
        other => panic!("expected error, got {other:?}"),
    }

    send(
        &mut host2,
        &SignalMessage::Bye {
            session_id: "s-live".to_string(),
            reason: None,
        },
    )
    .await;
    expect_silence(&mut host2, SILENCE_TIMEOUT).await;

    send(&mut viewer, &SignalMessage::ListDevices).await;
    match recv(&mut viewer).await {
        SignalMessage::Devices { devices } => {
            assert_eq!(devices.len(), 1);
            assert!(
                !devices[0].busy,
                "device must be free again after the host's bye"
            );
        }
        other => panic!("expected devices, got {other:?}"),
    }
}

/// Like `connect`, but presents `X-Real-IP` the way nginx does in production.
async fn connect_as(url: &str, real_ip: &str) -> WsStream {
    use tokio_tungstenite::tungstenite::client::IntoClientRequest;
    let mut request = url.into_client_request().expect("build request");
    request
        .headers_mut()
        .insert("x-real-ip", real_ip.parse().expect("header value"));
    let (stream, _response) = timeout(TIMEOUT, connect_async(request))
        .await
        .expect("connect timed out")
        .expect("connect failed");
    stream
}

// Slice 3.6a (D36): wrong-PIN guesses are rate-limited per client IP. After
// 10 misses the IP is blocked even for a correct PIN; another IP is unaffected.
#[tokio::test]
async fn wrong_pin_attempts_are_rate_limited_per_ip() {
    let url = spawn_server().await;

    let mut host = connect(&url).await;
    hello(&mut host, Role::Host).await;
    send(
        &mut host,
        &SignalMessage::HostRegister {
            name: "Test Host".to_string(),
            device: None,
            session_id: None,
            pin_rotation: false,
        },
    )
    .await;
    let pin = match recv(&mut host).await {
        SignalMessage::Registered { pin, .. } => pin,
        other => panic!("expected registered, got {other:?}"),
    };

    let mut attacker = connect_as(&url, "198.51.100.1").await;
    hello(&mut attacker, Role::Client).await;
    for _ in 0..10 {
        send(
            &mut attacker,
            &SignalMessage::Join {
                pin: "000000".to_string(),
            },
        )
        .await;
        match recv(&mut attacker).await {
            SignalMessage::Error { message } => assert_eq!(message, "unknown pin"),
            other => panic!("expected unknown pin, got {other:?}"),
        }
    }
    send(
        &mut attacker,
        &SignalMessage::Join {
            pin: "000000".to_string(),
        },
    )
    .await;
    match recv(&mut attacker).await {
        SignalMessage::Error { message } => assert_eq!(message, "too many attempts"),
        other => panic!("expected too many attempts, got {other:?}"),
    }
    send(&mut attacker, &SignalMessage::Join { pin: pin.clone() }).await;
    match recv(&mut attacker).await {
        SignalMessage::Error { message } => assert_eq!(message, "too many attempts"),
        other => panic!("expected too many attempts, got {other:?}"),
    }

    let mut other = connect_as(&url, "198.51.100.2").await;
    hello(&mut other, Role::Client).await;
    send(&mut other, &SignalMessage::Join { pin }).await;
    match recv(&mut other).await {
        SignalMessage::Joined { .. } => {}
        other => panic!("expected joined, got {other:?}"),
    }
}

/// Reads one `Browsers` response.
async fn recv_browsers(ws: &mut WsStream) -> Vec<proto::signal::BrowserEntry> {
    match recv(ws).await {
        SignalMessage::Browsers { browsers } => browsers,
        other => panic!("expected browsers, got {other:?}"),
    }
}

/// Asserts the next message is `Error { "not authenticated" }`.
async fn expect_not_authenticated(ws: &mut WsStream) {
    match recv(ws).await {
        SignalMessage::Error { message } => assert_eq!(message, "not authenticated"),
        other => panic!("expected not authenticated, got {other:?}"),
    }
}

// (3.7a-a) a second connection authenticating with the first one's token
// sees the same devices; ListBrowsers shows exactly one browser, current.
#[tokio::test]
async fn second_connection_with_same_token_shares_devices_and_lists_one_browser() {
    let url = spawn_server().await;

    let mut host = connect(&url).await;
    hello(&mut host, Role::Host).await;
    send(
        &mut host,
        &SignalMessage::HostRegister {
            name: "Test Host".to_string(),
            device: None,
            session_id: None,
            pin_rotation: false,
        },
    )
    .await;
    let (host_id, pin) = match recv(&mut host).await {
        SignalMessage::Registered { host_id, pin, .. } => (host_id, pin),
        other => panic!("expected registered, got {other:?}"),
    };

    let mut first = connect(&url).await;
    hello(&mut first, Role::Client).await;
    let (token, _) = client_auth(&mut first, None).await;
    send(&mut first, &SignalMessage::Join { pin }).await;
    match recv(&mut first).await {
        SignalMessage::Joined { .. } => {}
        other => panic!("expected joined, got {other:?}"),
    }

    let mut second = connect(&url).await;
    hello(&mut second, Role::Client).await;
    let (token2, devices) = client_auth(&mut second, Some(token.clone())).await;
    assert_eq!(token2, token);
    assert_eq!(devices.len(), 1);
    assert_eq!(devices[0].device_id, host_id);

    send(&mut second, &SignalMessage::ListBrowsers).await;
    let browsers = recv_browsers(&mut second).await;
    assert_eq!(browsers.len(), 1);
    assert!(browsers[0].current);
}

// (3.7a-b) an owner with two tokens: revoking B from A leaves one browser;
// B's connection is then "not authenticated", and B's old token yields a
// brand-new, empty owner. The second token is inserted through the Db
// handle (the wire protocol can't mint one).
#[tokio::test]
async fn revoking_another_browser_signs_that_connection_out() {
    let (url, db) = spawn_server_with_db().await;

    let mut a = connect(&url).await;
    hello(&mut a, Role::Client).await;
    let (token_a, _) = client_auth(&mut a, None).await;
    let (owner_id, token_id_a) = db
        .token_by_hash(&rcdesk_server::devices::hash_secret(&token_a))
        .expect("query token")
        .expect("token a stored");
    db.insert_token(
        &owner_id,
        "token-b-id",
        &rcdesk_server::devices::hash_secret("token-b-secret"),
        "Browser B",
        1,
    )
    .expect("insert second token");

    let mut b = connect(&url).await;
    hello(&mut b, Role::Client).await;
    let (token_b, _) = client_auth(&mut b, Some("token-b-secret".to_string())).await;
    assert_eq!(token_b, "token-b-secret");

    send(&mut a, &SignalMessage::ListBrowsers).await;
    let browsers = recv_browsers(&mut a).await;
    assert_eq!(browsers.len(), 2);
    assert_eq!(browsers.iter().filter(|b| b.current).count(), 1);
    assert!(browsers.iter().any(|b| b.current && b.id == token_id_a));

    send(
        &mut a,
        &SignalMessage::RevokeBrowser {
            id: "token-b-id".to_string(),
        },
    )
    .await;
    let browsers = recv_browsers(&mut a).await;
    assert_eq!(browsers.len(), 1);
    assert_eq!(browsers[0].id, token_id_a);

    send(&mut b, &SignalMessage::ListDevices).await;
    expect_not_authenticated(&mut b).await;

    let mut b2 = connect(&url).await;
    hello(&mut b2, Role::Client).await;
    let (new_token, devices) = client_auth(&mut b2, Some("token-b-secret".to_string())).await;
    assert_ne!(new_token, "token-b-secret");
    assert!(devices.is_empty());
    let (new_owner, _) = db
        .token_by_hash(&rcdesk_server::devices::hash_secret(&new_token))
        .expect("query token")
        .expect("new token stored");
    assert_ne!(new_owner, owner_id);
}

// (3.7a-c) SignOut -> SignedOut; afterwards the connection is "not
// authenticated" and the token is no longer recognized.
#[tokio::test]
async fn sign_out_forgets_owner_and_invalidates_token() {
    let url = spawn_server().await;

    let mut client = connect(&url).await;
    hello(&mut client, Role::Client).await;
    let (token, _) = client_auth(&mut client, None).await;

    send(&mut client, &SignalMessage::SignOut).await;
    match recv(&mut client).await {
        SignalMessage::SignedOut => {}
        other => panic!("expected signed_out, got {other:?}"),
    }
    send(&mut client, &SignalMessage::ListDevices).await;
    expect_not_authenticated(&mut client).await;

    let mut again = connect(&url).await;
    hello(&mut again, Role::Client).await;
    let (new_token, _) = client_auth(&mut again, Some(token.clone())).await;
    assert_ne!(new_token, token);
}

// (3.7a-d) RevokeBrowser with the connection's own token id behaves as
// SignOut.
#[tokio::test]
async fn revoking_own_browser_acts_as_sign_out() {
    let url = spawn_server().await;

    let mut client = connect(&url).await;
    hello(&mut client, Role::Client).await;
    client_auth(&mut client, None).await;
    send(&mut client, &SignalMessage::ListBrowsers).await;
    let browsers = recv_browsers(&mut client).await;

    send(
        &mut client,
        &SignalMessage::RevokeBrowser {
            id: browsers[0].id.clone(),
        },
    )
    .await;
    match recv(&mut client).await {
        SignalMessage::SignedOut => {}
        other => panic!("expected signed_out, got {other:?}"),
    }
    send(&mut client, &SignalMessage::ListBrowsers).await;
    expect_not_authenticated(&mut client).await;
}

/// Asserts the next message is `Error { message }` with exactly this text.
async fn expect_error(ws: &mut WsStream, expected: &str) {
    match recv(ws).await {
        SignalMessage::Error { message } => assert_eq!(message, expected),
        other => panic!("expected error {expected:?}, got {other:?}"),
    }
}

// (3.7b-a) passkey management without ClientAuth is "not authenticated".
#[tokio::test]
async fn passkey_management_without_client_auth_returns_not_authenticated() {
    let url = spawn_server().await;
    let mut client = connect(&url).await;
    hello(&mut client, Role::Client).await;

    for msg in [
        SignalMessage::ListPasskeys,
        SignalMessage::PasskeyRegisterStart,
        SignalMessage::PasskeyRegisterFinish {
            credential: "{}".to_string(),
        },
        SignalMessage::DeletePasskey {
            id: "x".to_string(),
        },
    ] {
        send(&mut client, &msg).await;
        expect_not_authenticated(&mut client).await;
    }
}

// (3.7b-b) an authenticated owner with no passkeys gets an empty list, and
// RegisterStart returns options that parse as JSON and demand a resident key.
#[cfg(feature = "passkeys")]
#[tokio::test]
async fn list_passkeys_is_empty_and_register_start_returns_options() {
    let url = spawn_server().await;
    let mut client = connect(&url).await;
    hello(&mut client, Role::Client).await;
    client_auth(&mut client, None).await;

    send(&mut client, &SignalMessage::ListPasskeys).await;
    match recv(&mut client).await {
        SignalMessage::Passkeys { passkeys } => assert!(passkeys.is_empty()),
        other => panic!("expected passkeys, got {other:?}"),
    }

    send(&mut client, &SignalMessage::PasskeyRegisterStart).await;
    match recv(&mut client).await {
        SignalMessage::PasskeyRegisterOptions { options } => {
            let json: serde_json::Value = serde_json::from_str(&options).expect("options json");
            assert_eq!(
                json["publicKey"]["authenticatorSelection"]["residentKey"],
                "required"
            );
        }
        other => panic!("expected passkey_register_options, got {other:?}"),
    }
}

// (3.7b-c) Finish without Start, a second Finish and garbage are all
// "passkey failed"; login needs no owner.
#[cfg(feature = "passkeys")]
#[tokio::test]
async fn passkey_finish_without_start_or_with_garbage_fails() {
    let url = spawn_server().await;
    let mut client = connect(&url).await;
    hello(&mut client, Role::Client).await;
    client_auth(&mut client, None).await;

    let finish = SignalMessage::PasskeyRegisterFinish {
        credential: "{}".to_string(),
    };
    send(&mut client, &finish).await;
    expect_error(&mut client, "passkey failed").await;

    send(&mut client, &SignalMessage::PasskeyRegisterStart).await;
    let _ = recv(&mut client).await;
    send(&mut client, &finish).await;
    expect_error(&mut client, "passkey failed").await;
    send(&mut client, &finish).await;
    expect_error(&mut client, "passkey failed").await;

    // Login: a stranger connection (no ClientAuth at all).
    let mut stranger = connect(&url).await;
    hello(&mut stranger, Role::Client).await;
    let login_finish = SignalMessage::PasskeyLoginFinish {
        credential: "garbage".to_string(),
    };
    send(&mut stranger, &login_finish).await;
    expect_error(&mut stranger, "passkey failed").await;

    send(&mut stranger, &SignalMessage::PasskeyLoginStart).await;
    match recv(&mut stranger).await {
        SignalMessage::PasskeyLoginOptions { options } => {
            let json: serde_json::Value = serde_json::from_str(&options).expect("options json");
            assert_eq!(json["publicKey"]["userVerification"], "required");
        }
        other => panic!("expected passkey_login_options, got {other:?}"),
    }
    send(&mut stranger, &login_finish).await;
    expect_error(&mut stranger, "passkey failed").await;
    // The state was consumed by the failed finish.
    send(&mut stranger, &login_finish).await;
    expect_error(&mut stranger, "passkey failed").await;
}

// (D47-1) Без фичи `passkeys` список читается из SQLite, а любая церемония
// (старт регистрации и входа) отвечает "passkey failed" (причина --
// "passkeys disabled" -- только в логе).
#[cfg(not(feature = "passkeys"))]
#[tokio::test]
async fn passkey_ceremonies_are_disabled_without_the_feature() {
    let url = spawn_server().await;
    let mut client = connect(&url).await;
    hello(&mut client, Role::Client).await;
    client_auth(&mut client, None).await;

    send(&mut client, &SignalMessage::ListPasskeys).await;
    match recv(&mut client).await {
        SignalMessage::Passkeys { passkeys } => assert!(passkeys.is_empty()),
        other => panic!("expected passkeys, got {other:?}"),
    }

    send(&mut client, &SignalMessage::PasskeyRegisterStart).await;
    expect_error(&mut client, "passkey failed").await;

    let mut stranger = connect(&url).await;
    hello(&mut stranger, Role::Client).await;
    send(&mut stranger, &SignalMessage::PasskeyLoginStart).await;
    expect_error(&mut stranger, "passkey failed").await;
}

// (3.7b-d) DeletePasskey removes the owner's own passkey but never someone
// else's. Passkeys are inserted through the Db handle (no authenticator).
#[tokio::test]
async fn delete_passkey_is_scoped_to_the_owner() {
    let (url, db) = spawn_server_with_db().await;
    db.insert_owner("other", 1).expect("other owner");
    db.insert_passkey("foreign", "other", "cred-foreign", "{}", "", 1)
        .expect("foreign passkey");

    let mut client = connect(&url).await;
    hello(&mut client, Role::Client).await;
    let (token, _) = client_auth(&mut client, None).await;
    let (owner_id, _) = db
        .token_by_hash(&rcdesk_server::devices::hash_secret(&token))
        .expect("token lookup")
        .expect("token present");
    db.insert_passkey("mine", &owner_id, "cred-mine", "{}", "UA", 2)
        .expect("own passkey");

    send(&mut client, &SignalMessage::ListPasskeys).await;
    match recv(&mut client).await {
        SignalMessage::Passkeys { passkeys } => {
            assert_eq!(passkeys.len(), 1);
            assert_eq!(passkeys[0].id, "mine");
        }
        other => panic!("expected passkeys, got {other:?}"),
    }

    send(
        &mut client,
        &SignalMessage::DeletePasskey {
            id: "foreign".to_string(),
        },
    )
    .await;
    match recv(&mut client).await {
        SignalMessage::Passkeys { passkeys } => assert_eq!(passkeys.len(), 1),
        other => panic!("expected passkeys, got {other:?}"),
    }
    assert_eq!(db.passkeys_for_owner("other").expect("list").len(), 1);

    send(
        &mut client,
        &SignalMessage::DeletePasskey {
            id: "mine".to_string(),
        },
    )
    .await;
    match recv(&mut client).await {
        SignalMessage::Passkeys { passkeys } => assert!(passkeys.is_empty()),
        other => panic!("expected passkeys, got {other:?}"),
    }
}

/// True once the stream is over (closed, errored, or a Close frame).
async fn stream_ended(ws: &mut WsStream, dur: Duration) -> bool {
    timeout(dur, async {
        loop {
            match ws.next().await {
                None | Some(Err(_)) | Some(Ok(Message::Close(_))) => return,
                Some(Ok(_)) => continue,
            }
        }
    })
    .await
    .is_ok()
}

// B1: an oversized frame closes the connection; the server stays alive.
#[tokio::test]
async fn oversized_message_closes_the_connection() {
    let url = spawn_server().await;
    let mut bad = connect(&url).await;
    hello(&mut bad, Role::Client).await;
    let big = serde_json::to_string(&SignalMessage::Join {
        pin: "0".repeat(MAX_MESSAGE_BYTES + 1024),
    })
    .expect("serialize");
    // The server may close while we are still writing; ignore send errors.
    let _ = timeout(TIMEOUT, bad.send(Message::Text(big.into()))).await;
    assert!(
        stream_ended(&mut bad, TIMEOUT).await,
        "connection must be closed after an oversized message"
    );

    let mut good = connect(&url).await;
    hello(&mut good, Role::Client).await;
    send(
        &mut good,
        &SignalMessage::Join {
            pin: "000000".to_string(),
        },
    )
    .await;
    match recv(&mut good).await {
        SignalMessage::Error { message } => assert_eq!(message, "unknown pin"),
        other => panic!("expected error, got {other:?}"),
    }
}

// B1: host name and device alias are cut to 64 chars.
#[tokio::test]
async fn host_name_and_alias_are_truncated_to_64_chars() {
    let url = spawn_server().await;
    let mut host = connect(&url).await;
    hello(&mut host, Role::Host).await;
    send(
        &mut host,
        &SignalMessage::HostRegister {
            name: "й".repeat(100),
            device: None,
            session_id: None,
            pin_rotation: false,
        },
    )
    .await;
    let (host_id, pin) = match recv(&mut host).await {
        SignalMessage::Registered { host_id, pin, .. } => (host_id, pin),
        other => panic!("expected registered, got {other:?}"),
    };

    let mut client = connect(&url).await;
    hello(&mut client, Role::Client).await;
    client_auth(&mut client, None).await;
    send(&mut client, &SignalMessage::Join { pin }).await;
    match recv(&mut client).await {
        SignalMessage::Joined { host_name, .. } => assert_eq!(host_name.chars().count(), 64),
        other => panic!("expected joined, got {other:?}"),
    }
    match recv(&mut host).await {
        SignalMessage::PeerJoined { .. } => {}
        other => panic!("expected peer_joined, got {other:?}"),
    }

    send(
        &mut client,
        &SignalMessage::RenameDevice {
            device_id: host_id,
            alias: Some("я".repeat(100)),
        },
    )
    .await;
    match recv(&mut client).await {
        SignalMessage::Devices { devices } => {
            assert_eq!(devices.len(), 1);
            assert_eq!(
                devices[0].alias.as_deref().map(|a| a.chars().count()),
                Some(64)
            );
        }
        other => panic!("expected devices, got {other:?}"),
    }
}

// B1: a peer that stops reading overflows its outbound queue and is
// disconnected; the host is freed for the next client. Multi-threaded
// runtime: the flooding host gets one `not in session` reply per message
// once the client is gone, and its writer must drain them concurrently --
// on the current-thread runtime it starved and the host itself got killed.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn peer_that_stops_reading_is_disconnected() {
    let url = spawn_server().await;
    let (mut host, mut client, session_id, pin) = host_and_joined_client(&url).await;

    // The client does not read. The host floods it with large ICE candidates.
    for _ in 0..600 {
        let msg = SignalMessage::Ice {
            session_id: session_id.clone(),
            candidate: IceCandidate {
                candidate: "x".repeat(60 * 1024),
                sdp_mid: None,
                sdp_mline_index: None,
            },
        };
        let json = serde_json::to_string(&msg).expect("serialize");
        if timeout(TIMEOUT, host.send(Message::Text(json.into())))
            .await
            .map(|r| r.is_err())
            .unwrap_or(true)
        {
            break;
        }
    }

    // Now the client starts reading and discarding: the stream must end.
    assert!(
        stream_ended(&mut client, Duration::from_secs(15)).await,
        "non-reading client must be disconnected"
    );

    // The host is free again: a second client gets Joined, not "host busy".
    let mut second = connect(&url).await;
    hello(&mut second, Role::Client).await;
    let mut joined = false;
    for _ in 0..20 {
        send(&mut second, &SignalMessage::Join { pin: pin.clone() }).await;
        match recv(&mut second).await {
            SignalMessage::Joined { .. } => {
                joined = true;
                break;
            }
            SignalMessage::Error { message } if message == "host busy" => {
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
            other => panic!("unexpected {other:?}"),
        }
    }
    assert!(joined, "host must be free after the slow client is dropped");
}

// B2 (audit S-H2): creating new owners is rate-limited per client IP; known
// tokens are free, another IP is unaffected.
#[tokio::test]
async fn new_owner_creation_is_rate_limited_per_ip() {
    let url = spawn_server().await;

    let mut ws = connect_as(&url, "203.0.113.50").await;
    hello(&mut ws, Role::Client).await;
    let mut tokens = Vec::new();
    for _ in 0..NEW_OWNER_BURST {
        let (token, _devices) = client_auth(&mut ws, None).await;
        tokens.push(token);
    }
    send(&mut ws, &SignalMessage::ClientAuth { token: None }).await;
    match recv(&mut ws).await {
        SignalMessage::Error { message } => assert_eq!(message, "too many attempts"),
        other => panic!("expected too many attempts, got {other:?}"),
    }

    let (token, _devices) = client_auth(&mut ws, Some(tokens[0].clone())).await;
    assert_eq!(token, tokens[0]);

    let mut other = connect_as(&url, "203.0.113.51").await;
    hello(&mut other, Role::Client).await;
    client_auth(&mut other, None).await;
}

/// Registers a host with `device` over a fresh connection from `ip`.
/// Returns the connection and the first response.
async fn register_host_as(
    url: &str,
    ip: &str,
    device: Option<DeviceCredentials>,
) -> (WsStream, SignalMessage) {
    let mut host = connect_as(url, ip).await;
    hello(&mut host, Role::Host).await;
    send(
        &mut host,
        &SignalMessage::HostRegister {
            name: "Test Host".to_string(),
            device,
            session_id: None,
            pin_rotation: false,
        },
    )
    .await;
    let response = recv(&mut host).await;
    (host, response)
}

// B2 (audit S-H2): registering new devices is rate-limited per client IP;
// known devices are free, another IP is unaffected.
#[tokio::test]
async fn new_device_registration_is_rate_limited_per_ip() {
    let url = spawn_server().await;
    let ip = "203.0.113.60";

    let mut credentials = None;
    for _ in 0..NEW_DEVICE_BURST {
        let (mut host, response) = register_host_as(&url, ip, None).await;
        match response {
            SignalMessage::Registered { device, .. } => {
                credentials = Some(device.expect("issued credentials"));
            }
            other => panic!("expected registered, got {other:?}"),
        }
        host.close(None).await.expect("close");
    }

    let (mut blocked, response) = register_host_as(&url, ip, None).await;
    match response {
        SignalMessage::Error { message } => assert_eq!(message, "too many attempts"),
        other => panic!("expected too many attempts, got {other:?}"),
    }
    let end = timeout(TIMEOUT, async {
        loop {
            match blocked.next().await {
                None | Some(Err(_)) | Some(Ok(Message::Close(_))) => break,
                Some(Ok(_)) => continue,
            }
        }
    })
    .await;
    assert!(end.is_ok(), "stream must end after the rate-limit error");

    // A known device from the same IP is not limited.
    let (_known, response) = register_host_as(&url, ip, credentials).await;
    match response {
        SignalMessage::Registered { device, .. } => assert!(device.is_none()),
        other => panic!("expected registered, got {other:?}"),
    }

    let (_other, response) = register_host_as(&url, "203.0.113.61", None).await;
    assert!(matches!(response, SignalMessage::Registered { .. }));
}

// B3: TURN credentials are not handed out in `Registered`, only per session.
#[tokio::test]
async fn registered_has_only_stun_but_joined_and_peer_joined_have_turn() {
    let ice = IceConfig::new(
        vec!["stun:stun.example:3478".into()],
        vec!["turn:turn.example:3478".into()],
        "secret".into(),
        3600,
    );
    let url = spawn_server_with(ice, Liveness::default()).await;
    let mut host = connect(&url).await;
    hello(&mut host, Role::Host).await;
    send(
        &mut host,
        &SignalMessage::HostRegister {
            name: "Test Host".to_string(),
            device: None,
            session_id: None,
            pin_rotation: false,
        },
    )
    .await;
    let pin = match recv(&mut host).await {
        SignalMessage::Registered {
            pin, ice_servers, ..
        } => {
            assert!(!ice_servers.is_empty());
            assert!(ice_servers.iter().all(|s| s.username.is_none()));
            pin
        }
        other => panic!("expected registered, got {other:?}"),
    };

    let mut client = connect(&url).await;
    hello(&mut client, Role::Client).await;
    send(&mut client, &SignalMessage::Join { pin }).await;
    match recv(&mut client).await {
        SignalMessage::Joined { ice_servers, .. } => {
            assert!(ice_servers.iter().any(|s| s.username.is_some()));
        }
        other => panic!("expected joined, got {other:?}"),
    }
    match recv(&mut host).await {
        SignalMessage::PeerJoined { ice_servers, .. } => {
            assert!(ice_servers.iter().any(|s| s.username.is_some()));
        }
        other => panic!("expected peer_joined, got {other:?}"),
    }
}

// B3: the server pings an idle connection.
#[tokio::test]
async fn server_pings_idle_connection() {
    let liveness = Liveness {
        ping_interval: Duration::from_millis(100),
        read_timeout: Duration::from_secs(5),
        ..Default::default()
    };
    let url = spawn_server_with(IceConfig::from_env(), liveness).await;
    let mut ws = connect(&url).await;
    hello(&mut ws, Role::Client).await;
    let got_ping = timeout(Duration::from_secs(1), async {
        loop {
            match ws.next().await {
                Some(Ok(Message::Ping(_))) => return true,
                Some(Ok(_)) => continue,
                _ => return false,
            }
        }
    })
    .await
    .expect("no ping within 1s");
    assert!(got_ping);
}

// B3: a peer that sends nothing (and does not answer pings) is dropped after
// the read timeout; a peer that answers pings stays alive.
#[tokio::test]
async fn silent_peer_is_disconnected_after_read_timeout() {
    let liveness = Liveness {
        ping_interval: Duration::from_millis(100),
        read_timeout: Duration::from_millis(400),
        ..Default::default()
    };
    let url = spawn_server_with(IceConfig::from_env(), liveness).await;

    let mut host = connect(&url).await;
    hello(&mut host, Role::Host).await;
    send(
        &mut host,
        &SignalMessage::HostRegister {
            name: "Silent".to_string(),
            device: None,
            session_id: None,
            pin_rotation: false,
        },
    )
    .await;
    match recv(&mut host).await {
        SignalMessage::Registered { .. } => {}
        other => panic!("expected registered, got {other:?}"),
    }
    // Not reading: no automatic Pong goes out.
    tokio::time::sleep(Duration::from_secs(1)).await;
    assert!(
        stream_ended(&mut host, TIMEOUT).await,
        "silent connection must be closed after the read timeout"
    );

    // Control: a client that keeps reading (and so answers Pings) survives.
    let mut live = connect(&url).await;
    hello(&mut live, Role::Client).await;
    let _ = timeout(Duration::from_secs(1), async {
        loop {
            if live.next().await.is_none() {
                return;
            }
        }
    })
    .await;
    send(
        &mut live,
        &SignalMessage::Join {
            pin: "000000".to_string(),
        },
    )
    .await;
    match recv(&mut live).await {
        SignalMessage::Error { message } => assert_eq!(message, "unknown pin"),
        other => panic!("expected error, got {other:?}"),
    }
}

/// Registers a host (optionally opted into PIN rotation); returns the socket
/// and its first PIN.
async fn register_host_with_rotation(url: &str, pin_rotation: bool) -> (WsStream, String) {
    let mut host = connect(url).await;
    hello(&mut host, Role::Host).await;
    send(
        &mut host,
        &SignalMessage::HostRegister {
            name: "Test Host".to_string(),
            device: None,
            session_id: None,
            pin_rotation,
        },
    )
    .await;
    let pin = match recv(&mut host).await {
        SignalMessage::Registered { pin, .. } => pin,
        other => panic!("expected registered, got {other:?}"),
    };
    (host, pin)
}

async fn join_expecting_error(url: &str, pin: &str, expected: &str) {
    let mut client = connect(url).await;
    hello(&mut client, Role::Client).await;
    send(
        &mut client,
        &SignalMessage::Join {
            pin: pin.to_string(),
        },
    )
    .await;
    match recv(&mut client).await {
        SignalMessage::Error { message } => assert_eq!(message, expected),
        other => panic!("expected error, got {other:?}"),
    }
}

// D44: a host that opted in gets a fresh PIN after every successful join.
#[tokio::test]
async fn join_rotates_pin_for_host_that_opted_in() {
    let url = spawn_server().await;
    let (mut host, old) = register_host_with_rotation(&url, true).await;

    let mut client = connect(&url).await;
    hello(&mut client, Role::Client).await;
    send(&mut client, &SignalMessage::Join { pin: old.clone() }).await;
    let session_id = match recv(&mut client).await {
        SignalMessage::Joined { session_id, .. } => session_id,
        other => panic!("expected joined, got {other:?}"),
    };
    match recv(&mut host).await {
        SignalMessage::PeerJoined { .. } => {}
        other => panic!("expected peer_joined, got {other:?}"),
    }
    let new = match recv(&mut host).await {
        SignalMessage::PinChanged { pin } => pin,
        other => panic!("expected pin_changed, got {other:?}"),
    };
    assert_ne!(new, old);
    assert_eq!(new.len(), 6);

    join_expecting_error(&url, &old, "unknown pin").await;

    send(
        &mut host,
        &SignalMessage::Bye {
            session_id,
            reason: None,
        },
    )
    .await;
    match recv(&mut client).await {
        SignalMessage::Bye { .. } => {}
        other => panic!("expected bye, got {other:?}"),
    }
    let mut client2 = connect(&url).await;
    hello(&mut client2, Role::Client).await;
    send(&mut client2, &SignalMessage::Join { pin: new }).await;
    match recv(&mut client2).await {
        SignalMessage::Joined { .. } => {}
        other => panic!("expected joined, got {other:?}"),
    }
}

// D44: an older host (no `pin_rotation`) keeps a static PIN.
#[tokio::test]
async fn join_keeps_pin_static_for_host_without_rotation() {
    let url = spawn_server().await;
    let (mut host, pin) = register_host_with_rotation(&url, false).await;

    let mut client = connect(&url).await;
    hello(&mut client, Role::Client).await;
    send(&mut client, &SignalMessage::Join { pin: pin.clone() }).await;
    let session_id = match recv(&mut client).await {
        SignalMessage::Joined { session_id, .. } => session_id,
        other => panic!("expected joined, got {other:?}"),
    };
    match recv(&mut host).await {
        SignalMessage::PeerJoined { .. } => {}
        other => panic!("expected peer_joined, got {other:?}"),
    }
    expect_silence(&mut host, SILENCE_TIMEOUT).await;

    send(
        &mut host,
        &SignalMessage::Bye {
            session_id,
            reason: None,
        },
    )
    .await;
    match recv(&mut client).await {
        SignalMessage::Bye { .. } => {}
        other => panic!("expected bye, got {other:?}"),
    }
    let mut client2 = connect(&url).await;
    hello(&mut client2, Role::Client).await;
    send(&mut client2, &SignalMessage::Join { pin }).await;
    match recv(&mut client2).await {
        SignalMessage::Joined { .. } => {}
        other => panic!("expected joined, got {other:?}"),
    }
}

// D44: an opted-in host's PIN is rotated by TTL, with `PinChanged`.
#[tokio::test]
async fn pin_expires_after_ttl() {
    // 1 s: the fresh PIN must still be valid by the time the client below
    // joins with it, and `PinChanged` must arrive within `recv`'s 2 s.
    let url = spawn_server_with(
        IceConfig::from_env(),
        Liveness {
            pin_ttl: Duration::from_millis(1000),
            ..Default::default()
        },
    )
    .await;
    let (mut host, old) = register_host_with_rotation(&url, true).await;
    let new = match recv(&mut host).await {
        SignalMessage::PinChanged { pin } => pin,
        other => panic!("expected pin_changed, got {other:?}"),
    };
    assert_ne!(new, old);
    assert_eq!(new.len(), 6);

    join_expecting_error(&url, &old, "unknown pin").await;

    let mut client = connect(&url).await;
    hello(&mut client, Role::Client).await;
    send(&mut client, &SignalMessage::Join { pin: new }).await;
    match recv(&mut client).await {
        SignalMessage::Joined { .. } => {}
        other => panic!("expected joined, got {other:?}"),
    }
}

#[tokio::test]
async fn pin_ttl_does_not_touch_host_without_rotation() {
    let url = spawn_server_with(
        IceConfig::from_env(),
        Liveness {
            pin_ttl: Duration::from_millis(500),
            ..Default::default()
        },
    )
    .await;
    let (mut host, _pin) = register_host_with_rotation(&url, false).await;
    expect_silence(&mut host, Duration::from_millis(1200)).await;
}

async fn join_and_expect(ws: &mut WsStream, pin: &str, expected: &str) {
    send(
        ws,
        &SignalMessage::Join {
            pin: pin.to_string(),
        },
    )
    .await;
    match recv(ws).await {
        SignalMessage::Error { message } => assert_eq!(message, expected),
        other => panic!("expected {expected}, got {other:?}"),
    }
}

// D2 (audit S-H4): `host busy` confirms a correct PIN, so it spends an attempt.
#[tokio::test]
async fn host_busy_spends_a_pin_attempt() {
    let url = spawn_server().await;
    let (_host, pin) = register_host_with_rotation(&url, false).await;

    let mut a = connect(&url).await;
    hello(&mut a, Role::Client).await;
    send(&mut a, &SignalMessage::Join { pin: pin.clone() }).await;
    match recv(&mut a).await {
        SignalMessage::Joined { .. } => {}
        other => panic!("expected joined, got {other:?}"),
    }

    let mut attacker = connect_as(&url, "198.51.100.10").await;
    hello(&mut attacker, Role::Client).await;
    for _ in 0..10 {
        join_and_expect(&mut attacker, &pin, "host busy").await;
    }
    join_and_expect(&mut attacker, &pin, "too many attempts").await;
}

// D2: `already in session` also confirms a correct PIN.
#[tokio::test]
async fn already_in_session_spends_a_pin_attempt() {
    let url = spawn_server().await;
    let (_host1, pin1) = register_host_with_rotation(&url, false).await;
    let (_host2, pin2) = register_host_with_rotation(&url, false).await;

    let mut client = connect_as(&url, "198.51.100.11").await;
    hello(&mut client, Role::Client).await;
    send(&mut client, &SignalMessage::Join { pin: pin1 }).await;
    match recv(&mut client).await {
        SignalMessage::Joined { .. } => {}
        other => panic!("expected joined, got {other:?}"),
    }
    for _ in 0..10 {
        join_and_expect(&mut client, &pin2, "already in session").await;
    }
    join_and_expect(&mut client, &pin2, "too many attempts").await;
}

// D2: IPv6 clients share one bucket per /64.
#[tokio::test]
async fn ipv6_prefix_shares_the_pin_bucket() {
    let url = spawn_server().await;

    let mut attacker = connect_as(&url, "2001:db8:1:2::1").await;
    hello(&mut attacker, Role::Client).await;
    for _ in 0..10 {
        join_and_expect(&mut attacker, "000000", "unknown pin").await;
    }

    let mut same_prefix = connect_as(&url, "2001:db8:1:2:ffff::9").await;
    hello(&mut same_prefix, Role::Client).await;
    join_and_expect(&mut same_prefix, "000000", "too many attempts").await;

    let mut other_prefix = connect_as(&url, "2001:db8:1:3::1").await;
    hello(&mut other_prefix, Role::Client).await;
    join_and_expect(&mut other_prefix, "000000", "unknown pin").await;
}

// D2: the global bucket stops distributed guessing; it is shared, so even a
// legitimate client with a correct PIN is refused once it is drained.
#[tokio::test]
async fn global_pin_bucket_limits_distributed_guessing() {
    let global = rcdesk_server::ip_limit::GlobalLimiter::new(5, Duration::from_secs(3600));
    let (url, _db) =
        spawn_full_with_global(IceConfig::from_env(), Liveness::default(), global).await;
    let (_host, pin) = register_host_with_rotation(&url, false).await;

    let mut attackers = Vec::new();
    for last in 21..=23 {
        let mut ws = connect_as(&url, &format!("198.51.100.{last}")).await;
        hello(&mut ws, Role::Client).await;
        attackers.push(ws);
    }
    // Attempts 1..=5 spend the global bucket; the 6th (third IP, own limit
    // intact) is refused globally.
    join_and_expect(&mut attackers[0], "000000", "unknown pin").await;
    join_and_expect(&mut attackers[0], "000000", "unknown pin").await;
    join_and_expect(&mut attackers[1], "000000", "unknown pin").await;
    join_and_expect(&mut attackers[1], "000000", "unknown pin").await;
    join_and_expect(&mut attackers[2], "000000", "unknown pin").await;
    join_and_expect(&mut attackers[2], "000000", "too many attempts").await;

    let mut legit = connect_as(&url, "198.51.100.24").await;
    hello(&mut legit, Role::Client).await;
    join_and_expect(&mut legit, &pin, "too many attempts").await;
}

// ---- Slice 3.8a: accounts "email + password" ----

/// Fake mailer recording every `(to, code)` it is asked to send.
#[derive(Clone, Default)]
struct RecordingMailer {
    sent: std::sync::Arc<std::sync::Mutex<Vec<(String, String)>>>,
    /// `(old address, new email)` of every email-changed notice.
    changed: std::sync::Arc<std::sync::Mutex<Vec<(String, String)>>>,
}

impl rcdesk_server::mail::Mailer for RecordingMailer {
    fn send_code(
        &self,
        to: &str,
        code: &str,
    ) -> futures_util::future::BoxFuture<'static, anyhow::Result<()>> {
        self.sent
            .lock()
            .expect("mailer mutex")
            .push((to.to_string(), code.to_string()));
        Box::pin(async { Ok(()) })
    }

    fn send_email_changed(
        &self,
        to: &str,
        new_email: &str,
    ) -> futures_util::future::BoxFuture<'static, anyhow::Result<()>> {
        self.changed
            .lock()
            .expect("mailer mutex")
            .push((to.to_string(), new_email.to_string()));
        Box::pin(async { Ok(()) })
    }
}

impl RecordingMailer {
    fn last_code(&self, to: &str) -> String {
        self.sent
            .lock()
            .expect("mailer mutex")
            .iter()
            .rev()
            .find(|(addr, _)| addr == to)
            .map(|(_, code)| code.clone())
            .expect("a code was sent to this address")
    }
}

async fn spawn_with_recording_mailer() -> (String, Db, RecordingMailer) {
    let mailer = RecordingMailer::default();
    let (url, db) = spawn_with_mailer(
        IceConfig::from_env(),
        Liveness::default(),
        rcdesk_server::ip_limit::GlobalLimiter::new(
            rcdesk_server::app::PIN_GLOBAL_BURST,
            rcdesk_server::app::PIN_GLOBAL_REFILL,
        ),
        std::sync::Arc::new(mailer.clone()),
    )
    .await;
    (url, db, mailer)
}

async fn client_conn(url: &str) -> WsStream {
    let mut ws = connect(url).await;
    hello(&mut ws, Role::Client).await;
    ws
}

fn owner_of_token(db: &Db, token: &str) -> Option<String> {
    db.token_by_hash(&rcdesk_server::devices::hash_secret(token))
        .expect("token lookup")
        .map(|(owner, _)| owner)
}

fn link_new_device(db: &Db, owner: &str, device_id: &str) {
    db.insert_device(device_id, "h", device_id, 1)
        .expect("device");
    db.link_device(owner, device_id, 2).expect("link");
}

async fn request_code(ws: &mut WsStream, mailer: &RecordingMailer, email: &str) -> String {
    send(
        ws,
        &SignalMessage::EmailCodeRequest {
            email: email.to_string(),
        },
    )
    .await;
    assert_eq!(recv(ws).await, SignalMessage::EmailCodeSent);
    mailer.last_code(email)
}

async fn confirm(ws: &mut WsStream, email: &str, code: &str, password: &str) {
    confirm_with(ws, email, code, password, None).await;
}

async fn confirm_with(
    ws: &mut WsStream,
    email: &str,
    code: &str,
    password: &str,
    purpose: Option<proto::signal::EmailCodePurpose>,
) {
    send(
        ws,
        &SignalMessage::EmailCodeConfirm {
            email: email.to_string(),
            code: code.to_string(),
            password: password.to_string(),
            purpose,
        },
    )
    .await;
}

/// Sends a hand-written JSON frame (an old client's wire shape).
async fn send_raw_json(ws: &mut WsStream, json: &str) {
    timeout(TIMEOUT, ws.send(Message::Text(json.to_string().into())))
        .await
        .expect("send timed out")
        .expect("send failed");
}

async fn password_login(ws: &mut WsStream, email: &str, password: &str) {
    send(
        ws,
        &SignalMessage::PasswordLogin {
            email: email.to_string(),
            password: password.to_string(),
        },
    )
    .await;
}

/// Reads `Authenticated` + `Account`, returns the token, devices and email.
async fn expect_signed_in(ws: &mut WsStream) -> (String, Vec<proto::signal::DeviceEntry>, String) {
    let (token, devices) = match recv(ws).await {
        SignalMessage::Authenticated { token, devices } => (token, devices),
        other => panic!("expected authenticated, got {other:?}"),
    };
    let email = match recv(ws).await {
        SignalMessage::Account { email: Some(email) } => email,
        other => panic!("expected account with email, got {other:?}"),
    };
    (token, devices, email)
}

#[tokio::test]
async fn registration_binds_email_to_the_current_owner_and_password_login_finds_it() {
    let (url, db, mailer) = spawn_with_recording_mailer().await;
    let mut a = client_conn(&url).await;
    let (token_a, _) = client_auth(&mut a, None).await;
    let owner = owner_of_token(&db, &token_a).expect("owner");
    link_new_device(&db, &owner, "dev1");

    send(&mut a, &SignalMessage::GetAccount).await;
    assert_eq!(recv(&mut a).await, SignalMessage::Account { email: None });

    let code = request_code(&mut a, &mailer, "bob@example.com").await;
    confirm(&mut a, " Bob@Example.com ", &code, "hunter22-pass").await;
    let (token, devices, email) = expect_signed_in(&mut a).await;
    assert_eq!(token, token_a, "registration keeps the connection's token");
    assert_eq!(email, "bob@example.com");
    assert_eq!(devices.len(), 1);
    assert_eq!(devices[0].device_id, "dev1");
    assert_eq!(owner_of_token(&db, &token).as_deref(), Some(owner.as_str()));

    send(&mut a, &SignalMessage::GetAccount).await;
    assert_eq!(
        recv(&mut a).await,
        SignalMessage::Account {
            email: Some("bob@example.com".to_string())
        }
    );

    // Another browser signs in with the password and sees the same owner.
    let mut b = client_conn(&url).await;
    password_login(&mut b, "BOB@example.com", "hunter22-pass").await;
    let (token_b, devices_b, email_b) = expect_signed_in(&mut b).await;
    assert_eq!(email_b, "bob@example.com");
    assert_eq!(devices_b.len(), 1);
    assert_eq!(devices_b[0].device_id, "dev1");
    assert_ne!(token_b, token_a);
    assert_eq!(
        owner_of_token(&db, &token_b).as_deref(),
        Some(owner.as_str())
    );
}

#[tokio::test]
async fn password_reset_merges_anonymous_devices_and_revokes_old_sessions() {
    let (url, db, mailer) = spawn_with_recording_mailer().await;
    // An existing account with a device, one signed-in browser and a password.
    let (account, _token_id, old_token) =
        rcdesk_server::owners::create_owner(&db, "UA", 1).expect("owner");
    link_new_device(&db, &account, "dev-account");
    let old_hash = rcdesk_server::accounts::hash_password("old-password-1").expect("hash");
    assert!(db
        .insert_account(&account, "bob@example.com", &old_hash, 1)
        .expect("account"));

    // An anonymous connection with its own device resets the password.
    let mut anon = client_conn(&url).await;
    let (anon_token, _) = client_auth(&mut anon, None).await;
    let anon_owner = owner_of_token(&db, &anon_token).expect("anon owner");
    link_new_device(&db, &anon_owner, "dev-anon");

    let code = request_code(&mut anon, &mailer, "bob@example.com").await;
    confirm(&mut anon, "bob@example.com", &code, "new-password-2").await;
    let (new_token, devices, email) = expect_signed_in(&mut anon).await;
    assert_eq!(email, "bob@example.com");
    assert_eq!(
        owner_of_token(&db, &new_token).as_deref(),
        Some(account.as_str())
    );
    let mut ids: Vec<_> = devices.iter().map(|d| d.device_id.as_str()).collect();
    ids.sort_unstable();
    assert_eq!(ids, vec!["dev-account", "dev-anon"]);

    // The old browser token no longer works: a fresh anonymous owner appears.
    assert_eq!(owner_of_token(&db, &old_token), None);
    let mut stale = client_conn(&url).await;
    let (fresh_token, fresh_devices) = client_auth(&mut stale, Some(old_token.clone())).await;
    assert_ne!(fresh_token, old_token);
    assert!(fresh_devices.is_empty());

    // Old password is rejected, new one works.
    let mut login = client_conn(&url).await;
    password_login(&mut login, "bob@example.com", "old-password-1").await;
    expect_error(&mut login, "wrong email or password").await;
    password_login(&mut login, "bob@example.com", "new-password-2").await;
    let (_, devices, _) = expect_signed_in(&mut login).await;
    assert_eq!(devices.len(), 2);
}

#[tokio::test]
async fn wrong_password_is_generic_and_ten_failures_lock_the_ip() {
    let (url, db, _mailer) = spawn_with_recording_mailer().await;
    let (account, _, _) = rcdesk_server::owners::create_owner(&db, "UA", 1).expect("owner");
    let hash = rcdesk_server::accounts::hash_password("right-password").expect("hash");
    db.insert_account(&account, "bob@example.com", &hash, 1)
        .expect("account");

    let mut ws = client_conn(&url).await;
    // Unknown address and malformed address give the same answer.
    password_login(&mut ws, "nobody@example.com", "whatever-pass").await;
    expect_error(&mut ws, "wrong email or password").await;
    password_login(&mut ws, "not an email", "whatever-pass").await;
    expect_error(&mut ws, "wrong email or password").await;
    for _ in 0..8 {
        password_login(&mut ws, "bob@example.com", "wrong-password").await;
        expect_error(&mut ws, "wrong email or password").await;
    }
    // Ten failures spent; even the right password is refused now.
    password_login(&mut ws, "bob@example.com", "right-password").await;
    expect_error(&mut ws, "too many attempts").await;
}

#[tokio::test]
async fn successful_logins_do_not_spend_the_login_limit() {
    let (url, db, _mailer) = spawn_with_recording_mailer().await;
    let (account, _, _) = rcdesk_server::owners::create_owner(&db, "UA", 1).expect("owner");
    let hash = rcdesk_server::accounts::hash_password("right-password").expect("hash");
    db.insert_account(&account, "bob@example.com", &hash, 1)
        .expect("account");

    let mut ws = client_conn(&url).await;
    for _ in 0..12 {
        password_login(&mut ws, "bob@example.com", "right-password").await;
        expect_signed_in(&mut ws).await;
    }
}

#[tokio::test]
async fn wrong_code_weak_password_and_invalid_email_are_rejected() {
    let (url, _db, mailer) = spawn_with_recording_mailer().await;
    let mut ws = client_conn(&url).await;
    client_auth(&mut ws, None).await;

    send(
        &mut ws,
        &SignalMessage::EmailCodeRequest {
            email: "nope".to_string(),
        },
    )
    .await;
    expect_error(&mut ws, "invalid email").await;
    confirm(&mut ws, "nope", "123456", "long-enough-pass").await;
    expect_error(&mut ws, "invalid email").await;

    let code = request_code(&mut ws, &mailer, "bob@example.com").await;
    let wrong = if code == "000000" { "111111" } else { "000000" };
    confirm(&mut ws, "bob@example.com", wrong, "long-enough-pass").await;
    expect_error(&mut ws, "wrong code").await;

    // A weak password does not burn a code attempt: the real code still works.
    confirm(&mut ws, "bob@example.com", &code, "short").await;
    expect_error(&mut ws, "weak password").await;
    confirm(&mut ws, "bob@example.com", &code, "long-enough-pass").await;
    expect_signed_in(&mut ws).await;
}

#[tokio::test]
async fn second_code_request_within_a_minute_is_refused() {
    let (url, _db, mailer) = spawn_with_recording_mailer().await;
    let mut ws = client_conn(&url).await;
    request_code(&mut ws, &mailer, "bob@example.com").await;
    send(
        &mut ws,
        &SignalMessage::EmailCodeRequest {
            email: "bob@example.com".to_string(),
        },
    )
    .await;
    expect_error(&mut ws, "too many attempts").await;
}

#[tokio::test]
async fn disabled_mailer_reports_email_unavailable() {
    let url = spawn_server().await;
    let mut ws = client_conn(&url).await;
    client_auth(&mut ws, None).await;
    send(
        &mut ws,
        &SignalMessage::EmailCodeRequest {
            email: "bob@example.com".to_string(),
        },
    )
    .await;
    expect_error(&mut ws, "email unavailable").await;
}

#[tokio::test]
async fn owner_with_an_account_cannot_register_another_email() {
    let (url, db, mailer) = spawn_with_recording_mailer().await;
    let mut ws = client_conn(&url).await;
    let (token, _) = client_auth(&mut ws, None).await;
    let owner = owner_of_token(&db, &token).expect("owner");
    let hash = rcdesk_server::accounts::hash_password("right-password").expect("hash");
    db.insert_account(&owner, "bob@example.com", &hash, 1)
        .expect("account");

    let code = request_code(&mut ws, &mailer, "other@example.com").await;
    confirm(&mut ws, "other@example.com", &code, "long-enough-pass").await;
    expect_error(&mut ws, "already registered").await;
}

#[tokio::test]
async fn registration_without_an_owner_is_not_authenticated_and_get_account_too() {
    let (url, _db, mailer) = spawn_with_recording_mailer().await;
    let mut ws = client_conn(&url).await;
    send(&mut ws, &SignalMessage::GetAccount).await;
    expect_not_authenticated(&mut ws).await;
    let code = request_code(&mut ws, &mailer, "bob@example.com").await;
    confirm(&mut ws, "bob@example.com", &code, "long-enough-pass").await;
    expect_not_authenticated(&mut ws).await;
}

// ---- EmailCodeConfirm purpose ----

#[tokio::test]
async fn reset_purpose_without_an_account_is_refused_and_creates_nothing() {
    use proto::signal::EmailCodePurpose;
    let (url, db, mailer) = spawn_with_recording_mailer().await;
    let mut ws = client_conn(&url).await;
    let (token, _) = client_auth(&mut ws, None).await;
    let owner = owner_of_token(&db, &token).expect("owner");

    let code = request_code(&mut ws, &mailer, "ghost@example.com").await;
    confirm_with(
        &mut ws,
        "ghost@example.com",
        &code,
        "long-enough-pass",
        Some(EmailCodePurpose::Reset),
    )
    .await;
    expect_error(&mut ws, "no such account").await;
    assert!(db
        .account_by_email("ghost@example.com")
        .expect("lookup")
        .is_none());
    assert_eq!(db.email_for_owner(&owner).expect("email"), None);
}

#[tokio::test]
async fn register_purpose_with_a_taken_email_is_refused_and_keeps_the_other_password() {
    use proto::signal::EmailCodePurpose;
    let (url, db, mailer) = spawn_with_recording_mailer().await;
    let (other_owner, _, _) = rcdesk_server::owners::create_owner(&db, "UA", 1).expect("owner");
    let other_hash = rcdesk_server::accounts::hash_password("old-password-1").expect("hash");
    assert!(db
        .insert_account(&other_owner, "bob@example.com", &other_hash, 1)
        .expect("account"));

    let mut ws = client_conn(&url).await;
    let (token, _) = client_auth(&mut ws, None).await;
    let owner = owner_of_token(&db, &token).expect("owner");
    let code = request_code(&mut ws, &mailer, "bob@example.com").await;
    confirm_with(
        &mut ws,
        "bob@example.com",
        &code,
        "new-password-2",
        Some(EmailCodePurpose::Register),
    )
    .await;
    expect_error(&mut ws, "email taken").await;
    assert_eq!(db.email_for_owner(&owner).expect("email"), None);

    // The other account's password is untouched and the new one does not work.
    let mut login = client_conn(&url).await;
    password_login(&mut login, "bob@example.com", "new-password-2").await;
    expect_error(&mut login, "wrong email or password").await;
    password_login(&mut login, "bob@example.com", "old-password-1").await;
    let (_, _, email) = expect_signed_in(&mut login).await;
    assert_eq!(email, "bob@example.com");
}

#[tokio::test]
async fn explicit_register_purpose_registers_on_a_free_email() {
    use proto::signal::EmailCodePurpose;
    let (url, db, mailer) = spawn_with_recording_mailer().await;
    let mut ws = client_conn(&url).await;
    let (token, _) = client_auth(&mut ws, None).await;
    let owner = owner_of_token(&db, &token).expect("owner");
    let code = request_code(&mut ws, &mailer, "bob@example.com").await;
    confirm_with(
        &mut ws,
        "bob@example.com",
        &code,
        "hunter22-pass",
        Some(EmailCodePurpose::Register),
    )
    .await;
    let (_, _, email) = expect_signed_in(&mut ws).await;
    assert_eq!(email, "bob@example.com");
    assert_eq!(
        db.email_for_owner(&owner).expect("email").as_deref(),
        Some("bob@example.com")
    );
}

#[tokio::test]
async fn explicit_reset_purpose_resets_an_existing_account() {
    use proto::signal::EmailCodePurpose;
    let (url, db, mailer) = spawn_with_recording_mailer().await;
    let (acc, _, _) = rcdesk_server::owners::create_owner(&db, "UA", 1).expect("owner");
    let hash = rcdesk_server::accounts::hash_password("old-password-1").expect("hash");
    assert!(db
        .insert_account(&acc, "carol@example.com", &hash, 1)
        .expect("account"));
    let mut ws = client_conn(&url).await;
    client_auth(&mut ws, None).await;
    let code = request_code(&mut ws, &mailer, "carol@example.com").await;
    confirm_with(
        &mut ws,
        "carol@example.com",
        &code,
        "new-password-2",
        Some(EmailCodePurpose::Reset),
    )
    .await;
    let (_, _, email) = expect_signed_in(&mut ws).await;
    assert_eq!(email, "carol@example.com");
    let mut login = client_conn(&url).await;
    password_login(&mut login, "carol@example.com", "new-password-2").await;
    expect_signed_in(&mut login).await;
}

#[tokio::test]
async fn old_client_without_purpose_still_resets_by_existing_email() {
    let (url, db, mailer) = spawn_with_recording_mailer().await;
    let (acc, _, _) = rcdesk_server::owners::create_owner(&db, "UA", 1).expect("owner");
    let hash = rcdesk_server::accounts::hash_password("old-password-1").expect("hash");
    assert!(db
        .insert_account(&acc, "dave@example.com", &hash, 1)
        .expect("account"));
    let mut ws = client_conn(&url).await;
    client_auth(&mut ws, None).await;
    let code = request_code(&mut ws, &mailer, "dave@example.com").await;
    send_raw_json(
        &mut ws,
        &format!(
            r#"{{"type":"email_code_confirm","email":"dave@example.com","code":"{code}","password":"new-password-2"}}"#
        ),
    )
    .await;
    let (_, _, email) = expect_signed_in(&mut ws).await;
    assert_eq!(email, "dave@example.com");
    let mut login = client_conn(&url).await;
    password_login(&mut login, "dave@example.com", "new-password-2").await;
    expect_signed_in(&mut login).await;
}

// ---- Email change for a signed-in account ----

async fn email_change(ws: &mut WsStream, email: &str, code: &str, password: &str) {
    send(
        ws,
        &SignalMessage::EmailChange {
            email: email.to_string(),
            code: code.to_string(),
            password: password.to_string(),
        },
    )
    .await;
}

/// Registers `email` / `password` on a fresh connection; returns the
/// connection and its owner.
async fn registered_conn(
    url: &str,
    db: &Db,
    mailer: &RecordingMailer,
    email: &str,
    password: &str,
) -> (WsStream, String) {
    let mut ws = client_conn(url).await;
    let (token, _) = client_auth(&mut ws, None).await;
    let owner = owner_of_token(db, &token).expect("owner");
    let code = request_code(&mut ws, mailer, email).await;
    confirm(&mut ws, email, &code, password).await;
    expect_signed_in(&mut ws).await;
    (ws, owner)
}

#[tokio::test]
async fn email_change_moves_the_account_and_notifies_the_old_address() {
    let (url, db, mailer) = spawn_with_recording_mailer().await;
    let (mut ws, owner) =
        registered_conn(&url, &db, &mailer, "old@example.com", "hunter22-pass").await;

    let code = request_code(&mut ws, &mailer, "new@example.com").await;
    email_change(&mut ws, " New@Example.com ", &code, "hunter22-pass").await;
    assert_eq!(
        recv(&mut ws).await,
        SignalMessage::Account {
            email: Some("new@example.com".to_string())
        }
    );
    assert_eq!(
        db.email_for_owner(&owner).expect("email").as_deref(),
        Some("new@example.com")
    );
    // The notice goes out right after the reply; give the server a moment.
    for _ in 0..50 {
        if !mailer.changed.lock().expect("mailer mutex").is_empty() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert_eq!(
        *mailer.changed.lock().expect("mailer mutex"),
        vec![("old@example.com".to_string(), "new@example.com".to_string())]
    );

    // The old address no longer logs in; the new one does with the same password.
    let mut login = client_conn(&url).await;
    password_login(&mut login, "old@example.com", "hunter22-pass").await;
    expect_error(&mut login, "wrong email or password").await;
    password_login(&mut login, "new@example.com", "hunter22-pass").await;
    let (_, _, email) = expect_signed_in(&mut login).await;
    assert_eq!(email, "new@example.com");
}

#[tokio::test]
async fn email_change_with_wrong_password_keeps_the_code_valid() {
    let (url, db, mailer) = spawn_with_recording_mailer().await;
    let (mut ws, owner) =
        registered_conn(&url, &db, &mailer, "old@example.com", "hunter22-pass").await;

    let code = request_code(&mut ws, &mailer, "new@example.com").await;
    email_change(&mut ws, "new@example.com", &code, "not-the-password").await;
    expect_error(&mut ws, "wrong password").await;
    assert_eq!(
        db.email_for_owner(&owner).expect("email").as_deref(),
        Some("old@example.com")
    );

    email_change(&mut ws, "new@example.com", &code, "hunter22-pass").await;
    assert_eq!(
        recv(&mut ws).await,
        SignalMessage::Account {
            email: Some("new@example.com".to_string())
        }
    );
}

#[tokio::test]
async fn email_change_with_wrong_code_is_rejected() {
    let (url, db, mailer) = spawn_with_recording_mailer().await;
    let (mut ws, owner) =
        registered_conn(&url, &db, &mailer, "old@example.com", "hunter22-pass").await;

    let code = request_code(&mut ws, &mailer, "new@example.com").await;
    let wrong = if code == "000000" { "111111" } else { "000000" };
    email_change(&mut ws, "new@example.com", wrong, "hunter22-pass").await;
    expect_error(&mut ws, "wrong code").await;
    assert_eq!(
        db.email_for_owner(&owner).expect("email").as_deref(),
        Some("old@example.com")
    );
    assert!(mailer.changed.lock().expect("mailer mutex").is_empty());
}

#[tokio::test]
async fn email_change_to_a_taken_email_is_rejected() {
    let (url, db, mailer) = spawn_with_recording_mailer().await;
    // Another account is set up directly: the per-IP code limiter is small.
    let (other_owner, _, _) = rcdesk_server::owners::create_owner(&db, "UA", 1).expect("owner");
    let other_hash = rcdesk_server::accounts::hash_password("other-pass-1").expect("hash");
    assert!(db
        .insert_account(&other_owner, "taken@example.com", &other_hash, 1)
        .expect("account"));
    let (mut ws, owner) =
        registered_conn(&url, &db, &mailer, "old@example.com", "hunter22-pass").await;

    let code = request_code(&mut ws, &mailer, "taken@example.com").await;
    email_change(&mut ws, "taken@example.com", &code, "hunter22-pass").await;
    expect_error(&mut ws, "email taken").await;
    assert_eq!(
        db.email_for_owner(&owner).expect("email").as_deref(),
        Some("old@example.com")
    );
    assert_eq!(
        db.email_for_owner(&other_owner).expect("email").as_deref(),
        Some("taken@example.com")
    );
}

#[tokio::test]
async fn email_change_to_the_current_email_is_rejected() {
    let (url, db, mailer) = spawn_with_recording_mailer().await;
    let (mut ws, _owner) =
        registered_conn(&url, &db, &mailer, "old@example.com", "hunter22-pass").await;

    email_change(&mut ws, "OLD@example.com", "123456", "hunter22-pass").await;
    expect_error(&mut ws, "same email").await;
}

#[tokio::test]
async fn email_change_requires_an_account_and_an_owner() {
    let (url, _db, _mailer) = spawn_with_recording_mailer().await;

    // Owner without an account.
    let mut ws = client_conn(&url).await;
    client_auth(&mut ws, None).await;
    email_change(&mut ws, "new@example.com", "123456", "hunter22-pass").await;
    expect_error(&mut ws, "no account").await;

    // Connection without an owner.
    let mut anon = client_conn(&url).await;
    email_change(&mut anon, "new@example.com", "123456", "hunter22-pass").await;
    expect_error(&mut anon, "not authenticated").await;

    // Invalid new address.
    email_change(&mut anon, "nope", "123456", "hunter22-pass").await;
    expect_error(&mut anon, "invalid email").await;
}
