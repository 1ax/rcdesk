//! `/ws` handler: the signaling protocol over one WebSocket connection.
//!
//! Every connection gets a read task (this module's `run_connection` and
//! friends, driven from the `on_upgrade` future) and an outgoing bounded queue
//! (`registry::Tx`, `OUTBOUND_QUEUE` messages) paired with a dedicated writer
//! task that serializes messages to the socket. Other connections push
//! messages to this one purely by holding a clone of its `Tx`. If the queue
//! overflows or the peer stops reading (`WRITE_TIMEOUT`), a kill switch closes
//! the connection: the read loop exits and runs its usual cleanup.

use axum::body::Bytes;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{ConnectInfo, State};
use axum::http::HeaderMap;
use axum::response::IntoResponse;
use futures_util::stream::SplitStream;
use futures_util::{SinkExt, StreamExt};
use proto::signal::{BrowserEntry, DeviceEntry, Role, SignalMessage};
use std::net::{IpAddr, SocketAddr};
use std::time::{Duration, Instant};
use tokio::time::{interval, timeout, MissedTickBehavior};
use webauthn_rs::prelude::{DiscoverableAuthentication, PasskeyRegistration};

use crate::app::AppState;
use crate::db::Db;
use crate::devices::{self, DeviceAuth};
use crate::ice::{self, IceConfig};
use crate::ip_limit::{GlobalLimiter, IpLimiter};
use crate::owners;
use crate::passkeys::{self, PasskeyError, SharedWebauthn};
use crate::registry::{Registry, Tx};

/// Максимальный размер входящего WebSocket-сообщения/кадра. Самый большой
/// легитимный кадр: `Offer.sdp` — единицы КБ, JSON WebAuthn — 1–2 КБ. Дефолт
/// tungstenite (64 МиБ) давал OOM одним сообщением.
pub const MAX_MESSAGE_BYTES: usize = 64 * 1024;
/// Сколько ждём записи в сокет, прежде чем счесть пира не читающим.
pub const WRITE_TIMEOUT: Duration = Duration::from_secs(10);
/// Максимальная длина имени хоста и алиаса устройства, в символах.
pub const MAX_NAME_CHARS: usize = 64;

/// The client's IP: the first valid `X-Real-IP` value (the server sits behind
/// nginx, so the socket peer is the proxy), otherwise the socket peer.
pub fn client_ip(headers: &HeaderMap, peer: SocketAddr) -> IpAddr {
    headers
        .get_all("x-real-ip")
        .iter()
        .filter_map(|value| value.to_str().ok())
        .find_map(|value| value.trim().parse::<IpAddr>().ok())
        .unwrap_or_else(|| peer.ip())
}

/// Что сервер знает о клиенте из апгрейда: IP и `User-Agent` (пустой, если
/// заголовка нет или он не UTF-8).
struct ClientInfo {
    ip: IpAddr,
    user_agent: String,
}

pub async fn ws_handler(
    ws: WebSocketUpgrade,
    State(state): State<AppState>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
) -> impl IntoResponse {
    let info = ClientInfo {
        ip: client_ip(&headers, peer),
        user_agent: headers
            .get(axum::http::header::USER_AGENT)
            .and_then(|value| value.to_str().ok())
            .unwrap_or_default()
            .to_string(),
    };
    ws.max_message_size(MAX_MESSAGE_BYTES)
        .max_frame_size(MAX_MESSAGE_BYTES)
        .on_upgrade(move |socket| handle_socket(socket, state, info))
}

async fn handle_socket(socket: WebSocket, state: AppState, info: ClientInfo) {
    let (mut sink, mut stream) = socket.split();
    let (tx, mut rx, kill) = Tx::channel();

    let liveness = state.liveness;
    let read_timeout = liveness.read_timeout;
    let mut writer = tokio::spawn(async move {
        let mut ping = interval(liveness.ping_interval);
        ping.set_missed_tick_behavior(MissedTickBehavior::Delay);
        // The first tick fires immediately; the first Ping goes out one
        // interval from now.
        ping.tick().await;
        loop {
            let msg = tokio::select! {
                msg = rx.recv() => match msg {
                    Some(msg) => msg,
                    None => break,
                },
                _ = ping.tick() => {
                    match timeout(WRITE_TIMEOUT, sink.send(Message::Ping(Bytes::new()))).await {
                        Ok(Ok(())) => {}
                        Ok(Err(_)) => break,
                        Err(_) => {
                            tracing::warn!("peer not reading for 10s, closing connection");
                            kill.kill();
                            break;
                        }
                    }
                    continue;
                }
            };
            let json = match serde_json::to_string(&msg) {
                Ok(json) => json,
                Err(err) => {
                    tracing::error!(?err, "failed to serialize outgoing signal message");
                    continue;
                }
            };
            match timeout(WRITE_TIMEOUT, sink.send(Message::Text(json.into()))).await {
                Ok(Ok(())) => {}
                Ok(Err(_)) => break,
                Err(_) => {
                    tracing::warn!("peer not reading for 10s, closing connection");
                    kill.kill();
                    break;
                }
            }
        }
        let _ = timeout(WRITE_TIMEOUT, sink.close()).await;
    });

    tracing::info!("websocket connection opened");
    run_connection(
        &mut stream,
        tx.clone(),
        &state.registry,
        &state.ice,
        &state.db,
        &state.pin_limiter,
        &state.pin_global_limiter,
        &state.owner_limiter,
        &state.device_limiter,
        &state.webauthn,
        info,
        read_timeout,
        liveness.pin_ttl,
    )
    .await;
    tracing::info!("websocket connection closed");

    // Drop our own sender clone so the writer task's channel closes once no
    // other connection (via the registry) is still holding a clone either.
    drop(tx);
    // Даём дослать последний `Error`, но не ждём застрявшего пира.
    if timeout(Duration::from_secs(5), &mut writer).await.is_err() {
        writer.abort();
    }
}

/// Reads the next text frame and parses it as a `SignalMessage`. Returns
/// `None` when the connection is over (closed, transport error) or the frame
/// was invalid JSON / not text (an `Error` reply is sent for the latter).
/// Ping/Pong control frames are transparently skipped (axum answers Pings
/// automatically at the transport level) but they reset the read timeout:
/// if no frame of any kind arrives within `read_timeout`, the connection is
/// considered dead and `None` is returned (B3).
async fn next_message(
    stream: &mut SplitStream<WebSocket>,
    tx: &Tx,
    read_timeout: Duration,
) -> Option<SignalMessage> {
    loop {
        let item = tokio::select! {
            item = timeout(read_timeout, stream.next()) => match item {
                Ok(item) => item,
                Err(_) => {
                    tracing::warn!("no frames for {read_timeout:?}, closing connection");
                    return None;
                }
            },
            _ = tx.killed() => {
                tracing::info!(
                    "connection closed by server: outbound queue overflow or peer not reading"
                );
                return None;
            }
        };
        match item {
            Some(Ok(Message::Text(text))) => match serde_json::from_str(&text) {
                Ok(msg) => return Some(msg),
                Err(err) => {
                    tracing::debug!(?err, "invalid json from peer");
                    let _ = tx.send(SignalMessage::Error {
                        message: "invalid message".to_string(),
                    });
                    return None;
                }
            },
            Some(Ok(Message::Ping(_))) | Some(Ok(Message::Pong(_))) => continue,
            Some(Ok(Message::Close(_))) | None => return None,
            Some(Ok(Message::Binary(_))) => {
                let _ = tx.send(SignalMessage::Error {
                    message: "invalid message".to_string(),
                });
                return None;
            }
            Some(Err(err)) => {
                tracing::warn!(?err, "websocket read error");
                return None;
            }
        }
    }
}

#[allow(clippy::too_many_arguments)]
async fn run_connection(
    stream: &mut SplitStream<WebSocket>,
    tx: Tx,
    registry: &Registry,
    ice: &IceConfig,
    db: &Db,
    pin_limiter: &IpLimiter,
    pin_global_limiter: &GlobalLimiter,
    owner_limiter: &IpLimiter,
    device_limiter: &IpLimiter,
    webauthn: &SharedWebauthn,
    info: ClientInfo,
    read_timeout: Duration,
    pin_ttl: Duration,
) {
    let role = match next_message(stream, &tx, read_timeout).await {
        Some(SignalMessage::Hello { role, .. }) => role,
        Some(_) => {
            let _ = tx.send(SignalMessage::Error {
                message: "expected hello".to_string(),
            });
            return;
        }
        None => return,
    };

    match role {
        Role::Host => {
            run_host(
                stream,
                tx,
                registry,
                ice,
                db,
                device_limiter,
                info.ip,
                read_timeout,
            )
            .await
        }
        Role::Client => {
            run_client(
                stream,
                tx,
                registry,
                ice,
                db,
                pin_limiter,
                pin_global_limiter,
                owner_limiter,
                webauthn,
                info,
                read_timeout,
                pin_ttl,
            )
            .await
        }
    }
}

#[allow(clippy::too_many_arguments)]
async fn run_host(
    stream: &mut SplitStream<WebSocket>,
    tx: Tx,
    registry: &Registry,
    ice: &IceConfig,
    db: &Db,
    device_limiter: &IpLimiter,
    ip: IpAddr,
    read_timeout: Duration,
) {
    let (name, device, session_id, pin_rotation) =
        match next_message(stream, &tx, read_timeout).await {
            Some(SignalMessage::HostRegister {
                name,
                device,
                session_id,
                pin_rotation,
            }) => (name, device, session_id, pin_rotation),
            Some(_) => {
                let _ = tx.send(SignalMessage::Error {
                    message: "expected host_register".to_string(),
                });
                return;
            }
            None => return,
        };

    if device.is_none() && !device_limiter.check(ip) {
        tracing::warn!(%ip, "new device registration rate-limited");
        let _ = tx.send(SignalMessage::Error {
            message: "too many attempts".to_string(),
        });
        return;
    }

    let name = owners::truncate_chars(&name, MAX_NAME_CHARS).to_string();
    let auth = match devices::authenticate(db, device.as_ref(), &name, ice::now_unix() as i64) {
        Ok(auth) => auth,
        Err(err) => {
            tracing::error!(?err, "device authentication failed");
            let _ = tx.send(SignalMessage::Error {
                message: "internal error".to_string(),
            });
            return;
        }
    };

    let (host_id, issued_credentials) = match auth {
        DeviceAuth::Unknown => {
            let _ = tx.send(SignalMessage::Error {
                message: "unknown device".to_string(),
            });
            return;
        }
        DeviceAuth::BadSecret => {
            let _ = tx.send(SignalMessage::Error {
                message: "invalid device credentials".to_string(),
            });
            return;
        }
        DeviceAuth::Issued {
            device_id,
            credentials,
        } => {
            device_limiter.spend(ip);
            (device_id, Some(credentials))
        }
        DeviceAuth::Known { device_id } => (device_id, None),
    };

    let (pin, displaced) = registry.register_host(
        host_id.clone(),
        name,
        tx.clone(),
        session_id.clone(),
        pin_rotation,
    );
    tracing::info!(host_id = %host_id, "host registered");
    // Slice 3.2a (D35): the host reported a still-live session_id at
    // registration (survived a signaling-only reconnect, slice 3.5a).
    // `peer_tx_for_host` after the fact is the cheapest way to tell the two
    // cases apart for logging: `Some` means `register_host` found a
    // `SessionEntry` for it under this host_id and re-attached it; `None`
    // means it's "detached" -- unknown to the server, or belonging to a
    // different host_id -- so the device just looks busy until a `Bye`.
    if let Some(sid) = &session_id {
        match registry.peer_tx_for_host(&host_id, sid) {
            Some(_) => {
                tracing::info!(host_id = %host_id, session_id = %sid, "session re-attached after host reconnect");
            }
            None => {
                tracing::info!(host_id = %host_id, session_id = %sid, "host reports a live session unknown to the server");
            }
        }
    }
    if let Some(displaced) = displaced {
        tracing::info!(host_id = %host_id, "displaced an existing connection for this host_id");
        let _ = displaced.host_tx.send(SignalMessage::Error {
            message: "replaced by a new connection".to_string(),
        });
        if let Some((session_id, _client_tx)) = displaced.session {
            // The session is P2P; losing signaling (here, the host's old
            // socket being displaced) doesn't tear it down, so the client is
            // not told `Bye` -- it keeps talking to the host directly.
            tracing::info!(%session_id, "session left to p2p after signaling loss");
        }
    }
    let _ = tx.send(SignalMessage::Registered {
        host_id: host_id.clone(),
        pin,
        ice_servers: ice.stun_servers(),
        device: issued_credentials,
    });

    loop {
        let msg = match next_message(stream, &tx, read_timeout).await {
            Some(msg) => msg,
            None => break,
        };

        match msg {
            SignalMessage::Offer {
                session_id,
                sdp,
                auth,
            } => match registry.peer_tx_for_host(&host_id, &session_id) {
                Some(peer) => {
                    tracing::debug!(%session_id, "forwarding offer host -> client");
                    let _ = peer.send(SignalMessage::Offer {
                        session_id,
                        sdp,
                        auth,
                    });
                }
                None => {
                    let _ = tx.send(SignalMessage::Error {
                        message: "not in session".to_string(),
                    });
                }
            },
            SignalMessage::Answer {
                session_id,
                sdp,
                auth,
            } => match registry.peer_tx_for_host(&host_id, &session_id) {
                Some(peer) => {
                    tracing::debug!(%session_id, "forwarding answer host -> client");
                    let _ = peer.send(SignalMessage::Answer {
                        session_id,
                        sdp,
                        auth,
                    });
                }
                None => {
                    let _ = tx.send(SignalMessage::Error {
                        message: "not in session".to_string(),
                    });
                }
            },
            SignalMessage::Ice {
                session_id,
                candidate,
            } => match registry.peer_tx_for_host(&host_id, &session_id) {
                Some(peer) => {
                    tracing::debug!(%session_id, "forwarding ice host -> client");
                    let _ = peer.send(SignalMessage::Ice {
                        session_id,
                        candidate,
                    });
                }
                None => {
                    let _ = tx.send(SignalMessage::Error {
                        message: "not in session".to_string(),
                    });
                }
            },
            // Slice 3.2c: OPAQUE login messages the host sends toward the
            // client during `PeerJoined`-triggered authentication. Same
            // opaque-relay shape as `Offer`/`Ice` above -- the server never
            // interprets `payload`.
            SignalMessage::AuthRequired { session_id } => {
                match registry.peer_tx_for_host(&host_id, &session_id) {
                    Some(peer) => {
                        tracing::debug!(%session_id, "forwarding auth_required host -> client");
                        let _ = peer.send(SignalMessage::AuthRequired { session_id });
                    }
                    None => {
                        let _ = tx.send(SignalMessage::Error {
                            message: "not in session".to_string(),
                        });
                    }
                }
            }
            SignalMessage::PakeResponse {
                session_id,
                payload,
            } => match registry.peer_tx_for_host(&host_id, &session_id) {
                Some(peer) => {
                    tracing::debug!(%session_id, "forwarding pake_response host -> client");
                    let _ = peer.send(SignalMessage::PakeResponse {
                        session_id,
                        payload,
                    });
                }
                None => {
                    let _ = tx.send(SignalMessage::Error {
                        message: "not in session".to_string(),
                    });
                }
            },
            SignalMessage::AuthFailed {
                session_id,
                retry_after_secs,
            } => match registry.peer_tx_for_host(&host_id, &session_id) {
                Some(peer) => {
                    tracing::debug!(%session_id, "forwarding auth_failed host -> client");
                    let _ = peer.send(SignalMessage::AuthFailed {
                        session_id,
                        retry_after_secs,
                    });
                }
                None => {
                    let _ = tx.send(SignalMessage::Error {
                        message: "not in session".to_string(),
                    });
                }
            },
            SignalMessage::Bye { session_id, reason } => {
                match registry.close_session_by_host(&host_id, &session_id) {
                    Some(Some(client_tx)) => {
                        tracing::info!(%session_id, "host ended session");
                        let _ = client_tx.send(SignalMessage::Bye { session_id, reason });
                    }
                    Some(None) => {
                        // Slice 3.2a (D35): a "detached" session -- reported
                        // by the host at registration, never backed by a
                        // `SessionEntry` here (or one that belonged to a
                        // different host_id) -- there's no client to notify.
                        tracing::debug!(%session_id, "host ended a detached session, nothing to notify");
                    }
                    None => {
                        // Unknown to the server -- most likely a stale
                        // `session_id` from before a signaling reconnect
                        // (slice 3.5a). The session is P2P and may well still
                        // be live; a quiet no-op instead of `Error`.
                        tracing::debug!(%session_id, "bye for a session not in session");
                    }
                }
            }
            _ => {
                let _ = tx.send(SignalMessage::Error {
                    message: "unexpected message".to_string(),
                });
                break;
            }
        }
    }

    tracing::info!(host_id = %host_id, "host disconnected");
    if let Some((session_id, _client_tx)) = registry.unregister_host(&host_id, &tx) {
        // Same reasoning as the displaced-connection case above: the socket
        // closing doesn't mean the P2P session is over, so no `Bye`.
        tracing::info!(%session_id, "session left to p2p after signaling loss");
    }
}

/// Builds the `DeviceEntry` list for `Authenticated`/`Devices`: the owner's
/// linked devices from the persistent store, each combined with its
/// sign-in-memory presence from the registry. One place for this so the
/// five branches below (`ClientAuth`, `ListDevices`, `ConnectDevice`'s
/// ownership check aside, `RenameDevice`, `ForgetDevice`) don't repeat it.
fn device_entries_for_owner(
    db: &Db,
    registry: &Registry,
    owner_id: &str,
) -> anyhow::Result<Vec<DeviceEntry>> {
    let devices = db.devices_for_owner(owner_id)?;
    Ok(devices
        .into_iter()
        .map(|device| {
            let presence = registry.host_presence(&device.device_id);
            DeviceEntry {
                device_id: device.device_id,
                name: device.name,
                alias: device.alias,
                online: presence.is_some(),
                busy: presence == Some(true),
                last_seen_at: device.last_seen_at,
            }
        })
        .collect())
}

/// Браузер, под которым авторизовано соединение (слайс 3.7a).
struct ClientSession {
    owner_id: String,
    token_id: String,
}

/// Владелец соединения, если его токен ещё жив. Токен мог быть отозван
/// другим браузером — тогда соединение забывает владельца (`session = None`)
/// и ведёт себя как неавторизованное.
fn current_owner(db: &Db, session: &mut Option<ClientSession>) -> anyhow::Result<Option<String>> {
    let Some(current) = session else {
        return Ok(None);
    };
    if db.token_exists(&current.token_id)? {
        return Ok(Some(current.owner_id.clone()));
    }
    *session = None;
    Ok(None)
}

/// `current_owner` для веток, которым владелец обязателен: сама отвечает
/// `not authenticated` / `internal error` и возвращает `None`.
fn require_owner(db: &Db, session: &mut Option<ClientSession>, tx: &Tx) -> Option<String> {
    match current_owner(db, session) {
        Ok(Some(owner_id)) => Some(owner_id),
        Ok(None) => {
            let _ = tx.send(SignalMessage::Error {
                message: "not authenticated".to_string(),
            });
            None
        }
        Err(err) => {
            tracing::error!(?err, "failed to check owner token");
            let _ = tx.send(SignalMessage::Error {
                message: "internal error".to_string(),
            });
            None
        }
    }
}

/// Список браузеров владельца для `Browsers`; `current_token_id` помечает
/// токен этого соединения.
fn browser_entries(
    db: &Db,
    owner_id: &str,
    current_token_id: &str,
) -> anyhow::Result<Vec<BrowserEntry>> {
    Ok(db
        .tokens_for_owner(owner_id)?
        .into_iter()
        .map(|token| BrowserEntry {
            current: token.token_id == current_token_id,
            id: token.token_id,
            user_agent: token.user_agent,
            created_at: token.created_at,
            last_used_at: token.last_used_at,
        })
        .collect())
}

/// Выход: удаляет токен соединения, забывает владельца, шлёт `SignedOut`.
fn sign_out(db: &Db, session: &mut Option<ClientSession>, tx: &Tx) {
    if let Some(current) = session.take() {
        if let Err(err) = db.delete_token(&current.owner_id, &current.token_id) {
            tracing::error!(?err, "failed to delete token on sign out");
            let _ = tx.send(SignalMessage::Error {
                message: "internal error".to_string(),
            });
            return;
        }
    }
    let _ = tx.send(SignalMessage::SignedOut);
}

/// Ответ `Passkeys` со списком passkey владельца.
fn send_passkeys(db: &Db, owner_id: &str, tx: &Tx) {
    match passkeys::passkey_entries(db, owner_id) {
        Ok(passkeys) => {
            let _ = tx.send(SignalMessage::Passkeys { passkeys });
        }
        Err(err) => {
            tracing::error!(?err, "failed to load passkeys for owner");
            let _ = tx.send(SignalMessage::Error {
                message: "internal error".to_string(),
            });
        }
    }
}

fn passkey_failed(tx: &Tx) {
    let _ = tx.send(SignalMessage::Error {
        message: "passkey failed".to_string(),
    });
}

/// Любая ошибка WebAuthn — `passkey failed` (причина только в логе),
/// ошибка БД — `internal error`.
fn passkey_error(tx: &Tx, err: PasskeyError) {
    match err {
        PasskeyError::Rejected(reason) => {
            tracing::warn!(%reason, "passkey ceremony rejected");
            passkey_failed(tx);
        }
        PasskeyError::Internal(err) => {
            tracing::error!(?err, "passkey ceremony failed");
            let _ = tx.send(SignalMessage::Error {
                message: "internal error".to_string(),
            });
        }
    }
}

/// Успешный вход по passkey: соединение становится владельцем аккаунта
/// `account_id`. Если до этого соединение было другим живым владельцем A:
/// анонимный A (без passkey) сливается в аккаунт — его привязки устройств
/// переезжают, сама запись A удаляется вместе с токенами; A-аккаунт (с
/// passkey) и A == B остаются как есть, у A удаляется только токен этого
/// соединения. Вход всегда выдаёт НОВЫЙ токен аккаунта; возвращает его.
fn switch_to_account(
    db: &Db,
    session: &mut Option<ClientSession>,
    account_id: &str,
    user_agent: &str,
    now: i64,
) -> anyhow::Result<String> {
    let previous = match current_owner(db, session)? {
        Some(previous_owner) => session.take().map(|s| (previous_owner, s.token_id)),
        None => None,
    };
    if let Some((previous_owner, previous_token)) = &previous {
        if !db.merge_anonymous_owner(previous_owner, account_id)? {
            db.delete_token(previous_owner, previous_token)?;
        }
    }
    let (token_id, token) = owners::issue_token(db, account_id, user_agent, now)?;
    *session = Some(ClientSession {
        owner_id: account_id.to_string(),
        token_id,
    });
    Ok(token)
}

#[allow(clippy::too_many_arguments)]
async fn run_client(
    stream: &mut SplitStream<WebSocket>,
    tx: Tx,
    registry: &Registry,
    ice: &IceConfig,
    db: &Db,
    pin_limiter: &IpLimiter,
    pin_global_limiter: &GlobalLimiter,
    owner_limiter: &IpLimiter,
    webauthn: &SharedWebauthn,
    info: ClientInfo,
    read_timeout: Duration,
    pin_ttl: Duration,
) {
    let ClientInfo { ip, user_agent } = info;
    // The owner this connection has authenticated as (slice 3.1), if any --
    // `ClientAuth` is optional, so an older client that never sends it stays
    // fully compatible, just without device linking/listing.
    let mut session: Option<ClientSession> = None;
    // Passkey ceremonies in flight on this connection (slice 3.7b): kept in
    // memory only, consumed by the matching `Finish`.
    let mut passkey_registration: Option<PasskeyRegistration> = None;
    let mut passkey_login: Option<DiscoverableAuthentication> = None;

    loop {
        let msg = match next_message(stream, &tx, read_timeout).await {
            Some(msg) => msg,
            None => break,
        };

        match msg {
            SignalMessage::ClientAuth { token } => {
                let now = ice::now_unix() as i64;
                let auth = match owners::lookup_token(db, token.as_deref(), &user_agent, now) {
                    Ok(Some((owner_id, token_id))) => {
                        Ok((owner_id, token_id, token.unwrap_or_default()))
                    }
                    Ok(None) => {
                        if !owner_limiter.check(ip) {
                            tracing::warn!(%ip, "new owner creation rate-limited");
                            let _ = tx.send(SignalMessage::Error {
                                message: "too many attempts".to_string(),
                            });
                            continue;
                        }
                        let created = owners::create_owner(db, &user_agent, now);
                        if created.is_ok() {
                            owner_limiter.spend(ip);
                        }
                        created
                    }
                    Err(err) => Err(err),
                };
                match auth {
                    Ok((owner_id, token_id, token)) => {
                        let devices = match device_entries_for_owner(db, registry, &owner_id) {
                            Ok(devices) => devices,
                            Err(err) => {
                                tracing::error!(?err, "failed to load devices for owner");
                                Vec::new()
                            }
                        };
                        session = Some(ClientSession { owner_id, token_id });
                        let _ = tx.send(SignalMessage::Authenticated { token, devices });
                    }
                    Err(err) => {
                        tracing::error!(?err, "owner authentication failed");
                        let _ = tx.send(SignalMessage::Error {
                            message: "internal error".to_string(),
                        });
                    }
                }
            }
            SignalMessage::ListDevices => {
                let Some(owner_id) = require_owner(db, &mut session, &tx) else {
                    continue;
                };
                match device_entries_for_owner(db, registry, &owner_id) {
                    Ok(devices) => {
                        let _ = tx.send(SignalMessage::Devices { devices });
                    }
                    Err(err) => {
                        tracing::error!(?err, "failed to load devices for owner");
                        let _ = tx.send(SignalMessage::Error {
                            message: "internal error".to_string(),
                        });
                    }
                }
            }
            SignalMessage::ListBrowsers => {
                let Some(owner_id) = require_owner(db, &mut session, &tx) else {
                    continue;
                };
                let current = session.as_ref().map(|s| s.token_id.as_str()).unwrap_or("");
                match browser_entries(db, &owner_id, current) {
                    Ok(browsers) => {
                        let _ = tx.send(SignalMessage::Browsers { browsers });
                    }
                    Err(err) => {
                        tracing::error!(?err, "failed to load browsers for owner");
                        let _ = tx.send(SignalMessage::Error {
                            message: "internal error".to_string(),
                        });
                    }
                }
            }
            SignalMessage::RevokeBrowser { id } => {
                let Some(owner_id) = require_owner(db, &mut session, &tx) else {
                    continue;
                };
                let current = session
                    .as_ref()
                    .map(|s| s.token_id.clone())
                    .unwrap_or_default();
                if id == current {
                    sign_out(db, &mut session, &tx);
                    continue;
                }
                match db
                    .delete_token(&owner_id, &id)
                    .and_then(|_| browser_entries(db, &owner_id, &current))
                {
                    Ok(browsers) => {
                        let _ = tx.send(SignalMessage::Browsers { browsers });
                    }
                    Err(err) => {
                        tracing::error!(?err, "failed to revoke browser");
                        let _ = tx.send(SignalMessage::Error {
                            message: "internal error".to_string(),
                        });
                    }
                }
            }
            SignalMessage::SignOut => {
                if require_owner(db, &mut session, &tx).is_none() {
                    continue;
                }
                sign_out(db, &mut session, &tx);
            }
            SignalMessage::ListPasskeys => {
                let Some(owner_id) = require_owner(db, &mut session, &tx) else {
                    continue;
                };
                send_passkeys(db, &owner_id, &tx);
            }
            SignalMessage::PasskeyRegisterStart => {
                let Some(owner_id) = require_owner(db, &mut session, &tx) else {
                    continue;
                };
                match passkeys::register_start(webauthn, db, &owner_id) {
                    Ok((options, state)) => {
                        passkey_registration = Some(state);
                        let _ = tx.send(SignalMessage::PasskeyRegisterOptions { options });
                    }
                    Err(err) => passkey_error(&tx, err),
                }
            }
            SignalMessage::PasskeyRegisterFinish { credential } => {
                let Some(owner_id) = require_owner(db, &mut session, &tx) else {
                    continue;
                };
                let Some(state) = passkey_registration.take() else {
                    tracing::warn!("passkey register finish without start");
                    passkey_failed(&tx);
                    continue;
                };
                match passkeys::register_finish(
                    webauthn,
                    db,
                    &owner_id,
                    &state,
                    &credential,
                    &user_agent,
                    ice::now_unix() as i64,
                ) {
                    Ok(()) => send_passkeys(db, &owner_id, &tx),
                    Err(err) => passkey_error(&tx, err),
                }
            }
            SignalMessage::DeletePasskey { id } => {
                let Some(owner_id) = require_owner(db, &mut session, &tx) else {
                    continue;
                };
                match db.delete_passkey(&owner_id, &id) {
                    Ok(_) => send_passkeys(db, &owner_id, &tx),
                    Err(err) => {
                        tracing::error!(?err, "failed to delete passkey");
                        let _ = tx.send(SignalMessage::Error {
                            message: "internal error".to_string(),
                        });
                    }
                }
            }
            SignalMessage::PasskeyLoginStart => match passkeys::login_start(webauthn) {
                Ok((options, state)) => {
                    passkey_login = Some(state);
                    let _ = tx.send(SignalMessage::PasskeyLoginOptions { options });
                }
                Err(err) => passkey_error(&tx, err),
            },
            SignalMessage::PasskeyLoginFinish { credential } => {
                let Some(state) = passkey_login.take() else {
                    tracing::warn!("passkey login finish without start");
                    passkey_failed(&tx);
                    continue;
                };
                let now = ice::now_unix() as i64;
                let owner_id = match passkeys::login_finish(webauthn, db, state, &credential, now) {
                    Ok(owner_id) => owner_id,
                    Err(err) => {
                        passkey_error(&tx, err);
                        continue;
                    }
                };
                match switch_to_account(db, &mut session, &owner_id, &user_agent, now) {
                    Ok(token) => {
                        let devices = match device_entries_for_owner(db, registry, &owner_id) {
                            Ok(devices) => devices,
                            Err(err) => {
                                tracing::error!(?err, "failed to load devices for owner");
                                Vec::new()
                            }
                        };
                        let _ = tx.send(SignalMessage::Authenticated { token, devices });
                    }
                    Err(err) => {
                        tracing::error!(?err, "failed to sign in with passkey");
                        let _ = tx.send(SignalMessage::Error {
                            message: "internal error".to_string(),
                        });
                    }
                }
            }
            SignalMessage::ConnectDevice { device_id } => {
                let Some(owner_id) = require_owner(db, &mut session, &tx) else {
                    continue;
                };

                let linked = match db.devices_for_owner(&owner_id) {
                    Ok(devices) => devices.iter().any(|d| d.device_id == device_id),
                    Err(err) => {
                        tracing::error!(?err, "failed to check device ownership");
                        let _ = tx.send(SignalMessage::Error {
                            message: "internal error".to_string(),
                        });
                        continue;
                    }
                };
                if !linked {
                    let _ = tx.send(SignalMessage::Error {
                        message: "device not linked".to_string(),
                    });
                    continue;
                }

                match registry.join_by_host_id(&device_id, tx.clone()) {
                    Ok((session_id, host_name, host_tx)) => {
                        tracing::info!(%session_id, "client connected to device");
                        let _ = tx.send(SignalMessage::Joined {
                            session_id: session_id.clone(),
                            host_name,
                            ice_servers: ice.ice_servers(ice::now_unix(), &session_id),
                            // `device_id` is the same id the client already
                            // used to ask for this connection (slice 3.5b):
                            // every registered host is a persistent device
                            // since 3.1, so this is always known here.
                            device_id: Some(device_id.clone()),
                        });
                        let _ = host_tx.send(SignalMessage::PeerJoined {
                            ice_servers: ice.ice_servers(ice::now_unix(), &session_id),
                            session_id,
                        });
                    }
                    Err(err) => {
                        let _ = tx.send(SignalMessage::Error {
                            message: err.message().to_string(),
                        });
                    }
                }
            }
            SignalMessage::RenameDevice { device_id, alias } => {
                let Some(owner_id) = require_owner(db, &mut session, &tx) else {
                    continue;
                };

                let alias = alias
                    .as_deref()
                    .map(|a| owners::truncate_chars(a, MAX_NAME_CHARS));
                match db.set_alias(&owner_id, &device_id, alias) {
                    Ok(true) => match device_entries_for_owner(db, registry, &owner_id) {
                        Ok(devices) => {
                            let _ = tx.send(SignalMessage::Devices { devices });
                        }
                        Err(err) => {
                            tracing::error!(?err, "failed to load devices for owner");
                            let _ = tx.send(SignalMessage::Error {
                                message: "internal error".to_string(),
                            });
                        }
                    },
                    Ok(false) => {
                        let _ = tx.send(SignalMessage::Error {
                            message: "device not linked".to_string(),
                        });
                    }
                    Err(err) => {
                        tracing::error!(?err, "failed to set device alias");
                        let _ = tx.send(SignalMessage::Error {
                            message: "internal error".to_string(),
                        });
                    }
                }
            }
            SignalMessage::ForgetDevice { device_id } => {
                let Some(owner_id) = require_owner(db, &mut session, &tx) else {
                    continue;
                };

                match db.unlink_device(&owner_id, &device_id) {
                    Ok(true) => match device_entries_for_owner(db, registry, &owner_id) {
                        Ok(devices) => {
                            let _ = tx.send(SignalMessage::Devices { devices });
                        }
                        Err(err) => {
                            tracing::error!(?err, "failed to load devices for owner");
                            let _ = tx.send(SignalMessage::Error {
                                message: "internal error".to_string(),
                            });
                        }
                    },
                    Ok(false) => {
                        let _ = tx.send(SignalMessage::Error {
                            message: "device not linked".to_string(),
                        });
                    }
                    Err(err) => {
                        tracing::error!(?err, "failed to unlink device");
                        let _ = tx.send(SignalMessage::Error {
                            message: "internal error".to_string(),
                        });
                    }
                }
            }
            SignalMessage::Join { pin } => {
                if !pin_limiter.check(ip) {
                    tracing::warn!(%ip, "pin attempts rate-limited");
                    let _ = tx.send(SignalMessage::Error {
                        message: "too many attempts".to_string(),
                    });
                    continue;
                }
                if !pin_global_limiter.check() {
                    tracing::warn!(%ip, "pin attempts rate-limited globally");
                    let _ = tx.send(SignalMessage::Error {
                        message: "too many attempts".to_string(),
                    });
                    continue;
                }
                match registry.join(&pin, tx.clone(), pin_ttl, Instant::now()) {
                    Ok((session_id, host_id, host_name, host_tx, new_pin)) => {
                        tracing::info!(%session_id, "client joined");
                        let _ = tx.send(SignalMessage::Joined {
                            session_id: session_id.clone(),
                            host_name,
                            ice_servers: ice.ice_servers(ice::now_unix(), &session_id),
                            // `host_id` is the device's persistent id (slice
                            // 3.1: every registered host is one) -- carried
                            // back so the client can reconnect to the same
                            // device later without a PIN (slice 3.5b).
                            device_id: Some(host_id.clone()),
                        });
                        let _ = host_tx.send(SignalMessage::PeerJoined {
                            ice_servers: ice.ice_servers(ice::now_unix(), &session_id),
                            session_id,
                        });
                        if let Some(pin) = new_pin {
                            let _ = host_tx.send(SignalMessage::PinChanged { pin });
                        }
                        match current_owner(db, &mut session) {
                            Ok(Some(owner_id)) => {
                                if let Err(err) =
                                    db.link_device(&owner_id, &host_id, ice::now_unix() as i64)
                                {
                                    tracing::error!(?err, "failed to link device to owner");
                                }
                            }
                            Ok(None) => {}
                            Err(err) => tracing::error!(?err, "failed to check owner token"),
                        }
                    }
                    Err(err) => {
                        // Any failed join is an attempt: `host busy` or
                        // `already in session` would otherwise confirm for
                        // free that the PIN is correct.
                        pin_limiter.spend(ip);
                        pin_global_limiter.spend();
                        tracing::info!(%ip, error = err.message(), "join rejected");
                        let _ = tx.send(SignalMessage::Error {
                            message: err.message().to_string(),
                        });
                    }
                }
            }
            SignalMessage::Offer {
                session_id,
                sdp,
                auth,
            } => match registry.peer_tx_for_client(&session_id, &tx) {
                Some(peer) => {
                    tracing::debug!(%session_id, "forwarding offer client -> host");
                    let _ = peer.send(SignalMessage::Offer {
                        session_id,
                        sdp,
                        auth,
                    });
                }
                None => {
                    let _ = tx.send(SignalMessage::Error {
                        message: "not in session".to_string(),
                    });
                }
            },
            SignalMessage::Answer {
                session_id,
                sdp,
                auth,
            } => match registry.peer_tx_for_client(&session_id, &tx) {
                Some(peer) => {
                    tracing::debug!(%session_id, "forwarding answer client -> host");
                    let _ = peer.send(SignalMessage::Answer {
                        session_id,
                        sdp,
                        auth,
                    });
                }
                None => {
                    let _ = tx.send(SignalMessage::Error {
                        message: "not in session".to_string(),
                    });
                }
            },
            SignalMessage::Ice {
                session_id,
                candidate,
            } => match registry.peer_tx_for_client(&session_id, &tx) {
                Some(peer) => {
                    tracing::debug!(%session_id, "forwarding ice client -> host");
                    let _ = peer.send(SignalMessage::Ice {
                        session_id,
                        candidate,
                    });
                }
                None => {
                    let _ = tx.send(SignalMessage::Error {
                        message: "not in session".to_string(),
                    });
                }
            },
            // Slice 3.2c: OPAQUE login messages the client sends toward the
            // host in response to `AuthRequired`. Same opaque-relay shape as
            // `Offer`/`Ice` above -- the server never interprets `payload`.
            SignalMessage::PakeStart {
                session_id,
                payload,
            } => match registry.peer_tx_for_client(&session_id, &tx) {
                Some(peer) => {
                    tracing::debug!(%session_id, "forwarding pake_start client -> host");
                    let _ = peer.send(SignalMessage::PakeStart {
                        session_id,
                        payload,
                    });
                }
                None => {
                    let _ = tx.send(SignalMessage::Error {
                        message: "not in session".to_string(),
                    });
                }
            },
            SignalMessage::PakeFinish {
                session_id,
                payload,
            } => match registry.peer_tx_for_client(&session_id, &tx) {
                Some(peer) => {
                    tracing::debug!(%session_id, "forwarding pake_finish client -> host");
                    let _ = peer.send(SignalMessage::PakeFinish {
                        session_id,
                        payload,
                    });
                }
                None => {
                    let _ = tx.send(SignalMessage::Error {
                        message: "not in session".to_string(),
                    });
                }
            },
            SignalMessage::Bye { session_id, reason } => {
                match registry.close_session_by_client(&session_id, &tx) {
                    Some(host_tx) => {
                        tracing::info!(%session_id, "client ended session");
                        let _ = host_tx.send(SignalMessage::Bye { session_id, reason });
                    }
                    None => {
                        // Unknown to the server -- most likely a stale
                        // `session_id` from before a signaling reconnect
                        // (slice 3.5a). The session is P2P and may well still
                        // be live; a quiet no-op instead of `Error`.
                        tracing::debug!(%session_id, "bye for a session not in session");
                    }
                }
            }
            _ => {
                let _ = tx.send(SignalMessage::Error {
                    message: "unexpected message".to_string(),
                });
                break;
            }
        }
    }

    for (session_id, _host_tx) in registry.disconnect_client(&tx) {
        // Same reasoning as the host-side disconnect above: the socket
        // closing doesn't mean the P2P session is over, so no `Bye`.
        tracing::info!(%session_id, "session left to p2p after signaling loss");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn peer() -> SocketAddr {
        "127.0.0.1:5555".parse().unwrap()
    }

    #[test]
    fn client_ip_prefers_x_real_ip_over_socket() {
        let mut headers = HeaderMap::new();
        headers.insert("x-real-ip", " 203.0.113.7 ".parse().unwrap());
        assert_eq!(
            client_ip(&headers, peer()),
            "203.0.113.7".parse::<IpAddr>().unwrap()
        );
    }

    #[test]
    fn client_ip_falls_back_to_socket_on_garbage_header() {
        let mut headers = HeaderMap::new();
        headers.insert("x-real-ip", "not-an-ip".parse().unwrap());
        assert_eq!(client_ip(&headers, peer()), peer().ip());
    }

    #[test]
    fn client_ip_uses_socket_without_header() {
        assert_eq!(client_ip(&HeaderMap::new(), peer()), peer().ip());
    }

    fn anonymous_session(db: &Db, now: i64) -> (Option<ClientSession>, String, String) {
        let (owner_id, token_id, token) =
            owners::authenticate(db, None, "UA", now).expect("authenticate");
        (
            Some(ClientSession {
                owner_id: owner_id.clone(),
                token_id,
            }),
            owner_id,
            token,
        )
    }

    #[test]
    fn switch_to_account_moves_devices_and_replaces_the_anonymous_token() {
        let db = Db::in_memory().expect("open in-memory db");
        let (mut session, anon, anon_token) = anonymous_session(&db, 10);
        db.insert_owner("account", 1).expect("account");
        db.insert_device("dev1", "h", "Mac", 1).expect("device");
        db.link_device(&anon, "dev1", 5).expect("link");
        db.set_alias(&anon, "dev1", Some("Work")).expect("alias");

        let token = switch_to_account(&db, &mut session, "account", "UA2", 20).expect("switch");

        assert_eq!(
            session.as_ref().map(|s| s.owner_id.as_str()),
            Some("account")
        );
        assert_eq!(
            db.token_by_hash(&crate::devices::hash_secret(&token))
                .expect("lookup")
                .map(|(owner, _)| owner),
            Some("account".to_string())
        );
        assert_eq!(
            db.token_by_hash(&crate::devices::hash_secret(&anon_token))
                .expect("lookup"),
            None
        );
        assert!(db.devices_for_owner(&anon).expect("anon").is_empty());
        assert_eq!(db.webauthn_user_id(&anon).expect("anon gone"), None);
        let moved = db.devices_for_owner("account").expect("account devices");
        assert_eq!(moved.len(), 1);
        assert_eq!(moved[0].alias.as_deref(), Some("Work"));
    }

    #[test]
    fn switch_to_account_keeps_an_account_owner_and_drops_only_the_current_token() {
        let db = Db::in_memory().expect("open in-memory db");
        let (mut session, other_account, old_token) = anonymous_session(&db, 10);
        db.insert_passkey("pk", &other_account, "cred", "{}", "", 1)
            .expect("passkey");
        let (_, other_token) =
            owners::issue_token(&db, &other_account, "UA", 11).expect("second token");
        db.insert_owner("account", 1).expect("account");
        db.insert_device("dev1", "h", "Mac", 1).expect("device");
        db.link_device(&other_account, "dev1", 5).expect("link");

        switch_to_account(&db, &mut session, "account", "UA2", 20).expect("switch");

        assert_eq!(db.devices_for_owner(&other_account).expect("devs").len(), 1);
        assert!(db.devices_for_owner("account").expect("devs").is_empty());
        let hash = crate::devices::hash_secret;
        assert_eq!(db.token_by_hash(&hash(&old_token)).expect("old"), None);
        assert!(db
            .token_by_hash(&hash(&other_token))
            .expect("kept")
            .is_some());
        assert_eq!(db.passkeys_for_owner(&other_account).expect("pk").len(), 1);
    }

    #[test]
    fn switch_to_account_without_a_session_or_to_the_same_owner_issues_a_new_token() {
        let db = Db::in_memory().expect("open in-memory db");
        db.insert_owner("account", 1).expect("account");
        let mut none = None;
        let token = switch_to_account(&db, &mut none, "account", "UA", 20).expect("switch");
        assert!(none.is_some());
        assert!(!token.is_empty());

        let (mut session, same, old_token) = anonymous_session(&db, 10);
        let new_token = switch_to_account(&db, &mut session, &same, "UA", 30).expect("switch");
        assert_ne!(new_token, old_token);
        let tokens = db.tokens_for_owner(&same).expect("tokens");
        assert_eq!(tokens.len(), 1, "old token deleted, new one issued");
    }
}
