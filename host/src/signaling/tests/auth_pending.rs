use super::*;

/// Fix to slice 3.2c: this is throttling, not a hard lockout -- six
/// consecutive `PakeStart`s (even ones that fail to finish) must all
/// still run and get a `PakeResponse`; only a *seventh*, sent right
/// after the sixth crossed `MAX_AUTH_ATTEMPTS`, is throttled. The
/// pre-fix behavior refused the sixth attempt itself, which meant the
/// owner could never log in again after five failures -- every later
/// attempt re-armed its own lockout before ever reaching `login_start`.
#[tokio::test(flavor = "multi_thread")]
async fn six_pake_starts_in_a_row_throttle_the_host() {
    use opaque_ke::ClientLogin;
    use rand_core::OsRng;

    let password = "correct horse battery staple";
    let ctx = test_ctx_with_password("six-in-a-row", password);
    let (out_tx, mut out_rx) = mpsc::unbounded_channel::<SignalMessage>();
    let (event_tx, _event_rx) = mpsc::channel(64);
    let mut active: Option<ActiveSession> = None;
    let mut current_session_id: Option<String> = None;
    let mut pending_auth: Option<PendingAuth> = None;
    let mut auth_failures = 0u32;
    let mut auth_locked_until: Option<Instant> = None;
    let (status_tx, _status_rx) = watch::channel(AgentStatus::Connecting);

    handle_signal_message(
        SignalMessage::PeerJoined {
            session_id: "sess-1".to_string(),
            ice_servers: vec![],
        },
        &ctx,
        &[],
        "111111",
        &status_tx,
        &out_tx,
        event_tx.clone(),
        &mut active,
        &mut current_session_id,
        &mut pending_auth,
        &mut auth_failures,
        &mut auth_locked_until,
    )
    .await;
    let _ = out_rx.try_recv(); // auth_required

    // A structurally valid `CredentialRequest` every time (a fresh
    // client start per attempt, same password) -- `login_start` must
    // succeed and answer `PakeResponse` for all six; only `PakeFinish`
    // is ever what actually completes a login, so none of these six
    // resolves the wait.
    let mut rng = OsRng;
    for attempt in 1..=6u32 {
        let client_start =
            ClientLogin::<crate::access::RcdeskCipherSuite>::start(&mut rng, password.as_bytes())
                .expect("client login start");
        handle_signal_message(
            SignalMessage::PakeStart {
                session_id: "sess-1".to_string(),
                payload: BASE64_URL_SAFE_NO_PAD.encode(client_start.message.serialize()),
            },
            &ctx,
            &[],
            "111111",
            &status_tx,
            &out_tx,
            event_tx.clone(),
            &mut active,
            &mut current_session_id,
            &mut pending_auth,
            &mut auth_failures,
            &mut auth_locked_until,
        )
        .await;
        match out_rx.try_recv() {
            Ok(SignalMessage::PakeResponse { session_id, .. }) => {
                assert_eq!(session_id, "sess-1")
            }
            other => panic!("attempt {attempt} expected pake_response, got {other:?}"),
        }
    }
    assert_eq!(auth_failures, 6);
    assert!(
        auth_locked_until.is_some(),
        "the sixth attempt must arm a throttle for the next one"
    );

    // Seventh, right away: throttled -- no `PakeResponse`, no further
    // increment.
    let client_start =
        ClientLogin::<crate::access::RcdeskCipherSuite>::start(&mut rng, password.as_bytes())
            .expect("client login start");
    handle_signal_message(
        SignalMessage::PakeStart {
            session_id: "sess-1".to_string(),
            payload: BASE64_URL_SAFE_NO_PAD.encode(client_start.message.serialize()),
        },
        &ctx,
        &[],
        "111111",
        &status_tx,
        &out_tx,
        event_tx,
        &mut active,
        &mut current_session_id,
        &mut pending_auth,
        &mut auth_failures,
        &mut auth_locked_until,
    )
    .await;
    assert_eq!(
        out_rx.try_recv().unwrap(),
        SignalMessage::AuthFailed {
            session_id: "sess-1".to_string(),
            retry_after_secs: Some(5),
        }
    );
    assert_eq!(
        auth_failures, 6,
        "a throttled attempt must not increment the counter"
    );
}

/// Fix to slice 3.2c: once the throttle armed by a run of failures has
/// expired, the next attempt runs normally -- and, if it's the owner's
/// actual password this time, a full login still succeeds and resets
/// both `auth_failures` and `auth_locked_until`, proving the owner can
/// always eventually get back in.
#[tokio::test(flavor = "multi_thread")]
async fn login_after_expired_throttle_succeeds_and_resets_the_counter() {
    use opaque_ke::{ClientLogin, ClientLoginFinishParameters, CredentialResponse, Identifiers};
    use rand_core::OsRng;

    let password = "correct horse battery staple";
    let ctx = test_ctx_with_password("after-expired-throttle", password);
    let (out_tx, mut out_rx) = mpsc::unbounded_channel::<SignalMessage>();
    let (event_tx, _event_rx) = mpsc::channel(64);
    let mut active: Option<ActiveSession> = None;
    let mut current_session_id: Option<String> = None;
    let mut pending_auth: Option<PendingAuth> = None;
    let mut auth_failures = 0u32;
    let mut auth_locked_until: Option<Instant> = None;
    let (status_tx, _status_rx) = watch::channel(AgentStatus::Connecting);

    handle_signal_message(
        SignalMessage::PeerJoined {
            session_id: "sess-1".to_string(),
            ice_servers: vec![],
        },
        &ctx,
        &[],
        "111111",
        &status_tx,
        &out_tx,
        event_tx.clone(),
        &mut active,
        &mut current_session_id,
        &mut pending_auth,
        &mut auth_failures,
        &mut auth_locked_until,
    )
    .await;
    let _ = out_rx.try_recv(); // auth_required

    // Simulate six prior failed attempts whose throttle has since
    // expired (set directly in the slot, same as the real timer would
    // leave it once `Instant::now()` passes it).
    auth_failures = 6;
    auth_locked_until = Some(Instant::now() - Duration::from_secs(1));

    let mut rng = OsRng;
    let client_start =
        ClientLogin::<crate::access::RcdeskCipherSuite>::start(&mut rng, password.as_bytes())
            .expect("client login start");
    handle_signal_message(
        SignalMessage::PakeStart {
            session_id: "sess-1".to_string(),
            payload: BASE64_URL_SAFE_NO_PAD.encode(client_start.message.serialize()),
        },
        &ctx,
        &[],
        "111111",
        &status_tx,
        &out_tx,
        event_tx.clone(),
        &mut active,
        &mut current_session_id,
        &mut pending_auth,
        &mut auth_failures,
        &mut auth_locked_until,
    )
    .await;
    let response_payload = match out_rx.try_recv().expect("expected pake_response") {
        SignalMessage::PakeResponse { payload, .. } => payload,
        other => panic!("expected pake_response, got {other:?}"),
    };
    assert_eq!(
        auth_failures, 7,
        "the expired-throttle attempt still counts and re-arms a longer throttle"
    );

    let response_bytes = BASE64_URL_SAFE_NO_PAD
        .decode(response_payload)
        .expect("decode pake_response payload");
    let response =
        CredentialResponse::<crate::access::RcdeskCipherSuite>::deserialize(&response_bytes)
            .expect("deserialize credential response");
    let ksf = crate::access::CustomKsf::default();
    let client_finish = client_start
        .state
        .finish(
            &mut rng,
            password.as_bytes(),
            response,
            ClientLoginFinishParameters::new(None, Identifiers::default(), Some(&ksf)),
        )
        .expect("client login finish");

    handle_signal_message(
        SignalMessage::PakeFinish {
            session_id: "sess-1".to_string(),
            payload: BASE64_URL_SAFE_NO_PAD.encode(client_finish.message.serialize()),
        },
        &ctx,
        &[],
        "111111",
        &status_tx,
        &out_tx,
        event_tx,
        &mut active,
        &mut current_session_id,
        &mut pending_auth,
        &mut auth_failures,
        &mut auth_locked_until,
    )
    .await;

    match out_rx.try_recv().expect("expected offer") {
        SignalMessage::Offer { session_id, .. } => assert_eq!(session_id, "sess-1"),
        other => panic!("expected offer, got {other:?}"),
    }
    assert!(active.is_some(), "session must have started successfully");
    assert_eq!(
        auth_failures, 0,
        "a successful login resets the failure counter"
    );
    assert!(
        auth_locked_until.is_none(),
        "a successful login clears the throttle too"
    );

    if let Some(active) = active.take() {
        active.shutdown().await;
    }
}

/// Slice 3.2c: `Bye` for the session a `PendingAuth` is waiting on
/// clears the wait (there's no `active` session to shut down).
#[tokio::test(flavor = "multi_thread")]
async fn bye_for_a_pending_session_clears_it() {
    let ctx = test_ctx_with_password("bye-clears", "correct horse battery staple");
    let (out_tx, mut out_rx) = mpsc::unbounded_channel::<SignalMessage>();
    let (event_tx, _event_rx) = mpsc::channel(64);
    let mut active: Option<ActiveSession> = None;
    let mut current_session_id: Option<String> = None;
    let mut pending_auth: Option<PendingAuth> = None;
    let mut auth_failures = 0u32;
    let mut auth_locked_until: Option<Instant> = None;
    let (status_tx, _status_rx) = watch::channel(AgentStatus::Connecting);

    handle_signal_message(
        SignalMessage::PeerJoined {
            session_id: "sess-1".to_string(),
            ice_servers: vec![],
        },
        &ctx,
        &[],
        "111111",
        &status_tx,
        &out_tx,
        event_tx.clone(),
        &mut active,
        &mut current_session_id,
        &mut pending_auth,
        &mut auth_failures,
        &mut auth_locked_until,
    )
    .await;
    let _ = out_rx.try_recv(); // auth_required
    assert!(pending_auth.is_some());

    handle_signal_message(
        SignalMessage::Bye {
            session_id: "sess-1".to_string(),
            reason: None,
        },
        &ctx,
        &[],
        "111111",
        &status_tx,
        &out_tx,
        event_tx,
        &mut active,
        &mut current_session_id,
        &mut pending_auth,
        &mut auth_failures,
        &mut auth_locked_until,
    )
    .await;

    assert!(pending_auth.is_none());
    assert!(active.is_none());
}

/// Slice 3.2c: `SessionEvent::AuthTimeout` for the wait currently
/// pending ends it with a `Bye`; one for a generation that's already
/// been superseded is ignored.
#[tokio::test(flavor = "multi_thread")]
async fn auth_timeout_sends_bye() {
    let ctx = test_ctx_with_password("auth-timeout", "correct horse battery staple");
    let (out_tx, mut out_rx) = mpsc::unbounded_channel::<SignalMessage>();
    let (event_tx, _event_rx) = mpsc::channel::<SessionEvent>(64);
    let mut active: Option<ActiveSession> = None;
    let mut current_session_id: Option<String> = None;
    let mut pending_auth: Option<PendingAuth> = None;
    let mut auth_failures = 0u32;
    let mut auth_locked_until: Option<Instant> = None;
    let (status_tx, _status_rx) = watch::channel(AgentStatus::Connecting);

    handle_signal_message(
        SignalMessage::PeerJoined {
            session_id: "sess-1".to_string(),
            ice_servers: vec![],
        },
        &ctx,
        &[],
        "111111",
        &status_tx,
        &out_tx,
        event_tx.clone(),
        &mut active,
        &mut current_session_id,
        &mut pending_auth,
        &mut auth_failures,
        &mut auth_locked_until,
    )
    .await;
    let _ = out_rx.try_recv(); // auth_required
    let generation = pending_auth.as_ref().expect("armed above").generation;

    // A stale generation (not the one currently pending) is ignored.
    handle_session_event(
        SessionEvent::AuthTimeout {
            session_id: "sess-1".to_string(),
            generation: generation + 1,
        },
        &ctx,
        "111111",
        &status_tx,
        &out_tx,
        &event_tx,
        &mut active,
        &mut current_session_id,
        &mut pending_auth,
    )
    .await;
    assert!(pending_auth.is_some());
    assert!(out_rx.try_recv().is_err());

    // The current generation ends the wait with a `Bye`.
    handle_session_event(
        SessionEvent::AuthTimeout {
            session_id: "sess-1".to_string(),
            generation,
        },
        &ctx,
        "111111",
        &status_tx,
        &out_tx,
        &event_tx,
        &mut active,
        &mut current_session_id,
        &mut pending_auth,
    )
    .await;
    assert!(pending_auth.is_none());
    assert_eq!(
        out_rx.try_recv().unwrap(),
        SignalMessage::Bye {
            session_id: "sess-1".to_string(),
            reason: None,
        }
    );
}

/// Состояние цикла сигналинга для тестов M-11 (вытеснение сеанса).
struct Slot {
    out_tx: mpsc::UnboundedSender<SignalMessage>,
    out_rx: mpsc::UnboundedReceiver<SignalMessage>,
    event_tx: mpsc::Sender<SessionEvent>,
    _event_rx: mpsc::Receiver<SessionEvent>,
    status_tx: watch::Sender<AgentStatus>,
    active: Option<ActiveSession>,
    current_session_id: Option<String>,
    pending_auth: Option<PendingAuth>,
    auth_failures: u32,
    auth_locked_until: Option<Instant>,
}

impl Slot {
    fn new() -> Self {
        let (out_tx, out_rx) = mpsc::unbounded_channel::<SignalMessage>();
        let (event_tx, _event_rx) = mpsc::channel(64);
        let (status_tx, _) = watch::channel(AgentStatus::Connecting);
        Slot {
            out_tx,
            out_rx,
            event_tx,
            _event_rx,
            status_tx,
            active: None,
            current_session_id: None,
            pending_auth: None,
            auth_failures: 0,
            auth_locked_until: None,
        }
    }

    async fn peer_joined(&mut self, ctx: &HostContext, session_id: &str) {
        handle_signal_message(
            SignalMessage::PeerJoined {
                session_id: session_id.to_string(),
                ice_servers: vec![],
            },
            ctx,
            &[],
            "111111",
            &self.status_tx,
            &self.out_tx,
            self.event_tx.clone(),
            &mut self.active,
            &mut self.current_session_id,
            &mut self.pending_auth,
            &mut self.auth_failures,
            &mut self.auth_locked_until,
        )
        .await;
    }

    fn drain(&mut self) -> Vec<SignalMessage> {
        let mut all = Vec::new();
        while let Ok(m) = self.out_rx.try_recv() {
            all.push(m);
        }
        all
    }
}

fn is_bye_for(msg: &SignalMessage, id: &str) -> bool {
    matches!(msg, SignalMessage::Bye { session_id, .. } if session_id == id)
}

/// M-11: при заданном пароле новый `PeerJoined` не трогает работающий
/// сеанс -- он гасится только после успешного входа.
#[tokio::test(flavor = "multi_thread")]
async fn peer_joined_with_password_keeps_active_session_until_login() {
    let mut ctx = test_ctx();
    let mut slot = Slot::new();
    slot.peer_joined(&ctx, "sess-1").await;
    assert!(slot.active.is_some(), "first session must start");
    slot.drain();
    let old_tag = slot.active.as_ref().unwrap().session_tag;

    ctx.access_store =
        test_ctx_with_password("keeps-active", "correct horse battery staple").access_store;
    slot.peer_joined(&ctx, "sess-2").await;

    let msgs = slot.drain();
    assert!(msgs.contains(&SignalMessage::AuthRequired {
        session_id: "sess-2".to_string()
    }));
    assert!(
        !msgs.iter().any(|m| is_bye_for(m, "sess-1")),
        "old session must not get a Bye: {msgs:?}"
    );
    assert_eq!(slot.active.as_ref().unwrap().session_tag, old_tag);
    assert!(!slot.active.as_ref().unwrap().peer.is_closed());
    assert_eq!(slot.current_session_id.as_deref(), Some("sess-1"));
    assert_eq!(
        slot.pending_auth.as_ref().map(|p| p.session_id.as_str()),
        Some("sess-2")
    );

    // `Bye` кандидата снимает только его ожидание.
    handle_signal_message(
        SignalMessage::Bye {
            session_id: "sess-2".to_string(),
            reason: None,
        },
        &ctx,
        &[],
        "111111",
        &slot.status_tx,
        &slot.out_tx,
        slot.event_tx.clone(),
        &mut slot.active,
        &mut slot.current_session_id,
        &mut slot.pending_auth,
        &mut slot.auth_failures,
        &mut slot.auth_locked_until,
    )
    .await;
    assert!(slot.pending_auth.is_none());
    assert!(slot.active.is_some());
    assert_eq!(slot.current_session_id.as_deref(), Some("sess-1"));
}

#[tokio::test(flavor = "multi_thread")]
async fn auth_timeout_for_pending_login_leaves_active_session_alone() {
    let mut ctx = test_ctx();
    let mut slot = Slot::new();
    slot.peer_joined(&ctx, "sess-1").await;
    ctx.access_store =
        test_ctx_with_password("timeout-active", "correct horse battery staple").access_store;
    slot.peer_joined(&ctx, "sess-2").await;
    slot.drain();
    let generation = slot.pending_auth.as_ref().expect("armed").generation;

    handle_session_event(
        SessionEvent::AuthTimeout {
            session_id: "sess-2".to_string(),
            generation,
        },
        &ctx,
        "111111",
        &slot.status_tx,
        &slot.out_tx,
        &slot.event_tx,
        &mut slot.active,
        &mut slot.current_session_id,
        &mut slot.pending_auth,
    )
    .await;

    assert!(slot.pending_auth.is_none());
    let msgs = slot.drain();
    assert_eq!(msgs.len(), 1, "{msgs:?}");
    assert!(is_bye_for(&msgs[0], "sess-2"));
    assert!(slot.active.is_some());
    assert!(!slot.active.as_ref().unwrap().peer.is_closed());
    assert_eq!(slot.current_session_id.as_deref(), Some("sess-1"));
}

#[tokio::test(flavor = "multi_thread")]
async fn peer_joined_without_password_supersedes_active_session() {
    let ctx = test_ctx();
    let mut slot = Slot::new();
    slot.peer_joined(&ctx, "sess-1").await;
    let old_tag = slot.active.as_ref().expect("first session").session_tag;
    let old_peer = Arc::clone(&slot.active.as_ref().unwrap().peer);
    slot.drain();

    slot.peer_joined(&ctx, "sess-2").await;

    assert!(old_peer.is_closed(), "old peer must be closed");
    let new = slot.active.as_ref().expect("new session");
    assert_ne!(new.session_tag, old_tag);
    assert_eq!(slot.current_session_id.as_deref(), Some("sess-2"));
    assert!(slot
        .drain()
        .iter()
        .any(|m| matches!(m, SignalMessage::Offer { session_id, .. } if session_id == "sess-2")));
}
