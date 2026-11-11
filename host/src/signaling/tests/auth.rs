use super::*;

/// Slice 3.2c: `PeerJoined` on a host with a password set must ask the
/// client to authenticate (`AuthRequired`) instead of offering a
/// session right away.
#[tokio::test(flavor = "multi_thread")]
async fn peer_joined_with_a_password_asks_for_auth_and_does_not_offer() {
    let ctx = test_ctx_with_password("asks-for-auth", "correct horse battery staple");
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
        event_tx,
        &mut active,
        &mut current_session_id,
        &mut pending_auth,
        &mut auth_failures,
        &mut auth_locked_until,
    )
    .await;

    assert_eq!(
        out_rx.try_recv().expect("expected auth_required"),
        SignalMessage::AuthRequired {
            session_id: "sess-1".to_string()
        }
    );
    assert!(
        out_rx.try_recv().is_err(),
        "no Offer must be sent while waiting for auth"
    );
    assert!(active.is_none());
    assert!(pending_auth.is_some());
}

/// Fail-closed: an unreadable/corrupt `access.json` must not be treated
/// as "no password" -- the session is refused with `Bye`.
#[tokio::test(flavor = "multi_thread")]
async fn peer_joined_with_unreadable_access_record_sends_bye_and_starts_nothing() {
    let mut ctx = test_ctx();
    let dir = std::env::temp_dir().join(format!(
        "rcdesk-host-signaling-test-ctx-corrupt-{}-{:?}",
        std::process::id(),
        std::thread::current().id()
    ));
    std::fs::create_dir_all(&dir).expect("create scratch dir");
    let store = crate::access::AccessStore::new(&dir);
    std::fs::write(store.path(), b"not json at all").expect("write corrupt record");
    ctx.access_store = store;

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
        event_tx,
        &mut active,
        &mut current_session_id,
        &mut pending_auth,
        &mut auth_failures,
        &mut auth_locked_until,
    )
    .await;

    assert_eq!(
        out_rx.try_recv().expect("expected Bye"),
        SignalMessage::Bye {
            session_id: "sess-1".to_string(),
            reason: None,
        }
    );
    assert!(
        out_rx.try_recv().is_err(),
        "neither AuthRequired nor Offer may follow"
    );
    assert!(active.is_none());
    assert!(pending_auth.is_none());
    assert_eq!(current_session_id, None);

    let _ = std::fs::remove_dir_all(&dir);
}

/// Slice 3.2c: a full, real OPAQUE login (client side via `opaque_ke`
/// directly, same as `access::tests`) through `AuthRequired`/
/// `PakeStart`/`PakeResponse`/`PakeFinish` ends with the host offering
/// a session, exactly as the no-password path would have.
#[tokio::test(flavor = "multi_thread")]
async fn full_opaque_login_then_offer() {
    use opaque_ke::{ClientLogin, ClientLoginFinishParameters, CredentialResponse, Identifiers};
    use rand_core::OsRng;

    let password = "correct horse battery staple";
    let ctx = test_ctx_with_password("full-login", password);
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
    assert_eq!(
        out_rx.try_recv().expect("expected auth_required"),
        SignalMessage::AuthRequired {
            session_id: "sess-1".to_string()
        }
    );

    let mut rng = OsRng;
    let client_start =
        ClientLogin::<crate::access::RcdeskCipherSuite>::start(&mut rng, password.as_bytes())
            .expect("client login start");
    let request_payload = BASE64_URL_SAFE_NO_PAD.encode(client_start.message.serialize());

    handle_signal_message(
        SignalMessage::PakeStart {
            session_id: "sess-1".to_string(),
            payload: request_payload,
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
    let finish_payload = BASE64_URL_SAFE_NO_PAD.encode(client_finish.message.serialize());

    handle_signal_message(
        SignalMessage::PakeFinish {
            session_id: "sess-1".to_string(),
            payload: finish_payload,
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
        SignalMessage::Offer {
            session_id,
            sdp,
            auth,
        } => {
            assert_eq!(session_id, "sess-1");
            // Slice 3.2e: this session was gated by a password, so the
            // offer must carry a dtls-binding tag over its own
            // fingerprint, verifiable with the *client's* copy of the
            // session key (`client_finish.session_key`) -- the two
            // sides derive the same key from a real OPAQUE login, same
            // as `access::tests::register_then_login_with_the_same_password_yields_equal_session_keys`.
            let tag = auth.expect("offer must carry a dtls-binding tag when a password is set");
            let fingerprint = crate::dtls_bind::fingerprint_from_sdp(&sdp)
                .expect("real webrtc-rs offer sdp must have a dtls fingerprint");
            assert!(crate::dtls_bind::verify_auth_tag(
                &client_finish.session_key[..],
                "offer",
                &fingerprint,
                &tag,
            ));
        }
        other => panic!("expected offer, got {other:?}"),
    }
    assert!(active.is_some(), "session must have started successfully");
    assert_eq!(auth_failures, 0);
    assert!(pending_auth.is_none());

    if let Some(active) = active.take() {
        active.shutdown().await;
    }
}

/// Runs a full real OPAQUE login (same steps as `full_opaque_login_then_offer`)
/// up through the host's `Offer`, for the two `Answer`-side dtls-binding
/// tests below (slice 3.2e), which only care about what happens next.
/// Returns the context (so the session stays valid for `handle_signal_message`
/// calls with it), the loop state after the `Offer`, and the client's
/// session key -- the client's own proof it holds the same key the host
/// does, exactly as a real browser client would after `finishLogin`.
#[allow(clippy::type_complexity)]
async fn logged_in_awaiting_answer(
    name: &str,
) -> (
    HostContext,
    Option<ActiveSession>,
    Option<String>,
    mpsc::UnboundedSender<SignalMessage>,
    mpsc::UnboundedReceiver<SignalMessage>,
    mpsc::Sender<SessionEvent>,
    watch::Sender<AgentStatus>,
    Vec<u8>,
) {
    use opaque_ke::{ClientLogin, ClientLoginFinishParameters, CredentialResponse, Identifiers};
    use rand_core::OsRng;

    let password = "correct horse battery staple";
    let ctx = test_ctx_with_password(name, password);
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
    let response = CredentialResponse::<crate::access::RcdeskCipherSuite>::deserialize(
        &BASE64_URL_SAFE_NO_PAD
            .decode(response_payload)
            .expect("decode pake_response payload"),
    )
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
    let session_key = client_finish.session_key[..].to_vec();

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
        event_tx.clone(),
        &mut active,
        &mut current_session_id,
        &mut pending_auth,
        &mut auth_failures,
        &mut auth_locked_until,
    )
    .await;
    let _ = out_rx.try_recv(); // the Offer
    assert!(active.is_some(), "session must have started successfully");

    (
        ctx,
        active,
        current_session_id,
        out_tx,
        out_rx,
        event_tx,
        status_tx,
        session_key,
    )
}

/// Slice 3.2e: an `Answer` with no `auth` at all, while the session was
/// gated by a password, must be rejected -- the host ends the session
/// (`Bye`) instead of applying an unbound answer.
#[tokio::test(flavor = "multi_thread")]
async fn answer_without_auth_ends_session_when_a_password_is_set() {
    let (ctx, mut active, mut current_session_id, out_tx, mut out_rx, event_tx, status_tx, _) =
        logged_in_awaiting_answer("answer-no-auth").await;

    handle_signal_message(
        SignalMessage::Answer {
            session_id: "sess-1".to_string(),
            sdp: "v=0\r\na=fingerprint:sha-256 AA:BB:CC\r\n".to_string(),
            auth: None,
        },
        &ctx,
        &[],
        "111111",
        &status_tx,
        &out_tx,
        event_tx,
        &mut active,
        &mut current_session_id,
        &mut None,
        &mut 0,
        &mut None,
    )
    .await;

    expect_final_stats(&mut out_rx);
    assert_eq!(
        out_rx.try_recv().expect("expected bye"),
        SignalMessage::Bye {
            session_id: "sess-1".to_string(),
            reason: None,
        }
    );
    assert!(active.is_none(), "session must have been ended");
    assert_eq!(current_session_id, None);
}

/// Slice 3.2e: an `Answer` whose `auth` tag verifies against the
/// client's session key and the answer's own fingerprint must *not* be
/// rejected by the binding check -- the host goes on to try `set_answer`
/// (which fails here, since this is a synthetic SDP rather than a real
/// DTLS answer). Slice E3: that failure now ends the session with a
/// `Bye`, so the `Bye` below comes from `set_answer`, not the binding.
#[tokio::test(flavor = "multi_thread")]
async fn answer_with_a_valid_auth_tag_is_not_rejected() {
    let (
        ctx,
        mut active,
        mut current_session_id,
        out_tx,
        mut out_rx,
        event_tx,
        status_tx,
        session_key,
    ) = logged_in_awaiting_answer("answer-valid-auth").await;

    let sdp = "v=0\r\na=fingerprint:sha-256 AA:BB:CC\r\n".to_string();
    let fingerprint = crate::dtls_bind::fingerprint_from_sdp(&sdp).expect("sdp has a fingerprint");
    let tag = crate::dtls_bind::auth_tag(&session_key, "answer", &fingerprint);

    handle_signal_message(
        SignalMessage::Answer {
            session_id: "sess-1".to_string(),
            sdp,
            auth: Some(tag),
        },
        &ctx,
        &[],
        "111111",
        &status_tx,
        &out_tx,
        event_tx,
        &mut active,
        &mut current_session_id,
        &mut None,
        &mut 0,
        &mut None,
    )
    .await;

    // Slice E3: binding passed, `set_answer` failed on the synthetic SDP
    // -> the session is ended with a Bye.
    expect_final_stats(&mut out_rx);
    assert_eq!(
        out_rx
            .try_recv()
            .expect("expected bye after set_answer failure"),
        SignalMessage::Bye {
            session_id: "sess-1".to_string(),
            reason: None,
        }
    );
    assert!(active.is_none(), "session must have been ended");
    assert_eq!(current_session_id, None);
}

/// Slice 3.2c: a client that gets its password wrong fails locally
/// (`ClientLogin::finish`, same as `access::tests::login_with_a_wrong_password_fails`)
/// and never even produces a `PakeFinish` to send -- the host's wait
/// must survive that (no `Offer`, `pending_auth` still armed), and a
/// second, correct attempt afterwards must still succeed.
#[tokio::test(flavor = "multi_thread")]
async fn wrong_password_client_finish_fails_and_host_stays_pending() {
    use opaque_ke::{ClientLogin, ClientLoginFinishParameters, CredentialResponse, Identifiers};
    use rand_core::OsRng;

    let password = "correct horse battery staple";
    let ctx = test_ctx_with_password("wrong-then-right", password);
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

    // First, wrong-password attempt: the host's `PakeStart` handling
    // can't tell it's wrong yet (OPAQUE's OPRF step is password-blind),
    // so it still answers `PakeResponse` -- the mismatch is only ever
    // caught client-side, in `ClientLogin::finish` below.
    let mut rng = OsRng;
    let wrong_client_start =
        ClientLogin::<crate::access::RcdeskCipherSuite>::start(&mut rng, b"wrong password")
            .expect("client login start");
    handle_signal_message(
        SignalMessage::PakeStart {
            session_id: "sess-1".to_string(),
            payload: BASE64_URL_SAFE_NO_PAD.encode(wrong_client_start.message.serialize()),
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
    let wrong_response_payload = match out_rx.try_recv().expect("expected pake_response") {
        SignalMessage::PakeResponse { payload, .. } => payload,
        other => panic!("expected pake_response, got {other:?}"),
    };
    let wrong_response = CredentialResponse::<crate::access::RcdeskCipherSuite>::deserialize(
        &BASE64_URL_SAFE_NO_PAD
            .decode(wrong_response_payload)
            .expect("decode pake_response payload"),
    )
    .expect("deserialize credential response");
    let ksf = crate::access::CustomKsf::default();
    let wrong_finish_result = wrong_client_start.state.finish(
        &mut rng,
        b"wrong password",
        wrong_response,
        ClientLoginFinishParameters::new(None, Identifiers::default(), Some(&ksf)),
    );
    assert!(
        wrong_finish_result.is_err(),
        "a wrong password must fail on the client side"
    );

    // No `PakeFinish` was ever sent (there is nothing valid to send),
    // so the host must still be waiting, with no `Offer` sent.
    assert!(out_rx.try_recv().is_err());
    assert!(active.is_none());
    assert!(pending_auth.is_some());

    // A second, correct attempt must still succeed.
    let right_client_start =
        ClientLogin::<crate::access::RcdeskCipherSuite>::start(&mut rng, password.as_bytes())
            .expect("client login start");
    handle_signal_message(
        SignalMessage::PakeStart {
            session_id: "sess-1".to_string(),
            payload: BASE64_URL_SAFE_NO_PAD.encode(right_client_start.message.serialize()),
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
    let right_response_payload = match out_rx.try_recv().expect("expected pake_response") {
        SignalMessage::PakeResponse { payload, .. } => payload,
        other => panic!("expected pake_response, got {other:?}"),
    };
    let right_response = CredentialResponse::<crate::access::RcdeskCipherSuite>::deserialize(
        &BASE64_URL_SAFE_NO_PAD
            .decode(right_response_payload)
            .expect("decode pake_response payload"),
    )
    .expect("deserialize credential response");
    let right_client_finish = right_client_start
        .state
        .finish(
            &mut rng,
            password.as_bytes(),
            right_response,
            ClientLoginFinishParameters::new(None, Identifiers::default(), Some(&ksf)),
        )
        .expect("client login finish");

    handle_signal_message(
        SignalMessage::PakeFinish {
            session_id: "sess-1".to_string(),
            payload: BASE64_URL_SAFE_NO_PAD.encode(right_client_finish.message.serialize()),
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

    if let Some(active) = active.take() {
        active.shutdown().await;
    }
}

/// Slice 3.2c: a `PakeFinish` that isn't a valid OPAQUE finalization
/// (garbage payload, not this attacker's actual login attempt) is
/// rejected with `AuthFailed { retry_after_secs: None }`.
#[tokio::test(flavor = "multi_thread")]
async fn garbage_finalization_yields_auth_failed() {
    use opaque_ke::ClientLogin;
    use rand_core::OsRng;

    let password = "correct horse battery staple";
    let ctx = test_ctx_with_password("garbage-finalization", password);
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
    let _ = out_rx.try_recv(); // pake_response
                               // `login` is `Some` now -- a `PakeFinish` matches and is attempted,
                               // but its payload isn't even valid base64.
    handle_signal_message(
        SignalMessage::PakeFinish {
            session_id: "sess-1".to_string(),
            payload: "not valid base64 at all!!".to_string(),
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
        out_rx.try_recv().expect("expected auth_failed"),
        SignalMessage::AuthFailed {
            session_id: "sess-1".to_string(),
            retry_after_secs: None,
        }
    );
    assert!(active.is_none());
    assert!(
        pending_auth.is_some(),
        "the wait stays armed for a fresh PakeStart"
    );
}
