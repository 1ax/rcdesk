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
use proto::signal::{Role, SignalMessage};
use tokio::sync::mpsc;

use crate::app::AppState;
use crate::db::Db;
use crate::devices::{self, DeviceAuth};
use crate::ice::{self, IceConfig};
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
        Role::Client => run_client(stream, tx, registry, ice).await,
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
        if let Some((session_id, client_tx)) = displaced.session {
            let _ = client_tx.send(SignalMessage::Bye { session_id });
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
                        let _ = tx.send(SignalMessage::Error {
                            message: "not in session".to_string(),
                        });
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
    if let Some((session_id, client_tx)) = registry.unregister_host(&host_id, &tx) {
        let _ = client_tx.send(SignalMessage::Bye { session_id });
    }
}

async fn run_client(
    stream: &mut SplitStream<WebSocket>,
    tx: Tx,
    registry: &Registry,
    ice: &IceConfig,
) {
    loop {
        let msg = match next_message(stream, &tx).await {
            Some(msg) => msg,
            None => break,
        };

        match msg {
            SignalMessage::Join { pin } => match registry.join(&pin, tx.clone()) {
                Ok((session_id, host_name, host_tx)) => {
                    tracing::info!(%session_id, "client joined");
                    let _ = tx.send(SignalMessage::Joined {
                        session_id: session_id.clone(),
                        host_name,
                        ice_servers: ice.ice_servers(ice::now_unix()),
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
                        let _ = tx.send(SignalMessage::Error {
                            message: "not in session".to_string(),
                        });
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

    if let Some((session_id, host_tx)) = registry.disconnect_client(&tx) {
        tracing::info!(%session_id, "client disconnected");
        let _ = host_tx.send(SignalMessage::Bye { session_id });
    }
}
