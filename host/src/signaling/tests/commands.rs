use super::*;

/// Слайс D50d: `UpdateHost` при недоступности -- `Failed` с причиной.
#[tokio::test]
async fn update_host_without_dirs_is_refused() {
    let ctx = test_ctx();
    match begin_update(&ctx) {
        Err(refusal) => assert_eq!(
            *refusal,
            ControlMessage::UpdateStatus {
                state: proto::control::UpdateState::Failed,
                version: None,
                message: Some("обновление недоступно на этом хосте".to_string()),
                received: None,
                total: None,
            }
        ),
        Ok(_) => panic!("обновление не должно стартовать без update_dirs"),
    }
}

/// `InputMessage::ClipboardText` arriving on the `input` channel must be
/// applied straight to the session's clipboard backend, not handed to
/// `InputRouter` like every other input message (slice 2.5b) -- see
/// `apply_remote_clipboard_text`'s doc comment for why (ordering with
/// the next `Key`).
#[tokio::test(flavor = "multi_thread")]
async fn clipboard_text_on_input_channel_is_applied_directly_not_routed() {
    let mut ctx = test_ctx();
    let fake_clipboard = crate::clipboard::FakeClipboardBackend::new();
    let fake_for_ctx = fake_clipboard.clone();
    ctx.build_clipboard = Some(Box::new(move || {
        Ok(Box::new(fake_for_ctx.clone()) as Box<dyn ClipboardBackend>)
    }));

    let (out_tx, mut out_rx) = mpsc::unbounded_channel::<SignalMessage>();
    let (event_tx, _event_rx) = mpsc::channel(64);
    let mut active: Option<ActiveSession> = None;
    let mut current_session_id: Option<String> = None;
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
        &mut None,
        &mut 0,
        &mut None,
    )
    .await;
    assert!(active.is_some(), "session must have started successfully");
    let _ = out_rx.try_recv(); // the Offer

    let msg = proto::input::InputMessage::ClipboardText {
        text: "from client".to_string(),
    };
    let data = serde_json::to_vec(&msg).unwrap();
    handle_session_event(
        SessionEvent::DataChannelMessage {
            label: "input".to_string(),
            data: data.into(),
            is_string: true,
        },
        &ctx,
        "111111",
        &status_tx,
        &out_tx,
        &event_tx,
        &mut active,
        &mut current_session_id,
        &mut None,
    )
    .await;

    // Applied on the blocking pool (M-7): wait for it to land.
    for _ in 0..200 {
        if !fake_clipboard.set_calls().is_empty() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert_eq!(fake_clipboard.set_calls(), vec!["from client".to_string()]);

    if let Some(active) = active.take() {
        active.shutdown().await;
    }
}

/// `ControlMessage::SetQuality` arriving on the `control` channel must
/// reach the session's adaptation controller (slice 3.5e) -- proven
/// through the real `run_adapt_task` and its 1s ticker (`adapt::TICK`),
/// the same harness as `clipboard_text_on_input_channel_is_applied_directly_not_routed`
/// (no real WebRTC peer needed). There's no open data channel to read
/// `ControlMessage::Quality` back off of, so the effect is observed
/// through `VideoPipeline`'s test-only `rate_control` handle instead
/// (`run_adapt_task` calls `rate_control.set()` on every `Decision`).
#[tokio::test(flavor = "multi_thread")]
async fn set_quality_reaches_the_adapt_controller() {
    let mut ctx = test_ctx();
    ctx.adapt = true;

    let (out_tx, mut out_rx) = mpsc::unbounded_channel::<SignalMessage>();
    let (event_tx, _event_rx) = mpsc::channel(64);
    let mut active: Option<ActiveSession> = None;
    let mut current_session_id: Option<String> = None;
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
        &mut None,
        &mut 0,
        &mut None,
    )
    .await;
    assert!(active.is_some(), "session must have started successfully");
    let _ = out_rx.try_recv(); // the Offer

    assert_eq!(
        active.as_ref().unwrap().video.rate_control.fps(),
        30,
        "starts pinned to the session's configured fps"
    );

    let msg = ControlMessage::SetQuality {
        preset: QualityPreset::Sharp,
    };
    let data = serde_json::to_vec(&msg).unwrap();
    handle_session_event(
        SessionEvent::DataChannelMessage {
            label: "control".to_string(),
            data: data.into(),
            is_string: true,
        },
        &ctx,
        "111111",
        &status_tx,
        &out_tx,
        &event_tx,
        &mut active,
        &mut current_session_id,
        &mut None,
    )
    .await;

    assert_eq!(
        active.as_ref().unwrap().quality_preset,
        QualityPreset::Sharp,
        "the session remembers the chosen preset"
    );

    // `run_adapt_task` only applies a `Decision` to `RateControl` on its
    // own 1s ticker (see that function's doc comment) -- wait a couple
    // of ticks rather than racing it.
    // `<=`, not `==`: on a slow CI runner the software encoder can't keep
    // up and the controller legitimately steps below the 15 fps ceiling
    // within the same wait (seen on ubuntu-latest: 12) -- what matters
    // here is that the ceiling arrived, not where overload left fps.
    tokio::time::sleep(Duration::from_millis(2_500)).await;
    let fps = active.as_ref().unwrap().video.rate_control.fps();
    assert!(
        fps <= 15,
        "sharp's fps ceiling must have reached the controller and been applied, got {fps}"
    );

    if let Some(active) = active.take() {
        active.shutdown().await;
    }
}

/// `AgentCommand::EndSession` with an active session must tell the
/// server (`Bye`), tear the session down and report `Registered` again
/// -- the tray agent's "End session" menu item (slice 2.6c).
#[tokio::test(flavor = "multi_thread")]
async fn end_session_command_sends_bye_and_reports_registered() {
    let ctx = test_ctx();
    let (out_tx, mut out_rx) = mpsc::unbounded_channel::<SignalMessage>();
    let (event_tx, _event_rx) = mpsc::channel(64);
    let mut active: Option<ActiveSession> = None;
    let mut current_session_id: Option<String> = None;
    let (status_tx, status_rx) = watch::channel(AgentStatus::Connecting);

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
        &mut None,
        &mut 0,
        &mut None,
    )
    .await;
    assert!(active.is_some(), "session must have started successfully");
    let _ = out_rx.try_recv(); // the Offer

    handle_agent_command(
        AgentCommand::EndSession,
        "111111",
        &status_tx,
        &out_tx,
        &mut active,
        &mut current_session_id,
        &mut None,
    )
    .await;

    assert!(active.is_none());
    assert_eq!(current_session_id, None);
    expect_final_stats(&mut out_rx);
    assert_eq!(
        out_rx.try_recv().expect("expected a Bye message"),
        SignalMessage::Bye {
            session_id: "sess-1".to_string(),
            reason: None,
        }
    );
    assert_eq!(
        *status_rx.borrow(),
        AgentStatus::Registered {
            pin: "111111".to_string()
        }
    );
}

/// `AgentCommand::EndSession` with no active session and no pending auth
/// is a no-op: no `Bye`, no status change.
#[tokio::test(flavor = "multi_thread")]
async fn end_session_command_with_no_active_session_is_a_no_op() {
    let (out_tx, mut out_rx) = mpsc::unbounded_channel::<SignalMessage>();
    let mut active: Option<ActiveSession> = None;
    let mut current_session_id: Option<String> = None;
    let (status_tx, status_rx) = watch::channel(AgentStatus::Registered {
        pin: "111111".to_string(),
    });

    handle_agent_command(
        AgentCommand::EndSession,
        "111111",
        &status_tx,
        &out_tx,
        &mut active,
        &mut current_session_id,
        &mut None,
    )
    .await;

    assert!(out_rx.try_recv().is_err(), "no Bye should have been sent");
    assert!(
        !status_rx.has_changed().unwrap(),
        "status must not change when there was no active session"
    );
}

/// Fix to slice 3.2c: `AgentCommand::EndSession` while a client is
/// still waiting on `AuthRequired` (no `ActiveSession` yet, but a
/// `PendingAuth` is armed) must give up on that wait too -- not just
/// silently no-op, as it used to before this fix -- clearing
/// `pending_auth` and telling the server `Bye` for its session id.
/// Status is not touched: it was never anything but `Registered` while
/// only a login was pending.
#[tokio::test(flavor = "multi_thread")]
async fn end_session_command_clears_pending_auth_and_sends_bye() {
    let ctx = test_ctx_with_password("end-session-clears-pending", "correct horse battery staple");
    let (out_tx, mut out_rx) = mpsc::unbounded_channel::<SignalMessage>();
    let (event_tx, _event_rx) = mpsc::channel(64);
    let mut active: Option<ActiveSession> = None;
    let mut current_session_id: Option<String> = None;
    let mut pending_auth: Option<PendingAuth> = None;
    let mut auth_failures = 0u32;
    let mut auth_locked_until: Option<Instant> = None;
    let (status_tx, status_rx) = watch::channel(AgentStatus::Registered {
        pin: "111111".to_string(),
    });

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
    let _ = out_rx.try_recv(); // auth_required
    assert!(pending_auth.is_some());

    handle_agent_command(
        AgentCommand::EndSession,
        "111111",
        &status_tx,
        &out_tx,
        &mut active,
        &mut current_session_id,
        &mut pending_auth,
    )
    .await;

    assert!(pending_auth.is_none());
    assert_eq!(
        out_rx.try_recv().expect("expected a Bye message"),
        SignalMessage::Bye {
            session_id: "sess-1".to_string(),
            reason: None,
        }
    );
    assert!(
        !status_rx.has_changed().unwrap(),
        "status must not change -- it was already Registered"
    );
}
