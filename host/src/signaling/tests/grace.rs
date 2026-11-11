use super::*;

/// Slice 3.5a: `SessionSlot::run_while_offline` must still drain and act
/// on session events while there's no live signaling connection --
/// otherwise input arriving over data channels would stall (the event
/// channel is bounded) and e.g. the peer connection failing would never
/// free the slot. Exercised directly on the slot: start a session, feed
/// it a `ConnectionState(Closed)` event through the slot's own
/// `event_tx` (the same channel `PeerSession` reports through in
/// production), and confirm `run_while_offline` frees the slot before
/// the passed-in future resolves.
#[tokio::test(flavor = "multi_thread")]
async fn run_while_offline_still_frees_the_slot_on_connection_state_closed() {
    let ctx = test_ctx();
    let mut slot = SessionSlot::new();
    let (out_tx, mut out_rx) = mpsc::unbounded_channel::<SignalMessage>();
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
        slot.event_tx.clone(),
        &mut slot.active,
        &mut slot.current_session_id,
        &mut slot.pending_auth,
        &mut slot.auth_failures,
        &mut slot.auth_locked_until,
    )
    .await;
    assert!(slot.is_active(), "session must have started successfully");
    let _ = out_rx.try_recv(); // the Offer

    let tag = slot.active.as_ref().unwrap().session_tag;
    slot.event_tx
        .send(SessionEvent::ConnectionState {
            state: RTCPeerConnectionState::Closed,
            tag,
        })
        .await
        .expect("the slot's own event channel must accept a send");

    let (_cmd_tx, mut cmd_rx) = mpsc::unbounded_channel::<AgentCommand>();
    slot.run_while_offline(&ctx, &mut cmd_rx, async {
        // Give the offline select loop a turn to drain the event above
        // before the wrapped future resolves.
        tokio::time::sleep(Duration::from_millis(50)).await;
    })
    .await;

    assert!(
        !slot.is_active(),
        "a ConnectionState(Closed) event must free the slot even while offline"
    );
}

/// Starts a real session (via `PeerJoined`, same as the other tests in
/// this module) and returns the pieces `handle_session_event` needs, for
/// the slice 3.5b `Disconnected`/`DisconnectTimeout`/`DataChannelClosed`
/// tests below.
async fn started_session(
    session_id: &str,
) -> (
    HostContext,
    Option<ActiveSession>,
    Option<String>,
    mpsc::UnboundedSender<SignalMessage>,
    mpsc::UnboundedReceiver<SignalMessage>,
    mpsc::Sender<SessionEvent>,
    watch::Sender<AgentStatus>,
    watch::Receiver<AgentStatus>,
) {
    let ctx = test_ctx();
    let (out_tx, out_rx) = mpsc::unbounded_channel::<SignalMessage>();
    let (event_tx, _event_rx) = mpsc::channel(64);
    let mut active: Option<ActiveSession> = None;
    let mut current_session_id: Option<String> = None;
    let (status_tx, status_rx) = watch::channel(AgentStatus::Connecting);

    handle_signal_message(
        SignalMessage::PeerJoined {
            session_id: session_id.to_string(),
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

    (
        ctx,
        active,
        current_session_id,
        out_tx,
        out_rx,
        event_tx,
        status_tx,
        status_rx,
    )
}

/// Slice 3.5b: `ConnectionState(Disconnected)` must not end the session
/// right away -- only arm the `DISCONNECT_GRACE` window.
#[tokio::test(flavor = "multi_thread")]
async fn disconnected_does_not_end_the_session_immediately() {
    let (ctx, mut active, mut current_session_id, out_tx, _out_rx, event_tx, status_tx, _) =
        started_session("sess-1").await;

    handle_session_event(
        SessionEvent::ConnectionState {
            state: RTCPeerConnectionState::Disconnected,
            tag: active.as_ref().unwrap().session_tag,
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

    assert!(
        active.is_some(),
        "Disconnected alone must not end the session"
    );
    assert_eq!(current_session_id.as_deref(), Some("sess-1"));

    if let Some(active) = active.take() {
        active.shutdown().await;
    }
}

/// Slice 3.5b: `Connected` arriving while a disconnect grace window is
/// running cancels it -- the session survives and status goes back to
/// `InSession`.
#[tokio::test(flavor = "multi_thread")]
async fn connected_within_the_grace_window_cancels_it() {
    let (ctx, mut active, mut current_session_id, out_tx, _out_rx, event_tx, status_tx, status_rx) =
        started_session("sess-1").await;

    handle_session_event(
        SessionEvent::ConnectionState {
            state: RTCPeerConnectionState::Disconnected,
            tag: active.as_ref().unwrap().session_tag,
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
    handle_session_event(
        SessionEvent::ConnectionState {
            state: RTCPeerConnectionState::Connected,
            tag: active.as_ref().unwrap().session_tag,
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

    assert!(active.is_some(), "the session must survive");
    assert_eq!(
        *status_rx.borrow(),
        AgentStatus::InSession {
            pin: "111111".to_string()
        }
    );

    // The now-cancelled window's `DisconnectTimeout` must be ignored --
    // proven directly in `disconnect_grace_tests`; here it's enough to
    // check `pending_disconnect`'s observable effect via `is_current`.
    assert!(!active.as_ref().unwrap().disconnect_grace.is_current(1));

    if let Some(active) = active.take() {
        active.shutdown().await;
    }
}

/// Slice 3.5b: a `DisconnectTimeout` matching the currently pending
/// grace window ends the session, the same way `Failed`/`Closed` do.
/// The real `DISCONNECT_GRACE` timer isn't waited out here (this
/// workspace doesn't enable tokio's `test-util` feature needed to pause
/// the clock) -- the event is injected directly, exercising exactly the
/// same handler code the real timer's `send` would reach. Slice
/// 3.2a/D35: also checks that ending the session this way sends the
/// server a `Bye`, so it doesn't keep the device marked busy.
#[tokio::test(flavor = "multi_thread")]
async fn disconnect_timeout_with_matching_generation_ends_the_session() {
    let (
        ctx,
        mut active,
        mut current_session_id,
        out_tx,
        mut out_rx,
        event_tx,
        status_tx,
        status_rx,
    ) = started_session("sess-1").await;
    // Slice 3.2e: `started_session` uses `test_ctx()`, which has no
    // access password set -- the offer must carry no dtls-binding tag,
    // same behavior as before this slice.
    match out_rx.try_recv().expect("expected the Offer") {
        SignalMessage::Offer { auth, .. } => assert_eq!(auth, None),
        other => panic!("expected offer, got {other:?}"),
    }

    handle_session_event(
        SessionEvent::ConnectionState {
            state: RTCPeerConnectionState::Disconnected,
            tag: active.as_ref().unwrap().session_tag,
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
    handle_session_event(
        SessionEvent::DisconnectTimeout {
            session_id: "sess-1".to_string(),
            generation: 1,
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

    assert!(
        active.is_none(),
        "the grace window expiring must end the session"
    );
    assert_eq!(current_session_id, None);
    assert_eq!(
        *status_rx.borrow(),
        AgentStatus::Registered {
            pin: "111111".to_string()
        }
    );
    expect_final_stats(&mut out_rx);
    assert_eq!(
        out_rx.try_recv().expect("expected a Bye message"),
        SignalMessage::Bye {
            session_id: "sess-1".to_string(),
            reason: None,
        }
    );
}

/// Slice 3.5b: a `DisconnectTimeout` from a window a `Connected` already
/// cancelled must be ignored -- it's stale.
#[tokio::test(flavor = "multi_thread")]
async fn disconnect_timeout_from_a_cancelled_window_is_ignored() {
    let (ctx, mut active, mut current_session_id, out_tx, _out_rx, event_tx, status_tx, _) =
        started_session("sess-1").await;

    handle_session_event(
        SessionEvent::ConnectionState {
            state: RTCPeerConnectionState::Disconnected,
            tag: active.as_ref().unwrap().session_tag,
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
    handle_session_event(
        SessionEvent::ConnectionState {
            state: RTCPeerConnectionState::Connected,
            tag: active.as_ref().unwrap().session_tag,
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
    handle_session_event(
        SessionEvent::DisconnectTimeout {
            session_id: "sess-1".to_string(),
            generation: 1,
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

    assert!(
        active.is_some(),
        "a stale DisconnectTimeout must not end the session"
    );

    if let Some(active) = active.take() {
        active.shutdown().await;
    }
}

/// Slice 3.5b: a `DisconnectTimeout` naming a session that's no longer
/// current (a new one has since started) must not touch the new
/// session.
#[tokio::test(flavor = "multi_thread")]
async fn disconnect_timeout_from_an_old_session_does_not_touch_a_new_one() {
    let (ctx, mut active, mut current_session_id, out_tx, mut out_rx, event_tx, status_tx, _) =
        started_session("sess-1").await;

    handle_session_event(
        SessionEvent::ConnectionState {
            state: RTCPeerConnectionState::Disconnected,
            tag: active.as_ref().unwrap().session_tag,
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

    // A brand new session replaces the (still disconnected) old one --
    // mirrors `new PeerJoined while a session was active` in
    // `handle_signal_message`.
    handle_signal_message(
        SignalMessage::PeerJoined {
            session_id: "sess-2".to_string(),
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
    assert_eq!(current_session_id.as_deref(), Some("sess-2"));
    let _ = out_rx.try_recv(); // sess-1's Offer
    let _ = out_rx.try_recv(); // sess-2's Offer

    // sess-1's stale timeout arrives after sess-2 has already started.
    handle_session_event(
        SessionEvent::DisconnectTimeout {
            session_id: "sess-1".to_string(),
            generation: 1,
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

    assert!(
        active.is_some(),
        "an old session's timeout must not end the new session"
    );
    assert_eq!(current_session_id.as_deref(), Some("sess-2"));

    if let Some(active) = active.take() {
        active.shutdown().await;
    }
}

/// Fix to slice 3.5b: a `ConnectionState` event tagged with an old,
/// already-superseded session's tag must not touch the new one -- the
/// scenario this guards against is automatic client reconnection (bye ->
/// `connect_device` -> `PeerJoined`, all while the old `PeerConnection`
/// is still tearing down): its `Closed` can land on the shared event
/// channel after a new session has already taken `active`'s place.
#[tokio::test(flavor = "multi_thread")]
async fn connection_state_from_an_old_session_does_not_touch_a_new_one() {
    let (
        ctx,
        mut active,
        mut current_session_id,
        out_tx,
        mut out_rx,
        event_tx,
        status_tx,
        status_rx,
    ) = started_session("sess-1").await;
    let old_tag = active.as_ref().unwrap().session_tag;

    // sess-1 is superseded by a brand new sess-2, same as the
    // `DisconnectTimeout` test above.
    handle_signal_message(
        SignalMessage::PeerJoined {
            session_id: "sess-2".to_string(),
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
    assert_eq!(current_session_id.as_deref(), Some("sess-2"));
    let new_tag = active.as_ref().unwrap().session_tag;
    assert_ne!(old_tag, new_tag, "each PeerSession must get a distinct tag");
    let _ = out_rx.try_recv(); // sess-1's Offer
    let _ = out_rx.try_recv(); // sess-2's Offer
    let mut status_rx = status_rx;
    status_rx.borrow_and_update(); // mark the supersede's own status send as seen

    // sess-1's `PeerConnection`, still tearing down, reports `Closed`
    // after sess-2 has already started.
    handle_session_event(
        SessionEvent::ConnectionState {
            state: RTCPeerConnectionState::Closed,
            tag: old_tag,
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

    assert!(
        active.is_some(),
        "an old session's Closed event must not end the new session"
    );
    assert_eq!(current_session_id.as_deref(), Some("sess-2"));
    assert!(
        !status_rx.has_changed().unwrap(),
        "a stale Closed event must not report any new status"
    );

    if let Some(active) = active.take() {
        active.shutdown().await;
    }
}

/// Fix to slice 3.5b: same as
/// `connection_state_from_an_old_session_does_not_touch_a_new_one`, for
/// the `control` data channel closing -- the concrete bug this fixes
/// (client reconnects: bye -> new session starts -> the old session's
/// `control` channel finishes closing and its event arrives after).
#[tokio::test(flavor = "multi_thread")]
async fn data_channel_closed_from_an_old_session_does_not_touch_a_new_one() {
    let (ctx, mut active, mut current_session_id, out_tx, mut out_rx, event_tx, status_tx, _) =
        started_session("sess-1").await;
    let old_tag = active.as_ref().unwrap().session_tag;

    handle_signal_message(
        SignalMessage::PeerJoined {
            session_id: "sess-2".to_string(),
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
    assert_eq!(current_session_id.as_deref(), Some("sess-2"));
    let _ = out_rx.try_recv(); // sess-1's Offer
    let _ = out_rx.try_recv(); // sess-2's Offer

    // sess-1's `control` channel finishes its close handshake after
    // sess-2 has already started.
    handle_session_event(
        SessionEvent::DataChannelClosed {
            label: "control".to_string(),
            tag: old_tag,
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

    assert!(
        active.is_some(),
        "an old session's control channel closing must not end the new session"
    );
    assert_eq!(current_session_id.as_deref(), Some("sess-2"));

    if let Some(active) = active.take() {
        active.shutdown().await;
    }
}

/// Slice 3.5b: the `control` data channel closing ends the session
/// immediately, without waiting for a `Disconnected` grace window.
/// Slice 3.2a/D35: also checks that ending the session this way sends
/// the server a `Bye`, so it doesn't keep the device marked busy -- and
/// that the second, superseded `DataChannelClosed` (see below) does not
/// send a second one.
#[tokio::test(flavor = "multi_thread")]
async fn control_data_channel_closed_ends_the_session_immediately() {
    let (
        ctx,
        mut active,
        mut current_session_id,
        out_tx,
        mut out_rx,
        event_tx,
        status_tx,
        status_rx,
    ) = started_session("sess-1").await;
    let _ = out_rx.try_recv(); // the Offer
    let tag = active.as_ref().unwrap().session_tag;

    handle_session_event(
        SessionEvent::DataChannelClosed {
            label: "control".to_string(),
            tag,
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

    assert!(active.is_none());
    assert_eq!(current_session_id, None);
    assert_eq!(
        *status_rx.borrow(),
        AgentStatus::Registered {
            pin: "111111".to_string()
        }
    );
    expect_final_stats(&mut out_rx);
    assert_eq!(
        out_rx.try_recv().expect("expected a Bye message"),
        SignalMessage::Bye {
            session_id: "sess-1".to_string(),
            reason: None,
        }
    );

    // Our own `ActiveSession::shutdown` (called above) closing the peer
    // connection can deliver a second `DataChannelClosed` for the same
    // channel once the slot is already empty -- must be a quiet no-op,
    // not a double status flip, a panic, or a second `Bye`. Same `tag`
    // as before: from the *now-gone* session's own control channel.
    handle_session_event(
        SessionEvent::DataChannelClosed {
            label: "control".to_string(),
            tag,
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
    assert!(active.is_none());
    assert!(out_rx.try_recv().is_err(), "no second Bye should be sent");
}

/// A non-`control` data channel closing (e.g. `input`) must not end the
/// session -- only `control` is treated as a deliberate end of session.
#[tokio::test(flavor = "multi_thread")]
async fn non_control_data_channel_closed_does_not_end_the_session() {
    let (ctx, mut active, mut current_session_id, out_tx, _out_rx, event_tx, status_tx, _) =
        started_session("sess-1").await;

    handle_session_event(
        SessionEvent::DataChannelClosed {
            label: "input".to_string(),
            tag: active.as_ref().unwrap().session_tag,
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

    assert!(active.is_some());
    assert_eq!(current_session_id.as_deref(), Some("sess-1"));

    if let Some(active) = active.take() {
        active.shutdown().await;
    }
}
