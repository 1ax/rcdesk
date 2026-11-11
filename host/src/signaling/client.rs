//! The signaling WebSocket client: connecting and registering with the
//! server (`SignalingClient`, `ConnectError`, `Keepalive`), the LAN hub's
//! pinned-key TLS verification (`PinStore`, `PinVerifier`) and the raw
//! send/receive helpers over the socket.

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use futures_util::stream::{SplitSink, SplitStream};
use futures_util::{SinkExt, StreamExt};
use tokio::net::TcpStream;
use tokio::sync::{mpsc, watch};
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream};

use proto::signal::{DeviceCredentials, Role, SignalMessage};

use super::{
    handle_agent_command, handle_session_event, handle_signal_message, send_session_stats,
    AgentCommand, AgentStatus, HostContext, SessionSlot, SESSION_STATS_INTERVAL,
};

type WsStream = WebSocketStream<MaybeTlsStream<TcpStream>>;

/// Upper bound for the whole signaling handshake (TCP + WS + `Hello` +
/// `HostRegister` + waiting for `Registered`). 30 s is far beyond any honest
/// RTT, while an accept that never answers would otherwise freeze
/// `run_agent` forever.
pub const CONNECT_TIMEOUT: Duration = Duration::from_secs(30);
/// How often `SignalingClient::run` pings the signaling server, and how long
/// it tolerates silence (no incoming frame of any kind) before deciding the
/// connection is dead -- see `run`'s doc comment for why a plain
/// `read.next()` alone isn't enough. `app::run_agent` owns the value and
/// passes it through on every reconnect; `Default` is 20s/45s.
#[derive(Debug, Clone, Copy)]
pub struct Keepalive {
    pub interval: Duration,
    pub timeout: Duration,
}

impl Default for Keepalive {
    fn default() -> Self {
        Self {
            interval: Duration::from_secs(20),
            timeout: Duration::from_secs(45),
        }
    }
}
/// Файл закреплённого ключа LAN-хаба (D47-4): `<dir>/hub-pin-<slug>.txt`,
/// внутри hex SHA-256 от SubjectPublicKeyInfo сертификата хаба и перевод
/// строки. Для боевого сервера ([`crate::device::DEFAULT_SERVER`]) файла нет:
/// там работает обычная проверка по корням webpki.
#[derive(Debug, Clone)]
pub struct PinStore {
    path: std::path::PathBuf,
    /// Unix-режим записываемого файла (`0600`, для общего каталога macOS --
    /// `fullmode::SHARED_FILE_MODE`).
    mode: u32,
}

impl PinStore {
    /// Хранилище для `server` в каталоге `dir`; `None` для боевого сервера.
    pub fn new(dir: &std::path::Path, server: &str, mode: u32) -> Option<Self> {
        let slug = crate::device::server_slug(server)?;
        Some(Self {
            path: dir.join(format!("hub-pin-{slug}.txt")),
            mode,
        })
    }

    /// Хранилище рядом с файлом учётных данных `device_store` (тот же каталог
    /// и тот же режим файла).
    pub fn for_device_store(
        device_store: &crate::device::DeviceStore,
        server: &str,
    ) -> Option<Self> {
        Self::new(device_store.dir(), server, device_store.file_mode())
    }

    pub fn path(&self) -> &std::path::Path {
        &self.path
    }

    /// Сохранённый отпечаток; `None`, если файла нет, он не читается или пуст.
    pub fn load(&self) -> Option<String> {
        match std::fs::read_to_string(&self.path) {
            Ok(text) => Some(text.trim().to_string()).filter(|t| !t.is_empty()),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => None,
            Err(err) => {
                tracing::warn!(path = %self.path.display(), error = %err, "не удалось прочитать закреплённый ключ хаба");
                None
            }
        }
    }

    pub fn save(&self, fingerprint: &str) -> std::io::Result<()> {
        let text = format!("{fingerprint}\n");
        crate::fsutil::write_atomic_mode(&self.path, text.as_bytes(), self.mode)
    }
}

/// Что верификатор увидел за рукопожатие.
#[derive(Debug, Default)]
struct PinState {
    /// Отпечаток, предъявленный при первом знакомстве (закрепить после
    /// успешной регистрации).
    first_seen: Option<String>,
    /// (закреплённый, предъявленный) при несовпадении.
    mismatch: Option<(String, String)>,
}

/// Проверка сертификата сервера: сначала обычная (корни webpki), для
/// самоподписанного сертификата LAN-хаба -- закрепление ключа по первому
/// знакомству (TOFU). Подпись рукопожатия проверяется всегда.
#[derive(Debug)]
struct PinVerifier {
    webpki: Arc<rustls::client::WebPkiServerVerifier>,
    algs: rustls::crypto::WebPkiSupportedAlgorithms,
    /// Закрепление включено (сервер не боевой).
    tofu: bool,
    /// Отпечаток, сохранённый раньше.
    pinned: Option<String>,
    state: Arc<Mutex<PinState>>,
}

impl PinVerifier {
    fn new(tofu: bool, pinned: Option<String>) -> anyhow::Result<Self> {
        let roots = rustls::RootCertStore {
            roots: webpki_roots::TLS_SERVER_ROOTS.to_vec(),
        };
        let provider = Arc::new(rustls::crypto::ring::default_provider());
        let algs = provider.signature_verification_algorithms;
        let webpki =
            rustls::client::WebPkiServerVerifier::builder_with_provider(Arc::new(roots), provider)
                .build()
                .map_err(|err| {
                    anyhow::anyhow!("не удалось собрать проверку сертификатов: {err}")
                })?;
        Ok(Self {
            webpki,
            algs,
            tofu,
            pinned,
            state: Arc::new(Mutex::new(PinState::default())),
        })
    }
}

impl rustls::client::danger::ServerCertVerifier for PinVerifier {
    fn verify_server_cert(
        &self,
        end_entity: &rustls::pki_types::CertificateDer<'_>,
        intermediates: &[rustls::pki_types::CertificateDer<'_>],
        server_name: &rustls::pki_types::ServerName<'_>,
        ocsp_response: &[u8],
        now: rustls::pki_types::UnixTime,
    ) -> Result<rustls::client::danger::ServerCertVerified, rustls::Error> {
        let webpki_err = match self.webpki.verify_server_cert(
            end_entity,
            intermediates,
            server_name,
            ocsp_response,
            now,
        ) {
            Ok(ok) => return Ok(ok),
            Err(err) => err,
        };
        if !self.tofu {
            return Err(webpki_err);
        }
        // Самоподписанный сертификат хаба: имя не проверяем (IP нет в SAN),
        // решает отпечаток открытого ключа.
        let seen =
            crate::hub::cert::cert_spki_sha256_hex(end_entity.as_ref()).ok_or_else(|| {
                rustls::Error::General("не удалось разобрать сертификат сервера".to_string())
            })?;
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        match &self.pinned {
            Some(pinned) if *pinned == seen => {}
            Some(pinned) => {
                state.mismatch = Some((pinned.clone(), seen.clone()));
                return Err(rustls::Error::General(format!(
                    "ключ сервера не совпал с закреплённым: закреплён {pinned}, предъявлен {seen}"
                )));
            }
            None => state.first_seen = Some(seen),
        }
        Ok(rustls::client::danger::ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &rustls::pki_types::CertificateDer<'_>,
        dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls12_signature(message, cert, dss, &self.algs)
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &rustls::pki_types::CertificateDer<'_>,
        dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(message, cert, dss, &self.algs)
    }

    fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
        self.algs.supported_schemes()
    }
}

/// Why `SignalingClient::connect` failed. Distinguishes the one case
/// `app::run_agent` must react to specially -- the server not recognizing a
/// saved device (slice 3.1c) -- from every other failure.
#[derive(Debug, thiserror::Error)]
pub enum ConnectError {
    /// The signaling server doesn't know this device (e.g. its database was
    /// recreated) -- the caller should forget its saved credentials and
    /// register again as a new device.
    #[error("signaling server does not know this device")]
    UnknownDevice,
    #[error(transparent)]
    Other(#[from] anyhow::Error),
}

/// A registered signaling connection, holding the PIN the owner reads out to
/// pair a client.
pub struct SignalingClient {
    host_id: String,
    pin: String,
    ice_servers: Vec<proto::signal::IceServer>,
    /// Freshly issued device credentials (slice 3.1c), present only when
    /// this `connect` call was this device's very first registration -- see
    /// `issued_device`.
    issued_device: Option<DeviceCredentials>,
    write: SplitSink<WsStream, Message>,
    read: SplitStream<WsStream>,
}

impl SignalingClient {
    /// Connects to the signaling server, sends `Hello` + `HostRegister`
    /// (carrying `device`, if the caller has saved credentials from a
    /// previous registration -- slice 3.1c -- and `session_id`, if the
    /// caller's `SessionSlot` still has a live session from before this
    /// reconnect -- slice 3.2a/D35, see `SessionSlot::current_session_id`),
    /// and waits for `Registered`.
    pub async fn connect(
        url: &str,
        name: &str,
        device: Option<DeviceCredentials>,
        session_id: Option<String>,
        pin: Option<PinStore>,
    ) -> Result<Self, ConnectError> {
        Self::connect_with_timeout(url, name, device, session_id, pin, CONNECT_TIMEOUT).await
    }

    /// Same as [`Self::connect`], but the whole handshake is bounded by
    /// `timeout`.
    pub async fn connect_with_timeout(
        url: &str,
        name: &str,
        device: Option<DeviceCredentials>,
        session_id: Option<String>,
        pin: Option<PinStore>,
        timeout: Duration,
    ) -> Result<Self, ConnectError> {
        match tokio::time::timeout(timeout, Self::handshake(url, name, device, session_id, pin))
            .await
        {
            Ok(result) => result,
            Err(_elapsed) => Err(ConnectError::Other(anyhow::anyhow!(
                "signaling connect timed out after {timeout:?}"
            ))),
        }
    }

    async fn handshake(
        url: &str,
        name: &str,
        device: Option<DeviceCredentials>,
        session_id: Option<String>,
        pin_store: Option<PinStore>,
    ) -> Result<Self, ConnectError> {
        // `wss://`: своя проверка сертификата с закреплением ключа хаба (D47-4);
        // `ws://` -- как раньше, без TLS.
        let mut pin_state = None;
        let (ws_stream, _response) = if url.starts_with("wss://") {
            let verifier = PinVerifier::new(
                pin_store.is_some(),
                pin_store.as_ref().and_then(PinStore::load),
            )?;
            let state = Arc::clone(&verifier.state);
            let config = rustls::ClientConfig::builder_with_provider(Arc::new(
                rustls::crypto::ring::default_provider(),
            ))
            .with_safe_default_protocol_versions()
            .map_err(anyhow::Error::from)?
            .dangerous()
            .with_custom_certificate_verifier(Arc::new(verifier))
            .with_no_client_auth();
            let connected = tokio_tungstenite::connect_async_tls_with_config(
                url,
                None,
                false,
                Some(tokio_tungstenite::Connector::Rustls(Arc::new(config))),
            )
            .await;
            match connected {
                Ok(connected) => {
                    pin_state = Some(state);
                    connected
                }
                Err(err) => {
                    let mismatch = state
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .mismatch
                        .take();
                    if let (Some((pinned, seen)), Some(store)) = (mismatch, &pin_store) {
                        return Err(ConnectError::Other(anyhow::anyhow!(
                            "ключ хаба не совпал с закреплённым: закреплён {pinned}, предъявлен {seen}; \
                             если хаб переустановлен, удалите файл {}",
                            store.path().display()
                        )));
                    }
                    return Err(ConnectError::Other(err.into()));
                }
            }
        } else {
            tokio_tungstenite::connect_async(url)
                .await
                .map_err(anyhow::Error::from)?
        };
        let (mut write, mut read) = ws_stream.split();

        send(
            &mut write,
            &SignalMessage::Hello {
                role: Role::Host,
                version: env!("CARGO_PKG_VERSION").to_string(),
            },
        )
        .await?;
        send(
            &mut write,
            &SignalMessage::HostRegister {
                name: name.to_string(),
                device,
                session_id,
                // D44: the host understands `PinChanged` (see `apply_pin_change`).
                pin_rotation: true,
                // Слайс W: карты перечисляются при каждой (пере)регистрации --
                // адрес мог смениться.
                ifaces: crate::wake::local_ifaces(),
                // D92: ОС хоста для списка устройств и подсказок клиента.
                os: Some(crate::platform::host_os()),
            },
        )
        .await?;

        let (host_id, pin, ice_servers, issued_device) = match next_message(&mut read).await {
            Some(SignalMessage::Registered {
                host_id,
                pin,
                ice_servers,
                device,
            }) => (host_id, pin, ice_servers, device),
            Some(SignalMessage::Error { message }) => {
                if message == "unknown device" {
                    return Err(ConnectError::UnknownDevice);
                }
                return Err(ConnectError::Other(anyhow::anyhow!(
                    "signaling server rejected registration: {message}"
                )));
            }
            Some(other) => {
                return Err(ConnectError::Other(anyhow::anyhow!(
                    "unexpected reply while registering: {other:?}"
                )))
            }
            None => {
                return Err(ConnectError::Other(anyhow::anyhow!(
                    "signaling connection closed before registration completed"
                )))
            }
        };

        // Регистрация прошла -- ключ хаба можно закрепить (только теперь, чтобы
        // чужой сервер без нашего протокола ничего не записал).
        if let (Some(store), Some(state)) = (&pin_store, &pin_state) {
            let first_seen = state
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .first_seen
                .take();
            if let Some(fingerprint) = first_seen {
                match store.save(&fingerprint) {
                    Ok(()) => tracing::info!("закреплён ключ хаба {fingerprint} для {url}"),
                    Err(err) => tracing::warn!(
                        path = %store.path().display(),
                        error = %err,
                        "не удалось сохранить закреплённый ключ хаба"
                    ),
                }
            }
        }

        Ok(Self {
            host_id,
            pin,
            ice_servers,
            issued_device,
            write,
            read,
        })
    }

    pub fn pin(&self) -> &str {
        &self.pin
    }

    pub fn host_id(&self) -> &str {
        &self.host_id
    }

    /// ICE servers the signaling server sent in `Registered`. `main.rs`
    /// merges these with any `--stun` CLI overrides before building the
    /// `SessionConfig` used for every session (see `docs/dev-run.md`).
    pub fn ice_servers(&self) -> &[proto::signal::IceServer] {
        &self.ice_servers
    }

    /// Freshly issued device credentials (slice 3.1c), as `Registered` sent
    /// them -- `Some` only on this device's very first registration; on a
    /// successful re-registration with already-known credentials it's
    /// `None`, since the caller already has what it needs saved. See
    /// `app::run_agent`, which saves this via `device::DeviceStore::save`.
    pub fn issued_device(&self) -> Option<&DeviceCredentials> {
        self.issued_device.as_ref()
    }

    /// Drives the signaling connection: forwards SDP/ICE between the wire
    /// and a `PeerSession`, one session at a time. Returns once the
    /// connection is lost (an error) -- reconnecting is `app::run_agent`'s
    /// job, one layer up. `ctx` is a shared reference (see that function's
    /// doc comment for why) so it survives every reconnect; `status` is
    /// updated to `InSession`/`Registered` as the one active session's
    /// `RTCPeerConnection` connects/disconnects.
    ///
    /// `keepalive` guards against a *quiet* drop -- laptop sleep, a NAT
    /// mapping expiring, a half-open TCP connection -- none of which fail a
    /// plain `read.next()` on their own, so without this a dead connection
    /// would never surface as an `Err` and `run_agent` would never
    /// reconnect. A background task pings the server every
    /// `keepalive.interval`; the main loop tracks the time of the last
    /// *any* incoming frame (text, ping, pong -- traffic in either direction
    /// proves the socket is alive) and bails out once that's older than
    /// `keepalive.timeout`.
    ///
    /// `commands` is the tray agent's "End session" (and any future command,
    /// slice 2.6c) input, owned by `app::run_agent` across every reconnect
    /// -- see `AgentCommand`'s doc comment.
    ///
    /// `slot` is the one active session's state (`SessionSlot`), also owned
    /// by `app::run_agent` across every reconnect (slice 3.5a). Unlike
    /// before 3.5a, `run` does **not** shut an active session down when it
    /// returns -- losing signaling doesn't end a peer-to-peer session, only
    /// the ability to set up a new one until the socket comes back.
    pub async fn run(
        self,
        ctx: &HostContext,
        status: &watch::Sender<AgentStatus>,
        keepalive: Keepalive,
        commands: &mut mpsc::UnboundedReceiver<AgentCommand>,
        slot: &mut SessionSlot,
    ) -> anyhow::Result<()> {
        let SignalingClient {
            host_id,
            mut pin,
            ice_servers: registered_ice_servers,
            issued_device: _,
            mut read,
            write,
        } = self;

        let (out_tx, mut out_rx) = mpsc::unbounded_channel::<SignalMessage>();
        let writer = tokio::spawn(async move {
            let mut write = write;
            let mut ping_ticker = tokio::time::interval(keepalive.interval);
            ping_ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            // The first tick fires immediately; that's fine here (an extra,
            // harmless ping right after connecting), unlike `adapt`'s ticker
            // which specifically needs to skip it.
            loop {
                tokio::select! {
                    msg = out_rx.recv() => {
                        let Some(msg) = msg else { break };
                        let Ok(json) = serde_json::to_string(&msg) else {
                            continue;
                        };
                        if write.send(Message::text(json)).await.is_err() {
                            break;
                        }
                    }
                    _ = ping_ticker.tick() => {
                        if write.send(Message::Ping(bytes::Bytes::new())).await.is_err() {
                            break;
                        }
                    }
                }
            }
            let _ = write.close().await;
        });

        let mut last_incoming = Instant::now();
        let mut timeout_checker = tokio::time::interval(keepalive.interval);
        timeout_checker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        let mut timed_out = false;
        // Periodic traffic report to the server. Lives per connection: while
        // signaling is down the loop is not running, so reports are simply
        // skipped; the counters are cumulative, the next one catches up.
        let mut stats_ticker = tokio::time::interval_at(
            tokio::time::Instant::now() + SESSION_STATS_INTERVAL,
            SESSION_STATS_INTERVAL,
        );
        stats_ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

        loop {
            tokio::select! {
                incoming = read.next() => {
                    let Some(incoming) = incoming else { break };
                    if incoming.is_ok() {
                        last_incoming = Instant::now();
                    }
                    let msg = match incoming {
                        Ok(Message::Text(text)) => match serde_json::from_str::<SignalMessage>(text.as_str()) {
                            Ok(msg) => msg,
                            Err(err) => {
                                tracing::warn!(?err, "invalid signal message from server");
                                continue;
                            }
                        },
                        Ok(Message::Ping(_)) | Ok(Message::Pong(_)) => continue,
                        Ok(Message::Close(_)) => break,
                        Ok(_) => continue,
                        Err(err) => {
                            tracing::warn!(?err, "signaling websocket read error");
                            break;
                        }
                    };

                    if let SignalMessage::PinChanged { pin: new_pin } = msg {
                        apply_pin_change(new_pin, &mut pin, status);
                        continue;
                    }

                    // Слайс W: разбудить соседа magic packet'ом; серверу не отвечаем.
                    if let SignalMessage::SendWake { targets } = msg {
                        tokio::task::spawn_blocking(move || crate::wake::send_wake(&targets));
                        continue;
                    }

                    handle_signal_message(
                        msg,
                        ctx,
                        &registered_ice_servers,
                        &pin,
                        status,
                        &out_tx,
                        slot.event_tx.clone(),
                        &mut slot.active,
                        &mut slot.current_session_id,
                        &mut slot.pending_auth,
                        &mut slot.auth_failures,
                        &mut slot.auth_locked_until,
                    )
                    .await;
                }
                Some(event) = slot.event_rx.recv() => {
                    handle_session_event(event, ctx, &pin, status, &out_tx, &slot.event_tx, &mut slot.active, &mut slot.current_session_id, &mut slot.pending_auth).await;
                }
                Some(cmd) = commands.recv() => {
                    handle_agent_command(cmd, &pin, status, &out_tx, &mut slot.active, &mut slot.current_session_id, &mut slot.pending_auth).await;
                }
                _ = stats_ticker.tick() => {
                    if let (Some(active), Some(session_id)) =
                        (slot.active.as_ref(), slot.current_session_id.as_deref())
                    {
                        send_session_stats(&active.peer, &out_tx, session_id, false).await;
                    }
                }
                _ = timeout_checker.tick() => {
                    let silence = last_incoming.elapsed();
                    if silence >= keepalive.timeout {
                        tracing::warn!(?silence, "signaling connection timed out, no traffic");
                        timed_out = true;
                        break;
                    }
                }
            }
        }

        // Deliberately no `slot.active` shutdown here (slice 3.5a): the
        // session is peer-to-peer and outlives this signaling connection --
        // `run_agent` keeps it alive across the reconnect that follows.
        //
        // `slot.pending_auth`, on the other hand, has no P2P channel of its
        // own yet (slice 3.2c) -- the OPAQUE handshake only ever exists
        // relayed through this very socket -- so it cannot survive the
        // reconnect that follows; clear it rather than leave a stale wait
        // for `AuthTimeout` to eventually clean up on its own.
        slot.pending_auth = None;
        drop(out_tx);
        // On a dead (half-open) connection the writer's close handshake can
        // block on the socket; don't let that stall the reconnect.
        let writer_abort = writer.abort_handle();
        if tokio::time::timeout(Duration::from_secs(2), writer)
            .await
            .is_err()
        {
            writer_abort.abort();
        }

        if timed_out {
            anyhow::bail!(
                "signaling connection timed out (no traffic for {:?})",
                keepalive.timeout
            );
        }
        anyhow::bail!("signaling connection to server closed (host_id={host_id})")
    }
}

/// D44: the server issued a new PIN (after a client joined, or by TTL).
/// Updates the local PIN and re-publishes the current status with it, so the
/// tray, `serve` and `pin.txt` show the fresh code; the status phase is
/// unchanged.
fn apply_pin_change(new_pin: String, pin: &mut String, status: &watch::Sender<AgentStatus>) {
    if new_pin == *pin {
        tracing::debug!("pin_changed with the same pin, ignoring");
        return;
    }
    tracing::info!(pin = %new_pin, "pin rotated by server");
    *pin = new_pin;
    let next = match &*status.borrow() {
        AgentStatus::Registered { .. } => Some(AgentStatus::Registered { pin: pin.clone() }),
        AgentStatus::InSession { .. } => Some(AgentStatus::InSession { pin: pin.clone() }),
        AgentStatus::Connecting | AgentStatus::Reconnecting { .. } => None,
    };
    if let Some(next) = next {
        status.send_replace(next);
    }
}
async fn send(write: &mut SplitSink<WsStream, Message>, msg: &SignalMessage) -> anyhow::Result<()> {
    let json = serde_json::to_string(msg)?;
    write.send(Message::text(json)).await?;
    Ok(())
}

async fn next_message(read: &mut SplitStream<WsStream>) -> Option<SignalMessage> {
    loop {
        match read.next().await {
            Some(Ok(Message::Text(text))) => match serde_json::from_str(text.as_str()) {
                Ok(msg) => return Some(msg),
                Err(err) => {
                    tracing::warn!(?err, "invalid json from signaling server");
                    return None;
                }
            },
            Some(Ok(Message::Ping(_))) | Some(Ok(Message::Pong(_))) => continue,
            Some(Ok(Message::Close(_))) | None => return None,
            Some(Ok(_)) => return None,
            Some(Err(err)) => {
                tracing::warn!(?err, "signaling websocket read error");
                return None;
            }
        }
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    use crate::signaling::test_ctx;

    /// A server that accepts TCP + WS but never answers must not freeze
    /// `connect`: the handshake is bounded by the timeout.
    #[tokio::test(flavor = "multi_thread")]
    async fn connect_times_out_when_the_server_never_answers() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let url = format!("ws://{addr}/ws");

        tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut ws = tokio_tungstenite::accept_async(stream).await.unwrap();
            // Read everything, answer nothing, keep the connection open.
            while ws.next().await.is_some() {}
        });

        let started = Instant::now();
        let result = SignalingClient::connect_with_timeout(
            &url,
            "test-host",
            None,
            None,
            None,
            Duration::from_millis(300),
        )
        .await;
        let elapsed = started.elapsed();

        match result {
            Err(ConnectError::Other(err)) => {
                assert!(err.to_string().contains("timed out"), "got: {err}");
            }
            Err(ConnectError::UnknownDevice) => panic!("expected a timeout, got UnknownDevice"),
            Ok(_) => panic!("expected a timeout, got a connection"),
        }
        assert!(elapsed < Duration::from_secs(5), "took {elapsed:?}");
    }

    /// Slice 3.5a: `run` must not shut an active session down when it
    /// returns -- a P2P session outlives the signaling connection that set
    /// it up. Proven end to end against a fake server: register, trigger a
    /// session via `PeerJoined` (real `start_session`/`PeerSession`, same as
    /// `peer_joined_sends_bye_when_session_start_fails` above), wait for the
    /// host's `Offer` as proof it started, then drop the connection. `run`
    /// must return an error, and the slot -- the observable way to check
    /// this without reaching into `ActiveSession`'s private internals --
    /// must still report a live session.
    #[tokio::test(flavor = "multi_thread")]
    async fn run_leaves_the_slot_session_active_when_the_connection_drops() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let url = format!("ws://{addr}/ws");

        let (offer_tx, mut offer_rx) = tokio::sync::oneshot::channel::<()>();
        tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut ws = tokio_tungstenite::accept_async(stream).await.unwrap();
            let _ = ws.next().await; // Hello
            let _ = ws.next().await; // HostRegister
            let registered = serde_json::to_string(&SignalMessage::Registered {
                host_id: "host-1".to_string(),
                pin: "111111".to_string(),
                ice_servers: vec![],
                device: None,
            })
            .unwrap();
            ws.send(Message::text(registered)).await.unwrap();

            let peer_joined = serde_json::to_string(&SignalMessage::PeerJoined {
                session_id: "sess-1".to_string(),
                ice_servers: vec![],
            })
            .unwrap();
            ws.send(Message::text(peer_joined)).await.unwrap();

            // Wait for the host's Offer (proof the session actually
            // started) before dropping the connection out from under it.
            // The host also sends keepalive WS pings (and may send other
            // text messages) first -- skip everything that isn't the Offer;
            // only a closed/failed connection is a real failure.
            loop {
                match ws.next().await {
                    Some(Ok(Message::Text(text))) => {
                        if let Ok(SignalMessage::Offer { .. }) =
                            serde_json::from_str::<SignalMessage>(&text)
                        {
                            break;
                        }
                    }
                    Some(Ok(Message::Ping(_) | Message::Pong(_) | Message::Frame(_)))
                    | Some(Ok(Message::Binary(_))) => continue,
                    other => panic!("connection ended before an Offer arrived: {other:?}"),
                }
            }
            let _ = offer_tx.send(());
            drop(ws);
        });

        let client = SignalingClient::connect(&url, "test-host", None, None, None)
            .await
            .expect("connect");
        let ctx = test_ctx();
        let (status_tx, _status_rx) = watch::channel(AgentStatus::Connecting);
        let (_cmd_tx, mut cmd_rx) = mpsc::unbounded_channel::<AgentCommand>();
        let mut slot = SessionSlot::new();

        let result = client
            .run(
                &ctx,
                &status_tx,
                Keepalive::default(),
                &mut cmd_rx,
                &mut slot,
            )
            .await;
        assert!(
            result.is_err(),
            "run must return once the server drops the connection"
        );

        tokio::time::timeout(Duration::from_secs(5), &mut offer_rx)
            .await
            .expect("the fake server must report the Offer in time")
            .expect("the fake server must have received an Offer");

        assert!(
            slot.is_active(),
            "the P2P session must survive the signaling connection dropping"
        );

        if let Some(active) = slot.active.take() {
            active.shutdown().await;
        }
    }

    #[test]
    fn apply_pin_change_updates_registered_status() {
        let (status_tx, status_rx) = watch::channel(AgentStatus::Registered {
            pin: "111111".to_string(),
        });
        let mut pin = "111111".to_string();
        apply_pin_change("222222".to_string(), &mut pin, &status_tx);
        assert_eq!(pin, "222222");
        assert_eq!(
            *status_rx.borrow(),
            AgentStatus::Registered {
                pin: "222222".to_string()
            }
        );
    }

    #[test]
    fn apply_pin_change_keeps_in_session_phase() {
        let (status_tx, status_rx) = watch::channel(AgentStatus::InSession {
            pin: "111111".to_string(),
        });
        let mut pin = "111111".to_string();
        apply_pin_change("222222".to_string(), &mut pin, &status_tx);
        assert_eq!(pin, "222222");
        assert_eq!(
            *status_rx.borrow(),
            AgentStatus::InSession {
                pin: "222222".to_string()
            }
        );
    }

    #[test]
    fn apply_pin_change_does_not_touch_other_phases() {
        let (status_tx, status_rx) = watch::channel(AgentStatus::Connecting);
        let mut pin = "111111".to_string();
        apply_pin_change("222222".to_string(), &mut pin, &status_tx);
        assert_eq!(pin, "222222");
        assert_eq!(*status_rx.borrow(), AgentStatus::Connecting);
        assert!(!status_rx.has_changed().unwrap());
    }

    #[test]
    fn apply_pin_change_same_pin_is_noop() {
        let (status_tx, status_rx) = watch::channel(AgentStatus::Registered {
            pin: "111111".to_string(),
        });
        let mut pin = "111111".to_string();
        apply_pin_change("111111".to_string(), &mut pin, &status_tx);
        assert_eq!(pin, "111111");
        assert!(!status_rx.has_changed().unwrap());
    }

    /// D44: a `pin_changed` message arriving on the live socket makes `run`
    /// re-publish `Registered` with the new PIN.
    #[tokio::test(flavor = "multi_thread")]
    async fn run_applies_pin_changed_to_status() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let url = format!("ws://{addr}/ws");
        tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut ws = tokio_tungstenite::accept_async(stream).await.unwrap();
            let _ = ws.next().await; // Hello
            let _ = ws.next().await; // HostRegister
            let registered = serde_json::to_string(&SignalMessage::Registered {
                host_id: "host-1".to_string(),
                pin: "111111".to_string(),
                ice_servers: vec![],
                device: None,
            })
            .unwrap();
            ws.send(Message::text(registered)).await.unwrap();
            ws.send(Message::text(r#"{"type":"pin_changed","pin":"222222"}"#))
                .await
                .unwrap();
            // Keep the socket open until the test ends.
            while ws.next().await.is_some() {}
        });

        let client = SignalingClient::connect(&url, "test-host", None, None, None)
            .await
            .expect("connect");
        let ctx = test_ctx();
        let (status_tx, mut status_rx) = watch::channel(AgentStatus::Registered {
            pin: "111111".to_string(),
        });
        status_rx.mark_unchanged();
        let (_cmd_tx, mut cmd_rx) = mpsc::unbounded_channel::<AgentCommand>();
        let mut slot = SessionSlot::new();

        let run = client.run(
            &ctx,
            &status_tx,
            Keepalive::default(),
            &mut cmd_rx,
            &mut slot,
        );
        let wait = async {
            tokio::time::timeout(Duration::from_secs(5), status_rx.changed())
                .await
                .expect("status must change in time")
                .expect("status sender alive");
        };
        tokio::select! {
            _ = run => panic!("run must not return while the socket is open"),
            _ = wait => {}
        }
        assert_eq!(
            *status_rx.borrow(),
            AgentStatus::Registered {
                pin: "222222".to_string()
            }
        );
    }

    /// Slice 3.2a/D35: `connect` puts the `session_id` argument into
    /// `HostRegister.session_id` -- `Some` when the caller (`app::run_agent`,
    /// via `SessionSlot::current_session_id`) has a live session from before
    /// this reconnect, `None` when it doesn't.
    #[tokio::test(flavor = "multi_thread")]
    async fn connect_puts_the_session_id_argument_into_host_register() {
        async fn fake_server_expecting_session_id(
            listener: tokio::net::TcpListener,
            expected: Option<String>,
        ) {
            let (stream, _) = listener.accept().await.unwrap();
            let mut ws = tokio_tungstenite::accept_async(stream).await.unwrap();
            let _ = ws.next().await; // Hello
            let host_register = match ws.next().await {
                Some(Ok(Message::Text(text))) => {
                    serde_json::from_str::<SignalMessage>(&text).expect("valid HostRegister json")
                }
                other => panic!("expected HostRegister, got {other:?}"),
            };
            match host_register {
                SignalMessage::HostRegister {
                    session_id,
                    pin_rotation,
                    ..
                } => {
                    assert_eq!(session_id, expected);
                    assert!(pin_rotation, "host must announce pin_rotation (D44)");
                }
                other => panic!("expected host_register, got {other:?}"),
            }
            let registered = serde_json::to_string(&SignalMessage::Registered {
                host_id: "host-1".to_string(),
                pin: "111111".to_string(),
                ice_servers: vec![],
                device: None,
            })
            .unwrap();
            ws.send(Message::text(registered)).await.unwrap();
        }

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let url = format!("ws://{addr}/ws");
        let server = tokio::spawn(fake_server_expecting_session_id(
            listener,
            Some("s-1".to_string()),
        ));
        let _client =
            SignalingClient::connect(&url, "test-host", None, Some("s-1".to_string()), None)
                .await
                .expect("connect");
        server.await.expect("server task");

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let url = format!("ws://{addr}/ws");
        let server = tokio::spawn(fake_server_expecting_session_id(listener, None));
        let _client = SignalingClient::connect(&url, "test-host", None, None, None)
            .await
            .expect("connect");
        server.await.expect("server task");
    }

    // ---- Закрепление ключа хаба (D47-4) ----

    fn pin_scratch_dir(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "rcdesk-host-pin-test-{name}-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    #[test]
    fn pin_store_path_and_round_trip_with_trim() {
        let dir = pin_scratch_dir("store");
        let store = PinStore::new(&dir, "ws://127.0.0.1:8100/ws", 0o600).unwrap();
        assert_eq!(store.path(), dir.join("hub-pin-127.0.0.1_8100_ws.txt"));
        assert_eq!(store.load(), None);
        store.save(&"ab".repeat(32)).unwrap();
        assert_eq!(store.load(), Some("ab".repeat(32)));
        std::fs::write(store.path(), "  cd  \n\n").unwrap();
        assert_eq!(store.load().as_deref(), Some("cd"));
        assert!(PinStore::new(&dir, crate::device::DEFAULT_SERVER, 0o600).is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[test]
    fn pin_store_file_has_mode_0600() {
        use std::os::unix::fs::PermissionsExt as _;
        let dir = pin_scratch_dir("mode");
        let store = PinStore::new(&dir, "wss://hub.lan:8443/ws", 0o600).unwrap();
        store.save("00").unwrap();
        let mode = std::fs::metadata(store.path())
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600);
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn self_signed() -> (rustls::pki_types::CertificateDer<'static>, String) {
        let key = rcgen::KeyPair::generate().unwrap();
        let der = crate::hub::cert::generate_cert(&key, "test-pc").unwrap();
        let fp = crate::hub::cert::key_fingerprint(&key);
        (rustls::pki_types::CertificateDer::from(der), fp)
    }

    fn verify(
        verifier: &PinVerifier,
        cert: &rustls::pki_types::CertificateDer<'static>,
    ) -> Result<(), rustls::Error> {
        use rustls::client::danger::ServerCertVerifier as _;
        verifier
            .verify_server_cert(
                cert,
                &[],
                &rustls::pki_types::ServerName::try_from("127.0.0.1").unwrap(),
                &[],
                rustls::pki_types::UnixTime::now(),
            )
            .map(|_| ())
    }

    #[test]
    fn verifier_without_saved_fingerprint_accepts_and_reports_it() {
        let (cert, fp) = self_signed();
        let verifier = PinVerifier::new(true, None).unwrap();
        verify(&verifier, &cert).unwrap();
        let state = verifier.state.lock().unwrap();
        assert_eq!(state.first_seen.as_deref(), Some(fp.as_str()));
        assert!(state.mismatch.is_none());
    }

    #[test]
    fn verifier_accepts_matching_fingerprint_without_touching_the_slot() {
        let (cert, fp) = self_signed();
        let verifier = PinVerifier::new(true, Some(fp)).unwrap();
        verify(&verifier, &cert).unwrap();
        assert!(verifier.state.lock().unwrap().first_seen.is_none());
    }

    #[test]
    fn verifier_rejects_a_different_fingerprint() {
        let (cert, fp) = self_signed();
        let verifier = PinVerifier::new(true, Some("0".repeat(64))).unwrap();
        let err = verify(&verifier, &cert).unwrap_err();
        let text = err.to_string();
        assert!(
            text.contains(&fp) && text.contains(&"0".repeat(64)),
            "{text}"
        );
        assert!(verifier.state.lock().unwrap().mismatch.is_some());
    }

    #[test]
    fn verifier_for_the_production_server_rejects_self_signed() {
        let (cert, _) = self_signed();
        let verifier = PinVerifier::new(false, None).unwrap();
        assert!(verify(&verifier, &cert).is_err());
        assert!(verifier.state.lock().unwrap().first_seen.is_none());
    }
}
