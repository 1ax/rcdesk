//! `/ws` handler: the signaling protocol over one WebSocket connection.
//!
//! Every connection gets a read task (this module's `run_connection` and
//! friends, driven from the `on_upgrade` future) and an outgoing
//! `mpsc::UnboundedSender<SignalMessage>` paired with a dedicated writer task
//! that serializes messages to the socket. Other connections push messages to
//! this one purely by holding a clone of its `Tx`.

use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::State;
use axum::response::IntoResponse;
use futures_util::stream::SplitStream;
use futures_util::{SinkExt, StreamExt};
use proto::signal::{DeviceEntry, Role, SignalMessage};
use tokio::sync::mpsc;

use crate::app::AppState;
use crate::db::Db;
use crate::devices::{self, DeviceAuth};
use crate::ice::{self, IceConfig};
use crate::owners;
use crate::registry::{Registry, Tx};

pub async fn ws_handler(ws: WebSocketUpgrade, State(state): State<AppState>) -> impl IntoResponse {
    ws.on_upgrade(move |socket| handle_socket(socket, state))
}

async fn handle_socket(socket: WebSocket, state: AppState) {
    let (mut sink, mut stream) = socket.split();
    let (tx, mut rx) = mpsc::unbounded_channel::<SignalMessage>();

    let writer = tokio::spawn(async move {
        while let Some(msg) = rx.recv().await {
            let json = match serde_json::to_string(&msg) {
                Ok(json) => json,
                Err(err) => {
                    tracing::error!(?err, "failed to serialize outgoing signal message");
                    continue;
                }
            };
            if sink.send(Message::Text(json.into())).await.is_err() {
                break;
            }
        }
        let _ = sink.close().await;
    });

    tracing::info!("websocket connection opened");
    run_connection(
        &mut stream,
        tx.clone(),
        &state.registry,
        &state.ice,
        &state.db,
    )
    .await;
    tracing::info!("websocket connection closed");

    // Drop our own sender clone so the writer task's channel closes once no
    // other connection (via the registry) is still holding a clone either.
    drop(tx);
    let _ = writer.await;
}

/// Reads the next text frame and parses it as a `SignalMessage`. Returns
/// `None` when the connection is over (closed, transport error) or the frame
/// was invalid JSON / not text (an `Error` reply is sent for the latter).
/// Ping/Pong control frames are transparently skipped; axum answers Pings
/// automatically at the transport level.
async fn next_message(stream: &mut SplitStream<WebSocket>, tx: &Tx) -> Option<SignalMessage> {
    loop {
        match stream.next().await {
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
                tracing::debug!(?err, "websocket read error");
                return None;
            }
        }
    }
}

async fn run_connection(
    stream: &mut SplitStream<WebSocket>,
    tx: Tx,
    registry: &Registry,
    ice: &IceConfig,
    db: &Db,
) {
    let role = match next_message(stream, &tx).await {
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
        Role::Host => run_host(stream, tx, registry, ice, db).await,
        Role::Client => run_client(stream, tx, registry, ice, db).await,
    }
}

async fn run_host(
    stream: &mut SplitStream<WebSocket>,
    tx: Tx,
    registry: &Registry,
    ice: &IceConfig,
    db: &Db,
) {
    let (name, device) = match next_message(stream, &tx).await {
        Some(SignalMessage::HostRegister { name, device }) => (name, device),
        Some(_) => {
            let _ = tx.send(SignalMessage::Error {
                message: "expected host_register".to_string(),
            });
            return;
        }
        None => return,
    };

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
        } => (device_id, Some(credentials)),
        DeviceAuth::Known { device_id } => (device_id, None),
    };

    let (pin, displaced) = registry.register_host(host_id.clone(), name, tx.clone());
    tracing::info!(host_id = %host_id, "host registered");
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
        ice_servers: ice.ice_servers(ice::now_unix()),
        device: issued_credentials,
    });

    loop {
        let msg = match next_message(stream, &tx).await {
            Some(msg) => msg,
            None => break,
        };

        match msg {
            SignalMessage::Offer { session_id, sdp } => {
                match registry.peer_tx_for_host(&host_id, &session_id) {
                    Some(peer) => {
                        tracing::debug!(%session_id, "forwarding offer host -> client");
                        let _ = peer.send(SignalMessage::Offer { session_id, sdp });
                    }
                    None => {
                        let _ = tx.send(SignalMessage::Error {
                            message: "not in session".to_string(),
                        });
                    }
                }
            }
            SignalMessage::Answer { session_id, sdp } => {
                match registry.peer_tx_for_host(&host_id, &session_id) {
                    Some(peer) => {
                        tracing::debug!(%session_id, "forwarding answer host -> client");
                        let _ = peer.send(SignalMessage::Answer { session_id, sdp });
                    }
                    None => {
                        let _ = tx.send(SignalMessage::Error {
                            message: "not in session".to_string(),
                        });
                    }
                }
            }
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
            SignalMessage::Bye { session_id } => {
                match registry.close_session_by_host(&host_id, &session_id) {
                    Some(client_tx) => {
                        tracing::info!(%session_id, "host ended session");
                        let _ = client_tx.send(SignalMessage::Bye { session_id });
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

async fn run_client(
    stream: &mut SplitStream<WebSocket>,
    tx: Tx,
    registry: &Registry,
    ice: &IceConfig,
    db: &Db,
) {
    // The owner this connection has authenticated as (slice 3.1), if any --
    // `ClientAuth` is optional, so an older client that never sends it stays
    // fully compatible, just without device linking/listing.
    let mut owner: Option<String> = None;

    loop {
        let msg = match next_message(stream, &tx).await {
            Some(msg) => msg,
            None => break,
        };

        match msg {
            SignalMessage::ClientAuth { token } => {
                match owners::authenticate(db, token.as_deref(), ice::now_unix() as i64) {
                    Ok((owner_id, token)) => {
                        let devices = match device_entries_for_owner(db, registry, &owner_id) {
                            Ok(devices) => devices,
                            Err(err) => {
                                tracing::error!(?err, "failed to load devices for owner");
                                Vec::new()
                            }
                        };
                        owner = Some(owner_id);
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
            SignalMessage::ListDevices => match &owner {
                Some(owner_id) => match device_entries_for_owner(db, registry, owner_id) {
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
                None => {
                    let _ = tx.send(SignalMessage::Error {
                        message: "not authenticated".to_string(),
                    });
                }
            },
            SignalMessage::ConnectDevice { device_id } => {
                let Some(owner_id) = owner.clone() else {
                    let _ = tx.send(SignalMessage::Error {
                        message: "not authenticated".to_string(),
                    });
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
                            ice_servers: ice.ice_servers(ice::now_unix()),
                            // `device_id` is the same id the client already
                            // used to ask for this connection (slice 3.5b):
                            // every registered host is a persistent device
                            // since 3.1, so this is always known here.
                            device_id: Some(device_id.clone()),
                        });
                        let _ = host_tx.send(SignalMessage::PeerJoined {
                            session_id,
                            ice_servers: ice.ice_servers(ice::now_unix()),
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
                let Some(owner_id) = owner.clone() else {
                    let _ = tx.send(SignalMessage::Error {
                        message: "not authenticated".to_string(),
                    });
                    continue;
                };

                match db.set_alias(&owner_id, &device_id, alias.as_deref()) {
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
                let Some(owner_id) = owner.clone() else {
                    let _ = tx.send(SignalMessage::Error {
                        message: "not authenticated".to_string(),
                    });
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
            SignalMessage::Join { pin } => match registry.join(&pin, tx.clone()) {
                Ok((session_id, host_id, host_name, host_tx)) => {
                    tracing::info!(%session_id, "client joined");
                    let _ = tx.send(SignalMessage::Joined {
                        session_id: session_id.clone(),
                        host_name,
                        ice_servers: ice.ice_servers(ice::now_unix()),
                        // `host_id` is the device's persistent id (slice
                        // 3.1: every registered host is one) -- carried
                        // back so the client can reconnect to the same
                        // device later without a PIN (slice 3.5b).
                        device_id: Some(host_id.clone()),
                    });
                    let _ = host_tx.send(SignalMessage::PeerJoined {
                        session_id,
                        ice_servers: ice.ice_servers(ice::now_unix()),
                    });
                    if let Some(owner_id) = &owner {
                        if let Err(err) = db.link_device(owner_id, &host_id, ice::now_unix() as i64)
                        {
                            tracing::error!(?err, "failed to link device to owner");
                        }
                    }
                }
                Err(err) => {
                    let _ = tx.send(SignalMessage::Error {
                        message: err.message().to_string(),
                    });
                }
            },
            SignalMessage::Offer { session_id, sdp } => {
                match registry.peer_tx_for_client(&session_id, &tx) {
                    Some(peer) => {
                        tracing::debug!(%session_id, "forwarding offer client -> host");
                        let _ = peer.send(SignalMessage::Offer { session_id, sdp });
                    }
                    None => {
                        let _ = tx.send(SignalMessage::Error {
                            message: "not in session".to_string(),
                        });
                    }
                }
            }
            SignalMessage::Answer { session_id, sdp } => {
                match registry.peer_tx_for_client(&session_id, &tx) {
                    Some(peer) => {
                        tracing::debug!(%session_id, "forwarding answer client -> host");
                        let _ = peer.send(SignalMessage::Answer { session_id, sdp });
                    }
                    None => {
                        let _ = tx.send(SignalMessage::Error {
                            message: "not in session".to_string(),
                        });
                    }
                }
            }
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
            SignalMessage::Bye { session_id } => {
                match registry.close_session_by_client(&session_id, &tx) {
                    Some(host_tx) => {
                        tracing::info!(%session_id, "client ended session");
                        let _ = host_tx.send(SignalMessage::Bye { session_id });
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

    if let Some((session_id, _host_tx)) = registry.disconnect_client(&tx) {
        // Same reasoning as the host-side disconnect above: the socket
        // closing doesn't mean the P2P session is over, so no `Bye`.
        tracing::info!(%session_id, "session left to p2p after signaling loss");
    }
}
