//! Signaling-loop handlers: incoming signal messages, agent commands and
//! transport session events, plus session start/supersede.

use std::sync::atomic::AtomicU32;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use base64::engine::general_purpose::URL_SAFE_NO_PAD as BASE64_URL_SAFE_NO_PAD;
use base64::Engine as _;
use tokio::sync::{mpsc, watch};

use proto::control::{ControlMessage, QualityPreset};
use proto::signal::SignalMessage;
use webrtc::peer_connection::RTCPeerConnectionState;

use crate::adapt::Feedback;
use crate::clipboard::ClipboardSync;
use crate::transport::SessionEvent;

use super::control::{
    begin_update, host_info_message, input_source_message, spawn_input_source_poll,
    switch_input_source, update_available, INPUT_SOURCE_POLL_INTERVAL,
};
use super::display::{
    curtain_announce_message, displays_message, recover_pipeline, refresh_display_geometry,
    set_curtain, spawn_display_poll, switch_display, RecoveryBudget, DISPLAY_POLL_INTERVAL,
};
use super::session::{
    auth_lockout_duration, next_auth_generation, ActiveSession, DisconnectGrace, PendingAuth,
    AUTH_TIMEOUT, DISCONNECT_GRACE, MAX_AUTH_ATTEMPTS,
};
use super::start::{bye_for_start_error, peer_ice_servers, start_session};
use super::{AgentCommand, AgentStatus, HostContext};

#[allow(clippy::too_many_arguments)]
pub(super) async fn handle_signal_message(
    msg: SignalMessage,
    ctx: &HostContext,
    registered_ice_servers: &[proto::signal::IceServer],
    pin: &str,
    status: &watch::Sender<AgentStatus>,
    out_tx: &mpsc::UnboundedSender<SignalMessage>,
    event_tx: mpsc::Sender<SessionEvent>,
    active: &mut Option<ActiveSession>,
    current_session_id: &mut Option<String>,
    pending_auth: &mut Option<PendingAuth>,
    auth_failures: &mut u32,
    auth_locked_until: &mut Option<Instant>,
) {
    match msg {
        SignalMessage::PeerJoined {
            session_id,
            ice_servers,
        } => {
            // Slice 3.2c: a new `PeerJoined` supersedes any login the
            // previous joining peer was in the middle of.
            *pending_auth = None;

            let access = ctx.access_store.load();
            if let crate::access::AccessLoad::Unusable(err) = &access {
                // Fail-closed: a password may be set but can't be read, so
                // starting an unauthenticated session would be wrong.
                tracing::error!(
                    %err,
                    path = %ctx.access_store.path().display(),
                    "access record unreadable, refusing session"
                );
                let _ = out_tx.send(SignalMessage::Bye {
                    session_id,
                    reason: None,
                });
                *pending_auth = None;
            } else if matches!(access, crate::access::AccessLoad::Present(_)) {
                let generation = next_auth_generation();
                *pending_auth = Some(PendingAuth {
                    session_id: session_id.clone(),
                    ice_servers,
                    login: None,
                    generation,
                });
                tracing::info!(%session_id, "password required, waiting for client authentication");
                let _ = out_tx.send(SignalMessage::AuthRequired {
                    session_id: session_id.clone(),
                });
                let timer_tx = event_tx.clone();
                tokio::spawn(async move {
                    tokio::time::sleep(AUTH_TIMEOUT).await;
                    let _ = timer_tx
                        .send(SessionEvent::AuthTimeout {
                            session_id,
                            generation,
                        })
                        .await;
                });
            } else {
                // No password: nothing to prove, the newcomer replaces the
                // running session right away. (With a password the old
                // session lives on until the newcomer's `PakeFinish`.)
                supersede_active_session(active, current_session_id, pin, status, out_tx).await;
                begin_session(
                    ctx,
                    session_id,
                    ice_servers,
                    None,
                    registered_ice_servers,
                    out_tx,
                    event_tx,
                    active,
                    current_session_id,
                )
                .await;
            }
        }
        SignalMessage::Answer {
            session_id,
            sdp,
            auth,
        } => {
            if current_session_id.as_deref() != Some(session_id.as_str()) {
                tracing::debug!(%session_id, "answer for unknown/stale session, ignoring");
                return;
            }
            // Slice 3.2e: when this session was gated by a password, the
            // client's answer must carry a valid dtls-binding tag for its
            // own fingerprint before the host applies it -- otherwise a
            // signaling-server-in-the-middle could swap in its own answer
            // undetected. No password (`session_key: None`) -- unchanged
            // behavior from before this slice.
            if let Some(session_key) = active.as_ref().and_then(|s| s.session_key.as_ref()) {
                let ok = crate::dtls_bind::fingerprint_from_sdp(&sdp)
                    .zip(auth.as_deref())
                    .is_some_and(|(fingerprint, tag)| {
                        crate::dtls_bind::verify_auth_tag(
                            session_key.as_bytes(),
                            "answer",
                            &fingerprint,
                            tag,
                        )
                    });
                if !ok {
                    tracing::warn!(%session_id, "answer failed dtls binding check, ending session");
                    if let Some(old) = active.take() {
                        old.shutdown_reporting(out_tx, current_session_id.as_deref())
                            .await;
                        let _ = status.send(AgentStatus::Registered {
                            pin: pin.to_string(),
                        });
                    }
                    *current_session_id = None;
                    let _ = out_tx.send(SignalMessage::Bye {
                        session_id,
                        reason: None,
                    });
                    return;
                }
            }
            let set_answer_failed = match active.as_ref() {
                Some(session) => match session.peer.set_answer(sdp).await {
                    Ok(()) => false,
                    Err(err) => {
                        // Slice E3: a session that cannot apply the answer
                        // can never connect; end it like a failed binding
                        // check instead of leaving it hanging.
                        tracing::warn!(?err, "failed to apply remote answer, ending session");
                        true
                    }
                },
                None => false,
            };
            if set_answer_failed {
                if let Some(old) = active.take() {
                    old.shutdown_reporting(out_tx, current_session_id.as_deref())
                        .await;
                    let _ = status.send(AgentStatus::Registered {
                        pin: pin.to_string(),
                    });
                }
                *current_session_id = None;
                let _ = out_tx.send(SignalMessage::Bye {
                    session_id,
                    reason: None,
                });
            }
        }
        SignalMessage::Ice {
            session_id,
            candidate,
        } => {
            if current_session_id.as_deref() == Some(session_id.as_str()) {
                if let Some(active) = active.as_ref() {
                    if let Err(err) = active.peer.add_remote_ice(candidate).await {
                        tracing::warn!(?err, "failed to add remote ice candidate");
                    }
                }
            } else {
                tracing::debug!(%session_id, "ice candidate for unknown/stale session, ignoring");
            }
        }
        // Slice 3.2c: the client's half of the OPAQUE login started by this
        // `PeerJoined`'s `AuthRequired`. Never logs `payload` (an encoded
        // OPAQUE protocol message) or any derived key material, only sizes
        // where useful for debugging.
        SignalMessage::PakeStart {
            session_id,
            payload,
        } => {
            let matches_pending = pending_auth
                .as_ref()
                .is_some_and(|p| p.session_id == session_id);
            if !matches_pending {
                tracing::debug!(%session_id, "pake_start for no/unknown pending auth, ignoring");
                return;
            }

            if let Some(locked_until) = *auth_locked_until {
                let now = Instant::now();
                if locked_until > now {
                    let retry_after_secs = (locked_until - now).as_secs_f64().ceil() as u32;
                    tracing::debug!(%session_id, retry_after_secs, "pake_start throttled, too soon after a previous failed attempt");
                    let _ = out_tx.send(SignalMessage::AuthFailed {
                        session_id,
                        retry_after_secs: Some(retry_after_secs.max(1)),
                    });
                    return;
                }
            }

            // Slice 3.2c fix: this is throttling, not a hard lockout -- it
            // paces how often an attempt may be *started*, it does not
            // refuse to run this one. Past `MAX_AUTH_ATTEMPTS`, arm
            // `auth_locked_until` for the *next* `PakeStart` (checked above,
            // before this one is even counted) and fall straight through to
            // running this attempt as usual. A hard refuse-and-return here
            // (the pre-fix behavior) meant the owner could never actually
            // log in again after `MAX_AUTH_ATTEMPTS` failures: every later
            // attempt would hit its own freshly re-armed lockout before
            // ever reaching `login_start`.
            *auth_failures += 1;
            if *auth_failures > MAX_AUTH_ATTEMPTS {
                let lockout = auth_lockout_duration(*auth_failures);
                *auth_locked_until = Some(Instant::now() + lockout);
                tracing::warn!(
                    %session_id,
                    auth_failures = *auth_failures,
                    lockout_secs = lockout.as_secs(),
                    "login attempt {}, next attempt allowed in {} s",
                    *auth_failures,
                    lockout.as_secs()
                );
            }

            // Re-read rather than reuse anything cached from `PeerJoined`:
            // the owner may have cleared the password since (`password
            // clear`), and this must reflect that right away.
            let record = match ctx.access_store.load() {
                crate::access::AccessLoad::Present(record) => record,
                crate::access::AccessLoad::Missing => {
                    tracing::warn!("password file disappeared mid-auth");
                    let _ = out_tx.send(SignalMessage::AuthFailed {
                        session_id,
                        retry_after_secs: None,
                    });
                    return;
                }
                crate::access::AccessLoad::Unusable(err) => {
                    tracing::error!(
                        %err,
                        path = %ctx.access_store.path().display(),
                        "access record unreadable mid-auth, refusing"
                    );
                    let _ = out_tx.send(SignalMessage::AuthFailed {
                        session_id,
                        retry_after_secs: None,
                    });
                    return;
                }
            };

            let request_bytes = match BASE64_URL_SAFE_NO_PAD.decode(&payload) {
                Ok(bytes) => bytes,
                Err(err) => {
                    tracing::warn!(?err, "invalid base64 in pake_start payload");
                    let _ = out_tx.send(SignalMessage::AuthFailed {
                        session_id,
                        retry_after_secs: None,
                    });
                    return;
                }
            };

            match crate::access::login_start(&record, &request_bytes) {
                Ok((login, response_bytes)) => {
                    if let Some(pending) = pending_auth.as_mut() {
                        pending.login = Some(login);
                    }
                    let _ = out_tx.send(SignalMessage::PakeResponse {
                        session_id,
                        payload: BASE64_URL_SAFE_NO_PAD.encode(response_bytes),
                    });
                }
                Err(err) => {
                    tracing::warn!(?err, "opaque login_start failed");
                    let _ = out_tx.send(SignalMessage::AuthFailed {
                        session_id,
                        retry_after_secs: None,
                    });
                }
            }
        }
        // Slice 3.2c: completes the login `PakeStart` above started. Same
        // "never log the payload/key material" rule as `PakeStart`.
        SignalMessage::PakeFinish {
            session_id,
            payload,
        } => {
            let matches_pending = pending_auth
                .as_ref()
                .is_some_and(|p| p.session_id == session_id && p.login.is_some());
            if !matches_pending {
                tracing::debug!(%session_id, "pake_finish with no pending login, ignoring");
                return;
            }
            // Taken here (not just borrowed): on both a successful and a
            // failed `login_finish`, the client must start a fresh
            // `PakeStart` to try again -- `opaque_ke::ServerLogin::finish`
            // consumes its state either way.
            let login = pending_auth.as_mut().and_then(|p| p.login.take());
            let Some(login) = login else {
                return;
            };

            let finalization_bytes = match BASE64_URL_SAFE_NO_PAD.decode(&payload) {
                Ok(bytes) => bytes,
                Err(err) => {
                    tracing::warn!(?err, "invalid base64 in pake_finish payload");
                    let _ = out_tx.send(SignalMessage::AuthFailed {
                        session_id,
                        retry_after_secs: None,
                    });
                    return;
                }
            };

            match login.login_finish(&finalization_bytes) {
                Ok(session_key) => {
                    *auth_failures = 0;
                    *auth_locked_until = None;
                    let pending = pending_auth
                        .take()
                        .expect("checked Some via matches_pending above");
                    tracing::info!(%session_id, "client authenticated");
                    supersede_active_session(active, current_session_id, pin, status, out_tx).await;
                    begin_session(
                        ctx,
                        session_id,
                        pending.ice_servers,
                        Some(session_key),
                        registered_ice_servers,
                        out_tx,
                        event_tx,
                        active,
                        current_session_id,
                    )
                    .await;
                }
                Err(err) => {
                    // The wait stays armed (`pending_auth` is still `Some`,
                    // just with `login` now `None`) -- the client can retry
                    // with a fresh `PakeStart`.
                    tracing::warn!(?err, %session_id, "opaque login_finish failed");
                    let _ = out_tx.send(SignalMessage::AuthFailed {
                        session_id,
                        retry_after_secs: None,
                    });
                }
            }
        }
        SignalMessage::Bye { session_id, .. } => {
            if current_session_id.as_deref() == Some(session_id.as_str()) {
                tracing::info!(%session_id, "session ended by peer");
                if let Some(old) = active.take() {
                    old.shutdown_reporting(out_tx, current_session_id.as_deref())
                        .await;
                    let _ = status.send(AgentStatus::Registered {
                        pin: pin.to_string(),
                    });
                }
                *current_session_id = None;
            } else if pending_auth
                .as_ref()
                .is_some_and(|p| p.session_id == session_id)
            {
                tracing::info!(%session_id, "pending auth ended by peer");
                *pending_auth = None;
            }
        }
        SignalMessage::Error { message } => {
            tracing::warn!(%message, "signaling server reported an error");
        }
        other => {
            tracing::debug!(?other, "unexpected signal message, ignoring");
        }
    }
}

/// Closes the running session (if any) because a new client takes over:
/// no password set, or the newcomer just passed the login (audit M-11).
async fn supersede_active_session(
    active: &mut Option<ActiveSession>,
    current_session_id: &mut Option<String>,
    pin: &str,
    status: &watch::Sender<AgentStatus>,
    out_tx: &mpsc::UnboundedSender<SignalMessage>,
) {
    if let Some(old) = active.take() {
        tracing::info!("new client takes over while a session was active; closing the old one");
        old.shutdown_reporting(out_tx, current_session_id.as_deref())
            .await;
        let _ = status.send(AgentStatus::Registered {
            pin: pin.to_string(),
        });
    }
    *current_session_id = None;
}

/// Starts the actual `PeerSession`/video pipeline and offers it to the
/// client (slice 3.2c): factored out of `handle_signal_message`'s
/// `PeerJoined` arm so it can be called either right away (no access
/// password set) or after a successful OPAQUE login (`PakeFinish`)
/// completes. `session_key` is `Some` only in the latter case.
#[allow(clippy::too_many_arguments)]
async fn begin_session(
    ctx: &HostContext,
    session_id: String,
    ice_servers: Vec<proto::signal::IceServer>,
    session_key: Option<crate::access::SessionKey>,
    registered_ice_servers: &[proto::signal::IceServer],
    out_tx: &mpsc::UnboundedSender<SignalMessage>,
    event_tx: mpsc::Sender<SessionEvent>,
    active: &mut Option<ActiveSession>,
    current_session_id: &mut Option<String>,
) {
    let chosen_ice_servers = peer_ice_servers(&ice_servers, registered_ice_servers);
    match start_session(ctx, chosen_ice_servers, event_tx).await {
        Ok(parts) => match parts.peer.create_offer().await {
            Ok(sdp) => {
                // Slice 3.2e: when this session was gated by a password,
                // bind the offer to the OPAQUE session key -- extract this
                // host's own DTLS fingerprint from the offer it just built
                // and tag it. `create_offer` always produces an SDP with a
                // fingerprint (`webrtc-rs` always sets up DTLS), so `None`
                // here would mean something is badly wrong with the SDP;
                // treat it the same as a `create_offer` failure rather than
                // silently offering an unbound session.
                let auth = match session_key.as_ref() {
                    Some(key) => match crate::dtls_bind::fingerprint_from_sdp(&sdp) {
                        Some(fingerprint) => Some(crate::dtls_bind::auth_tag(
                            key.as_bytes(),
                            "offer",
                            &fingerprint,
                        )),
                        None => {
                            tracing::warn!(
                                "offer sdp has no dtls fingerprint, refusing to start session"
                            );
                            parts.abort().await;
                            let _ = out_tx.send(SignalMessage::Bye {
                                session_id,
                                reason: None,
                            });
                            return;
                        }
                    },
                    None => None,
                };
                let _ = out_tx.send(SignalMessage::Offer {
                    session_id: session_id.clone(),
                    sdp,
                    auth,
                });
                *current_session_id = Some(session_id);
                let session_tag = parts.peer.tag();
                let session_peer = Arc::clone(&parts.peer);
                let poll_display = parts.video.display.clone();
                let display_poll_id = Arc::new(AtomicU32::new(poll_display.id));
                *active = Some(ActiveSession {
                    peer: parts.peer,
                    video: parts.video,
                    video_tx: parts.video_tx,
                    keyframe_flag: parts.keyframe_flag,
                    pipeline_generation: parts.pipeline_generation,
                    displays: parts.displays,
                    router: parts.router,
                    last_input_drop_warn: None,
                    input_dropped: 0,
                    cursor_task: parts.cursor_task,
                    input_available: parts.input_available,
                    input_reason: parts.input_reason,
                    clipboard: parts.clipboard,
                    clipboard_task: parts.clipboard_task,
                    elevation_task: parts.elevation_task,
                    files_task: parts.files_task,
                    files_enabled: parts.files_enabled,
                    session_tag,
                    quality_preset: QualityPreset::Auto,
                    curtain: ctx.curtain.clone(),
                    curtain_windows: parts.curtain_windows,
                    last_display_switch: None,
                    pipeline_recoveries: RecoveryBudget::default(),
                    display_poll_task: spawn_display_poll(
                        session_peer.events(),
                        session_tag,
                        DISPLAY_POLL_INTERVAL,
                        Arc::clone(&ctx.poll_displays),
                        Arc::clone(&display_poll_id),
                        poll_display,
                    ),
                    display_poll_id,
                    _display_wake: parts._display_wake,
                    input_source: ctx.input_source.clone(),
                    input_source_task: ctx.input_source.clone().map(|switcher| {
                        spawn_input_source_poll(
                            Arc::clone(&session_peer),
                            switcher,
                            INPUT_SOURCE_POLL_INTERVAL,
                        )
                    }),
                    disconnect_grace: DisconnectGrace::default(),
                    session_key,
                });
            }
            Err(err) => {
                tracing::warn!(?err, "failed to create offer");
                parts.abort().await;
                let _ = out_tx.send(SignalMessage::Bye {
                    session_id,
                    reason: None,
                });
            }
        },
        Err(err) => {
            tracing::warn!(?err, "failed to start session pipeline");
            let _ = out_tx.send(bye_for_start_error(session_id, &err));
        }
    }
}

/// Handles an `AgentCommand` from outside the signaling loop (see that
/// type's doc comment). Mirrors the `SignalMessage::Bye` arm of
/// `handle_signal_message` above (same shutdown + status update), except
/// this end is the one telling the server, not the other way around --
/// hence sending `SignalMessage::Bye` here instead of just reacting to one.
pub(super) async fn handle_agent_command(
    cmd: AgentCommand,
    pin: &str,
    status: &watch::Sender<AgentStatus>,
    out_tx: &mpsc::UnboundedSender<SignalMessage>,
    active: &mut Option<ActiveSession>,
    current_session_id: &mut Option<String>,
    pending_auth: &mut Option<PendingAuth>,
) {
    match cmd {
        AgentCommand::EndSession => {
            let Some(old) = active.take() else {
                // Slice 3.2c: no `active` session yet, but the owner may
                // still be waiting on a client's OPAQUE login
                // (`pending_auth`) -- "End session" should give up on that
                // wait too, not just silently no-op, even though there's no
                // `ActiveSession` to shut down and nothing to change
                // `status` to (it was never anything but `Registered` while
                // only a login was pending).
                let Some(pending) = pending_auth.take() else {
                    tracing::debug!(
                        "EndSession command received with no active session or pending auth, ignoring"
                    );
                    return;
                };
                tracing::info!(session_id = %pending.session_id, "ending pending auth (End session command)");
                let _ = out_tx.send(SignalMessage::Bye {
                    session_id: pending.session_id,
                    reason: None,
                });
                return;
            };
            let session_id = current_session_id.take();
            tracing::info!(?session_id, "ending session (End session command)");
            old.shutdown_reporting(out_tx, session_id.as_deref()).await;
            if let Some(session_id) = session_id {
                let _ = out_tx.send(SignalMessage::Bye {
                    session_id,
                    reason: None,
                });
            }
            let _ = status.send(AgentStatus::Registered {
                pin: pin.to_string(),
            });
        }
    }
}

#[allow(clippy::too_many_arguments)]
pub(super) async fn handle_session_event(
    event: SessionEvent,
    ctx: &HostContext,
    pin: &str,
    status: &watch::Sender<AgentStatus>,
    out_tx: &mpsc::UnboundedSender<SignalMessage>,
    event_tx: &mpsc::Sender<SessionEvent>,
    active: &mut Option<ActiveSession>,
    current_session_id: &mut Option<String>,
    pending_auth: &mut Option<PendingAuth>,
) {
    match event {
        SessionEvent::LocalIce(candidate) => {
            if let Some(session_id) = current_session_id.clone() {
                let _ = out_tx.send(SignalMessage::Ice {
                    session_id,
                    candidate,
                });
            }
        }
        SessionEvent::ConnectionState { state, tag } => {
            // Fix to slice 3.5b: during automatic client reconnection (bye
            // -> `connect_device` -> `PeerJoined`, all while the old
            // `PeerConnection` is still tearing down), a state change from
            // the *old*, already superseded session can land here after a
            // new session has already taken `active`'s place -- e.g. its
            // `Closed` arriving right as the new session is starting up.
            // Acting on it would wrongly end the new session, so it's
            // dropped unless it's tagged with the session that's actually
            // current right now.
            if !active.as_ref().is_some_and(|a| a.session_tag == tag) {
                tracing::debug!(
                    ?state,
                    tag,
                    "connection state event from a superseded session, ignoring"
                );
                return;
            }
            tracing::info!(?state, "peer connection state changed");
            if state == RTCPeerConnectionState::Connected {
                // Slice 3.5b: also cancels any disconnect grace window in
                // progress -- the connection recovered on its own.
                if let Some(active) = active.as_mut() {
                    active.disconnect_grace.connected();
                    let _ = status.send(AgentStatus::InSession {
                        pin: pin.to_string(),
                    });
                }
            } else if state == RTCPeerConnectionState::Disconnected {
                // Slice 3.5b: `Disconnected` can be a brief ICE hiccup (a
                // Wi-Fi roam, a dropped packet train) that recovers to
                // `Connected` on its own -- unlike `Failed`/`Closed`, it
                // does not end the session right away. Arm a
                // `DISCONNECT_GRACE` window instead: `Connected` above
                // cancels it if it arrives first; otherwise
                // `DisconnectTimeout` below ends the session the same way
                // `Failed`/`Closed` do. Agent status deliberately stays
                // `InSession` for the whole window -- no flicker on a
                // hiccup that self-heals.
                if let (Some(active), Some(session_id)) =
                    (active.as_mut(), current_session_id.clone())
                {
                    if let Some(generation) = active.disconnect_grace.disconnected() {
                        let timer_tx = event_tx.clone();
                        tokio::spawn(async move {
                            tokio::time::sleep(DISCONNECT_GRACE).await;
                            let _ = timer_tx
                                .send(SessionEvent::DisconnectTimeout {
                                    session_id,
                                    generation,
                                })
                                .await;
                        });
                    }
                }
            } else if matches!(
                state,
                RTCPeerConnectionState::Failed | RTCPeerConnectionState::Closed
            ) {
                if let Some(old) = active.take() {
                    old.shutdown_reporting(out_tx, current_session_id.as_deref())
                        .await;
                    let _ = status.send(AgentStatus::Registered {
                        pin: pin.to_string(),
                    });
                }
                // Slice 3.2a/D35: the session just ended on this end
                // (`Failed`/`Closed`), not via a `Bye` from the peer or an
                // `EndSession` command -- tell the server so it doesn't keep
                // the device marked busy for a session that's actually over.
                if let Some(session_id) = current_session_id.take() {
                    let _ = out_tx.send(SignalMessage::Bye {
                        session_id,
                        reason: None,
                    });
                }
            }
        }
        SessionEvent::DisconnectTimeout {
            session_id,
            generation,
        } => {
            let matches_current = current_session_id.as_deref() == Some(session_id.as_str());
            let matches_pending = active
                .as_ref()
                .is_some_and(|a| a.disconnect_grace.is_current(generation));
            if matches_current && matches_pending {
                tracing::info!(%session_id, "disconnect grace window expired, ending session");
                if let Some(old) = active.take() {
                    old.shutdown_reporting(out_tx, current_session_id.as_deref())
                        .await;
                    let _ = status.send(AgentStatus::Registered {
                        pin: pin.to_string(),
                    });
                }
                // Slice 3.2a/D35: same reasoning as `Failed`/`Closed` above
                // -- the session ended locally, tell the server.
                if let Some(session_id) = current_session_id.take() {
                    let _ = out_tx.send(SignalMessage::Bye {
                        session_id,
                        reason: None,
                    });
                }
            } else {
                tracing::debug!(%session_id, generation, "stale disconnect timeout, ignoring");
            }
        }
        // Slice 3.2c: a `PendingAuth` (armed on `AuthRequired`) waited out
        // `AUTH_TIMEOUT` without the client completing its login.
        SessionEvent::AuthTimeout {
            session_id,
            generation,
        } => {
            let matches_pending = pending_auth
                .as_ref()
                .is_some_and(|p| p.session_id == session_id && p.generation == generation);
            if matches_pending {
                tracing::info!(%session_id, "auth wait timed out, ending pending session");
                *pending_auth = None;
                let _ = out_tx.send(SignalMessage::Bye {
                    session_id,
                    reason: None,
                });
            } else {
                tracing::debug!(%session_id, generation, "stale auth timeout, ignoring");
            }
        }
        SessionEvent::DataChannelClosed { label, tag } => {
            if label != "control" {
                tracing::debug!(label, "non-control data channel closed");
            } else if active.as_ref().is_some_and(|a| a.session_tag == tag) {
                // Slice 3.5b: the `control` channel closing is a
                // deliberate end of session from the client (closing the
                // tab, `pc.close()`) -- end it immediately instead of
                // waiting for the `Disconnected` grace window. Guarded with
                // `active.take()` (a no-op if already `None`) since our own
                // `ActiveSession::shutdown` closing the peer connection also
                // closes this same channel and can deliver this event again
                // after the slot is already empty.
                if let Some(old) = active.take() {
                    tracing::info!("control data channel closed by remote peer, ending session");
                    old.shutdown_reporting(out_tx, current_session_id.as_deref())
                        .await;
                    let _ = status.send(AgentStatus::Registered {
                        pin: pin.to_string(),
                    });
                }
                // Slice 3.2a/D35: same reasoning as `Failed`/`Closed` above
                // -- the session ended locally, tell the server.
                if let Some(session_id) = current_session_id.take() {
                    let _ = out_tx.send(SignalMessage::Bye {
                        session_id,
                        reason: None,
                    });
                }
            } else {
                // Fix to slice 3.5b: same reasoning as `ConnectionState`'s
                // tag check above -- this is the *old* session's `control`
                // channel finishing its close handshake after a new session
                // has already taken `active`'s place (e.g. during automatic
                // client reconnection), not the current session's.
                tracing::debug!(
                    label,
                    tag,
                    "control data channel closed event from a superseded session, ignoring"
                );
            }
        }
        SessionEvent::DisplayGeometryChanged { tag } => {
            // V-2: опрос геометрии дисплея; чужой (вытесненный) сеанс -- молча.
            if let Some(a) = active.as_mut().filter(|a| a.session_tag == tag) {
                refresh_display_geometry(ctx, a).await;
            }
        }
        SessionEvent::VideoFailed { tag } => {
            // V-2: сначала пробуем перезапустить конвейер (смена разрешения,
            // сон, TDR), и только при исчерпанном бюджете/ошибке -- конец сеанса.
            if let Some(a) = active.as_mut().filter(|a| a.session_tag == tag) {
                if recover_pipeline(ctx, a).await {
                    return;
                }
            }
            if active.as_ref().is_some_and(|a| a.session_tag == tag) {
                if let Some(old) = active.take() {
                    tracing::warn!("video pipeline failed, ending session");
                    old.shutdown_reporting(out_tx, current_session_id.as_deref())
                        .await;
                    let _ = status.send(AgentStatus::Registered {
                        pin: pin.to_string(),
                    });
                }
                if let Some(session_id) = current_session_id.take() {
                    let _ = out_tx.send(SignalMessage::Bye {
                        session_id,
                        reason: Some(proto::signal::ByeReason::VideoFailed),
                    });
                }
            } else {
                tracing::debug!(
                    tag,
                    "video failed event from a superseded session, ignoring"
                );
            }
        }
        SessionEvent::DataChannelMessage {
            label,
            data,
            is_string,
        } => match label.as_str() {
            "input" | "pointer" => {
                if !is_string {
                    tracing::warn!(label, "non-text message on input channel, ignoring");
                } else {
                    match serde_json::from_slice::<proto::input::InputMessage>(&data) {
                        // `ClipboardText` is never routed through
                        // `InputRouter` like every other input message
                        // (slice 2.5b); it is applied on a blocking-pool
                        // thread so a busy clipboard watcher cannot stall
                        // this loop (M-7). Never
                        // logged with its text -- length only (privacy).
                        Ok(proto::input::InputMessage::ClipboardText { text }) => {
                            if label == "pointer" {
                                tracing::warn!(
                                    label,
                                    len = text.len(),
                                    "clipboard_text on the unordered pointer channel, ignoring"
                                );
                            } else if let Some(active) = active.as_ref() {
                                apply_remote_clipboard_text(active.clipboard.clone(), text);
                            }
                        }
                        Ok(msg) => {
                            if let Some(active) = active.as_mut() {
                                match active.router.push(msg) {
                                    Ok(()) => {}
                                    Err(crate::input::PushError::Closed) => {
                                        tracing::warn!(label, "input router is no longer running");
                                    }
                                    Err(err) => {
                                        // Throttled: at most one warning per second
                                        // per session, with a count of what was dropped.
                                        active.input_dropped += 1;
                                        let now = Instant::now();
                                        let due = active.last_input_drop_warn.is_none_or(|t| {
                                            now.duration_since(t) >= Duration::from_secs(1)
                                        });
                                        if due {
                                            tracing::warn!(
                                                label,
                                                ?err,
                                                dropped = active.input_dropped,
                                                "input events dropped"
                                            );
                                            active.input_dropped = 0;
                                            active.last_input_drop_warn = Some(now);
                                        }
                                    }
                                }
                            }
                        }
                        Err(err) => {
                            tracing::warn!(label, ?err, "failed to parse input message");
                        }
                    }
                }
            }
            "control" => {
                if !is_string {
                    tracing::warn!(label, "non-text message on control channel, ignoring");
                } else {
                    match serde_json::from_slice::<ControlMessage>(&data) {
                        Ok(ControlMessage::Ping { ts }) => {
                            if let Some(active) = active.as_ref() {
                                if let Err(err) =
                                    active.peer.send_control(&ControlMessage::Pong { ts }).await
                                {
                                    tracing::warn!(?err, "failed to send pong");
                                }
                            }
                        }
                        Ok(ControlMessage::SelectDisplay { id }) => {
                            if let Some(active) = active.as_mut() {
                                switch_display(ctx, active, id).await;
                            }
                        }
                        Ok(ControlMessage::SetQuality { preset }) => {
                            if let Some(active) = active.as_mut() {
                                active.quality_preset = preset;
                                if let Some(adapt_tx) = &active.video.adapt_tx {
                                    let _ = adapt_tx.send(Feedback::Preset { preset });
                                } else {
                                    tracing::debug!(
                                        label,
                                        ?preset,
                                        "set_quality received with adaptation disabled (--no-adapt), ignoring"
                                    );
                                }
                            }
                        }
                        Ok(ControlMessage::SetCurtain { on }) => {
                            if let Some(active) = active.as_mut() {
                                let status = set_curtain(ctx, active, on).await;
                                if let Err(err) = active.peer.send_control(&status).await {
                                    tracing::warn!(?err, "failed to send curtain status");
                                }
                            }
                        }
                        Ok(ControlMessage::SwitchInputSource) => {
                            if let Some(active) = active.as_ref() {
                                if let Some(msg) =
                                    switch_input_source(active.input_source.as_deref())
                                {
                                    if let Err(err) = active.peer.send_control(&msg).await {
                                        tracing::warn!(?err, "failed to send input source");
                                    }
                                }
                            }
                        }
                        Ok(ControlMessage::SendCtrlAltDel) => {
                            if active.is_some() && ctx.machine_mode {
                                // Помощник лишь выставляет событие службе (`SendSAS`
                                // вызывает она); syscall блокирующий.
                                tokio::task::spawn_blocking(crate::platform::sas::request);
                            } else {
                                tracing::debug!(
                                    label,
                                    "send_ctrl_alt_del without active session or machine mode, ignoring"
                                );
                            }
                        }
                        Ok(ControlMessage::UpdateHost) => {
                            if let Some(active) = active.as_ref() {
                                match begin_update(ctx) {
                                    Err(refusal) => {
                                        if let Err(err) = active.peer.send_control(&refusal).await {
                                            tracing::warn!(?err, "failed to send update status");
                                        }
                                    }
                                    Ok(mut rx) => {
                                        // Статусы уходят в control-канал, пока
                                        // жив сеанс; задача обновления от этого
                                        // не зависит.
                                        let peer = Arc::clone(&active.peer);
                                        tokio::spawn(async move {
                                            while let Some(msg) = rx.recv().await {
                                                if peer.send_control(&msg).await.is_err() {
                                                    break;
                                                }
                                            }
                                        });
                                    }
                                }
                            }
                        }
                        Ok(ControlMessage::ClipboardText { text }) => {
                            // Wrong direction: `ClipboardText` is host ->
                            // client only on `control` (see
                            // `proto::control::ControlMessage::ClipboardText`'s
                            // doc comment; the client -> host direction is
                            // `proto::input::InputMessage::ClipboardText` on
                            // `input`, handled above). Named explicitly
                            // (rather than falling into the catch-all below)
                            // so its text never reaches a log line, even at
                            // `trace` (privacy).
                            tracing::warn!(
                                label,
                                len = text.len(),
                                "clipboard_text on control channel, ignoring (wrong direction)"
                            );
                        }
                        Ok(other) => {
                            tracing::trace!(label, ?other, "unexpected control message, ignoring");
                        }
                        Err(err) => {
                            tracing::warn!(label, ?err, "failed to parse control message");
                        }
                    }
                }
            }
            _ => {
                tracing::trace!(label, len = data.len(), is_string, "data channel message");
            }
        },
        SessionEvent::KeyframeRequested => {
            tracing::debug!("keyframe requested by remote peer (PLI/FIR)");
        }
        SessionEvent::Remb { bitrate_bps } => {
            tracing::debug!(bitrate_bps, "remb from peer");
            if let Some(active) = active.as_ref() {
                if let Some(adapt_tx) = &active.video.adapt_tx {
                    let _ = adapt_tx.send(Feedback::Remb { bitrate_bps });
                }
            }
        }
        SessionEvent::ReceiverReport {
            fraction_lost,
            total_lost,
            rtt,
            jitter,
            ..
        } => {
            tracing::debug!(fraction_lost, ?rtt, "receiver report from peer");
            if let Some(active) = active.as_ref() {
                if let Some(adapt_tx) = &active.video.adapt_tx {
                    let _ = adapt_tx.send(Feedback::Loss {
                        fraction: fraction_lost,
                        total_lost,
                    });
                    let _ = adapt_tx.send(Feedback::ReceiverStats {
                        rtt,
                        jitter_ms: jitter as f32 / 90.0,
                    });
                }
            }
        }
        SessionEvent::DataChannelOpen { label } => match (label.as_str(), active.as_ref()) {
            ("control", Some(active)) => {
                let msg = displays_message(&active.displays, active.video.display.id);
                match active.peer.send_control(&msg).await {
                    Ok(true) => {}
                    Ok(false) => {
                        tracing::warn!(
                            "control channel not open yet, dropped initial displays list"
                        );
                    }
                    Err(err) => {
                        tracing::warn!(?err, "failed to send initial displays list");
                    }
                }
                let host_info = host_info_message(
                    active.files_enabled,
                    active.curtain.is_some(),
                    active.input_source.is_some(),
                    update_available(ctx),
                    ctx.machine_mode && crate::platform::sas::SUPPORTED,
                );
                match active.peer.send_control(&host_info).await {
                    Ok(true) => {}
                    Ok(false) => {
                        tracing::warn!("control channel not open yet, dropped host info");
                    }
                    Err(err) => {
                        tracing::warn!(?err, "failed to send host info");
                    }
                }
                if let Some(msg) = curtain_announce_message(&active.curtain_windows) {
                    match active.peer.send_control(&msg).await {
                        Ok(true) => {}
                        Ok(false) => {
                            tracing::warn!("control channel not open yet, dropped curtain status");
                        }
                        Err(err) => {
                            tracing::warn!(?err, "failed to send curtain status");
                        }
                    }
                }
                if let Some(msg) = active
                    .input_source
                    .as_deref()
                    .and_then(input_source_message)
                {
                    if let Err(err) = active.peer.send_control(&msg).await {
                        tracing::warn!(?err, "failed to send initial input source");
                    }
                }
                let status = ControlMessage::InputStatus {
                    available: active.input_available,
                    reason: active.input_reason.clone(),
                };
                match active.peer.send_control(&status).await {
                    Ok(true) => {}
                    Ok(false) => {
                        tracing::warn!(
                            "control channel not open yet, dropped initial input status"
                        );
                    }
                    Err(err) => {
                        tracing::warn!(?err, "failed to send initial input status");
                    }
                }
            }
            _ => {
                tracing::trace!(label, "data channel open");
            }
        },
    }
}

/// Applies an incoming `InputMessage::ClipboardText` (client -> host) to the
/// session's clipboard, if clipboard sync is available for it. The mutex is
/// also held by the clipboard watcher thread during `poll()` (on Windows up
/// to ~1 s), so the work runs on `spawn_blocking` and is not awaited: the
/// signaling loop must not stall (M-7). `clipboard` is `None` when
/// `HostContext::build_clipboard` was `None` (`serve --no-clipboard`) or the
/// builder failed for this session; `text` is never logged (privacy).
fn apply_remote_clipboard_text(clipboard: Option<Arc<Mutex<ClipboardSync>>>, text: String) {
    match clipboard {
        Some(sync) => {
            drop(tokio::task::spawn_blocking(move || {
                let mut sync = sync.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
                sync.apply_remote(&text);
            }));
        }
        None => {
            tracing::debug!(
                len = text.len(),
                "clipboard sync unavailable for this session, ignoring incoming clipboard text"
            );
        }
    }
}
