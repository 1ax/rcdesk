mod auth;
mod auth_pending;
mod commands;
mod curtain;
mod grace;
mod start;

use super::control::begin_update;
use super::display::{
    curtain_announce_message, displays_message, set_curtain, switch_display, PIPELINE_RECOVERY_MAX,
};
use super::session::PendingAuth;
use super::start::{bye_for_start_error, start_session, start_session_with_display_wait};
use super::*;
use crate::encode::EncodedFrame;
use crate::transport::{PeerSession, SessionEvent};
use base64::engine::general_purpose::URL_SAFE_NO_PAD as BASE64_URL_SAFE_NO_PAD;
use base64::Engine as _;
use proto::control::{ControlMessage, DisplayEntry, QualityPreset};
use proto::signal::SignalMessage;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Mutex;
use std::time::Instant;
use tokio::sync::{mpsc, watch};
use webrtc::peer_connection::RTCPeerConnectionState;

pub(super) fn display(id: u32, title: &str, primary: bool) -> DisplayInfo {
    DisplayInfo {
        id,
        title: title.to_string(),
        x: 0,
        y: 0,
        width: 100,
        height: 100,
        primary,
    }
}

/// Slice 4.6b: no capturable display -> `start_session` fails with the
/// typed error and the resulting `Bye` carries `DisplayUnavailable`;
/// any other failure stays a plain `Bye`. The wait timeout is shrunk via
/// `start_session_with_display_wait` so the test doesn't sit out 5 s.
/// Ending a session with a live peer queues the final `SessionStats`
/// first -- ahead of the host's own `Bye`.
pub(super) fn expect_final_stats(out_rx: &mut mpsc::UnboundedReceiver<SignalMessage>) {
    match out_rx.try_recv().expect("expected a final SessionStats") {
        SignalMessage::SessionStats {
            session_id,
            final_report,
            ..
        } => {
            assert_eq!(session_id, "sess-1");
            assert!(final_report);
        }
        other => panic!("expected SessionStats, got {other:?}"),
    }
}

/// A `test_ctx()` with an access password registered under a fresh
/// scratch directory (slice 3.2c) -- mirrors
/// `access::tests::scratch_dir`/`app::tests::scratch_device_store`: a
/// unique name is enough, `tempfile` is not an approved dependency.
/// Unlike `test_ctx()` itself, this one *does* write a file (the
/// `access::register`ed record), so every caller needs its own
/// directory -- hence the `name` parameter.
pub(super) fn test_ctx_with_password(name: &str, password: &str) -> HostContext {
    let mut ctx = test_ctx();
    let dir = std::env::temp_dir().join(format!(
        "rcdesk-host-signaling-test-ctx-pw-{name}-{}-{:?}",
        std::process::id(),
        std::thread::current().id()
    ));
    std::fs::create_dir_all(&dir).expect("create scratch dir");
    let store = crate::access::AccessStore::new(&dir);
    store
        .save(&crate::access::register(password).expect("opaque registration"))
        .expect("save access record");
    ctx.access_store = store;
    ctx
}

/// Фейковая шторка (слайс 4.2c): пишет вызовы в общий журнал.
pub(super) struct FakeCurtain {
    log: Arc<Mutex<Vec<String>>>,
    ids: Vec<u32>,
    show_error: Option<String>,
    lock_error: Option<String>,
    shown: Mutex<Vec<u32>>,
}

impl Curtain for FakeCurtain {
    fn show(&self) -> anyhow::Result<Vec<u32>> {
        self.log.lock().unwrap().push("show".to_string());
        match &self.show_error {
            Some(msg) => Err(anyhow::anyhow!(msg.clone())),
            None => {
                *self.shown.lock().unwrap() = self.ids.clone();
                Ok(self.ids.clone())
            }
        }
    }
    fn hide(&self) {
        self.log.lock().unwrap().push("hide".to_string());
        self.shown.lock().unwrap().clear();
    }
    fn lock_screen(&self) -> anyhow::Result<()> {
        self.log.lock().unwrap().push("lock_screen".to_string());
        match &self.lock_error {
            Some(msg) => Err(anyhow::anyhow!(msg.clone())),
            None => Ok(()),
        }
    }
    fn shown(&self) -> Vec<u32> {
        self.shown.lock().unwrap().clone()
    }
}

pub(super) type SourceCalls = Arc<Mutex<Vec<(u32, Vec<u32>)>>>;

/// `test_ctx()` с фейковой шторкой и шпионом на `build_source`.
pub(super) fn curtain_ctx(
    ids: Vec<u32>,
    show_error: Option<&str>,
) -> (HostContext, Arc<Mutex<Vec<String>>>, SourceCalls) {
    curtain_ctx_with_lock_error(ids, show_error, None)
}

pub(super) fn curtain_ctx_with_lock_error(
    ids: Vec<u32>,
    show_error: Option<&str>,
    lock_error: Option<&str>,
) -> (HostContext, Arc<Mutex<Vec<String>>>, SourceCalls) {
    let log = Arc::new(Mutex::new(Vec::new()));
    let calls: SourceCalls = Arc::new(Mutex::new(Vec::new()));
    let mut ctx = test_ctx();
    ctx.curtain = Some(Arc::new(FakeCurtain {
        log: Arc::clone(&log),
        ids,
        show_error: show_error.map(str::to_string),
        lock_error: lock_error.map(str::to_string),
        shown: Mutex::new(Vec::new()),
    }));
    let spy = Arc::clone(&calls);
    ctx.build_source = Arc::new(move |id, exclude| {
        spy.lock().unwrap().push((id, exclude.to_vec()));
        Ok(Box::new(capture::synthetic::for_display(id, 30)?) as Box<dyn FrameSource>)
    });
    (ctx, log, calls)
}

/// Запускает сеанс через `PeerJoined`, как остальные тесты. Возвращает
/// каналы, чтобы они жили до конца теста.
pub(super) async fn start_active_session(
    ctx: &HostContext,
) -> (
    ActiveSession,
    mpsc::UnboundedReceiver<SignalMessage>,
    mpsc::Sender<SessionEvent>,
    mpsc::Receiver<SessionEvent>,
) {
    let (out_tx, out_rx) = mpsc::unbounded_channel::<SignalMessage>();
    let (event_tx, event_rx) = mpsc::channel(64);
    let mut active: Option<ActiveSession> = None;
    let mut current_session_id: Option<String> = None;
    let (status_tx, _status_rx) = watch::channel(AgentStatus::Connecting);
    handle_signal_message(
        SignalMessage::PeerJoined {
            session_id: "sess-1".to_string(),
            ice_servers: vec![],
        },
        ctx,
        &[],
        "111111",
        &status_tx,
        &out_tx,
        event_tx.clone(),
        &mut active,
        &mut current_session_id,
        &mut None,
        &mut 0,
        &mut None,
    )
    .await;
    (
        active.expect("session must have started"),
        out_rx,
        event_tx,
        event_rx,
    )
}
