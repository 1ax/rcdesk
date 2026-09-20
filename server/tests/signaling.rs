use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use proto::signal::{DeviceCredentials, IceCandidate, Role, SignalMessage};
use rcdesk_server::app::app;
use rcdesk_server::db::Db;
use rcdesk_server::ice::IceConfig;
use rcdesk_server::registry::Registry;
use tokio::net::{TcpListener, TcpStream};
use tokio::time::timeout;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::{connect_async, MaybeTlsStream, WebSocketStream};

type WsStream = WebSocketStream<MaybeTlsStream<TcpStream>>;

const TIMEOUT: Duration = Duration::from_secs(2);

/// Boots the signaling app on an OS-assigned port and returns its ws:// URL.
async fn spawn_server() -> String {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind ephemeral port");
    let addr = listener.local_addr().expect("local addr");
    let registry = Registry::new();
    let ice = IceConfig::from_env();
    let db = Db::in_memory().expect("open in-memory db");
    tokio::spawn(async move {
        axum::serve(listener, app(registry, ice, db))
            .await
            .expect("serve");
    });
    format!("ws://{addr}/ws")
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
        },
    )
    .await;
    let pin = match recv(&mut host).await {
        SignalMessage::Registered { pin, .. } => pin,
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
        } => {
            assert_eq!(host_name, "My Mac");
            assert!(!ice_servers.is_empty());
            assert!(ice_servers[0].urls[0].starts_with("stun:"));
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
#[tokio::test]
async fn offer_answer_and_ice_are_forwarded_between_session_peers() {
    let url = spawn_server().await;
    let (mut host, mut client, session_id, _pin) = host_and_joined_client(&url).await;

    send(
        &mut host,
        &SignalMessage::Offer {
            session_id: session_id.clone(),
            sdp: "offer-sdp".to_string(),
        },
    )
    .await;
    match recv(&mut client).await {
        SignalMessage::Offer {
            session_id: sid,
            sdp,
        } => {
            assert_eq!(sid, session_id);
            assert_eq!(sdp, "offer-sdp");
        }
        other => panic!("expected offer, got {other:?}"),
    }

    send(
        &mut client,
        &SignalMessage::Answer {
            session_id: session_id.clone(),
            sdp: "answer-sdp".to_string(),
        },
    )
    .await;
    match recv(&mut host).await {
        SignalMessage::Answer {
            session_id: sid,
            sdp,
        } => {
            assert_eq!(sid, session_id);
            assert_eq!(sdp, "answer-sdp");
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

// (f) client disconnects -> host gets Bye -> a new client can join with the
// same PIN.
#[tokio::test]
async fn client_disconnect_frees_host_for_new_client() {
    let url = spawn_server().await;
    let (mut host, client, session_id, pin) = host_and_joined_client(&url).await;

    drop(client);

    match recv(&mut host).await {
        SignalMessage::Bye { session_id: sid } => assert_eq!(sid, session_id),
        other => panic!("expected bye, got {other:?}"),
    }

    let mut new_client = connect(&url).await;
    hello(&mut new_client, Role::Client).await;
    send(&mut new_client, &SignalMessage::Join { pin }).await;

    match recv(&mut new_client).await {
        SignalMessage::Joined { .. } => {}
        other => panic!("expected joined, got {other:?}"),
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
// client of its active session gets Bye with that session's id.
#[tokio::test]
async fn re_registering_same_device_displaces_old_connection_and_ends_its_session() {
    let url = spawn_server().await;

    let mut host1 = connect(&url).await;
    hello(&mut host1, Role::Host).await;
    send(
        &mut host1,
        &SignalMessage::HostRegister {
            name: "Test Host".to_string(),
            device: None,
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
    let session_id = match recv(&mut client).await {
        SignalMessage::Joined { session_id, .. } => session_id,
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
        },
    )
    .await;

    match recv(&mut host1).await {
        SignalMessage::Error { message } => assert_eq!(message, "replaced by a new connection"),
        other => panic!("expected error, got {other:?}"),
    }
    match recv(&mut client).await {
        SignalMessage::Bye { session_id: sid } => assert_eq!(sid, session_id),
        other => panic!("expected bye, got {other:?}"),
    }
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
