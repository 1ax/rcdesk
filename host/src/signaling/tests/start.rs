use super::*;

#[test]
fn displays_message_builds_the_expected_control_message() {
    let displays = vec![display(1, "Built-in", true), display(2, "External", false)];

    let msg = displays_message(&displays, 2);

    assert_eq!(
        msg,
        ControlMessage::Displays {
            displays: vec![
                DisplayEntry {
                    id: 1,
                    title: "Built-in".to_string(),
                    width: 100,
                    height: 100,
                    primary: true,
                },
                DisplayEntry {
                    id: 2,
                    title: "External".to_string(),
                    width: 100,
                    height: 100,
                    primary: false,
                },
            ],
            current: 2,
        }
    );
}

/// Collects frames from `rx` until either `n` arrive or `timeout` elapses,
/// panicking on timeout (the synthetic source delivers frames regularly,
/// so a stall means the pipeline broke).
async fn collect_frames(
    rx: &mut mpsc::Receiver<EncodedFrame>,
    n: usize,
    timeout: Duration,
) -> Vec<EncodedFrame> {
    let mut frames = Vec::with_capacity(n);
    tokio::time::timeout(timeout, async {
        while frames.len() < n {
            match rx.recv().await {
                Some(frame) => frames.push(frame),
                None => break,
            }
        }
    })
    .await
    .expect("timed out waiting for frames");
    frames
}

/// Reads frames from `rx` until a keyframe is seen or `timeout` elapses,
/// panicking on timeout.
async fn wait_for_keyframe(
    rx: &mut mpsc::Receiver<EncodedFrame>,
    timeout: Duration,
) -> EncodedFrame {
    tokio::time::timeout(timeout, async {
        loop {
            let frame = rx.recv().await.expect("video channel closed");
            if frame.keyframe {
                return frame;
            }
        }
    })
    .await
    .expect("timed out waiting for a keyframe")
}

#[tokio::test(flavor = "multi_thread")]
async fn switching_display_restarts_pipeline_and_keeps_frames_flowing() {
    let ctx = test_ctx();
    let (events_tx, _events_rx) = mpsc::channel(64);
    let peer = PeerSession::new(ctx.session.clone(), events_tx, Arc::clone(&ctx.runtime))
        .await
        .unwrap();

    let (video_tx, mut video_rx) = mpsc::channel(4);
    let keyframe_flag = Arc::new(AtomicBool::new(false));
    let generation = Arc::new(AtomicU64::new(0));

    let displays = capture::synthetic::list_displays();
    let display1 = displays.iter().find(|d| d.id == 1).unwrap().clone();
    let display2 = displays.iter().find(|d| d.id == 2).unwrap().clone();

    let mut current = VideoPipeline::start(
        &ctx,
        display1,
        Arc::clone(&peer),
        video_tx.clone(),
        Arc::clone(&keyframe_flag),
        0,
        Arc::clone(&generation),
        &[],
    )
    .await
    .unwrap();

    let first_batch = collect_frames(&mut video_rx, 5, Duration::from_secs(5)).await;
    assert!(
        first_batch[0].keyframe,
        "first frame of a fresh pipeline must be a keyframe"
    );

    generation.store(1, Ordering::SeqCst);
    let next = VideoPipeline::start(
        &ctx,
        display2,
        Arc::clone(&peer),
        video_tx.clone(),
        Arc::clone(&keyframe_flag),
        1,
        Arc::clone(&generation),
        &[],
    )
    .await
    .unwrap();
    let old = std::mem::replace(&mut current, next);
    old.stop();

    // The new pipeline forces a keyframe on start; frames from the old
    // one may still be interleaved briefly, so look for the first
    // keyframe after the switch rather than assuming the very next
    // frame is it.
    let _keyframe_after_switch = wait_for_keyframe(&mut video_rx, Duration::from_secs(5)).await;

    // Frames keep flowing after the switch.
    let _more = collect_frames(&mut video_rx, 5, Duration::from_secs(5)).await;

    current.stop();
}

/// D24: once a newer generation is announced, the older pipeline's
/// forward task must stop writing into the shared channel (frames already
/// in the channel or in flight at that moment may still land).
#[tokio::test(flavor = "multi_thread")]
async fn superseded_pipeline_stops_sending_frames() {
    let ctx = test_ctx();
    let (events_tx, _events_rx) = mpsc::channel(64);
    let peer = PeerSession::new(ctx.session.clone(), events_tx, Arc::clone(&ctx.runtime))
        .await
        .unwrap();
    let (video_tx, mut video_rx) = mpsc::channel(4);
    let keyframe_flag = Arc::new(AtomicBool::new(false));
    let generation = Arc::new(AtomicU64::new(0));
    let display = capture::synthetic::list_displays().remove(0);
    let pipeline = VideoPipeline::start(
        &ctx,
        display,
        peer,
        video_tx,
        keyframe_flag,
        0,
        Arc::clone(&generation),
        &[],
    )
    .await
    .unwrap();
    collect_frames(&mut video_rx, 3, Duration::from_secs(5)).await;

    generation.store(1, Ordering::SeqCst);
    // Let the channel fill and the forward task block in `send`, then
    // drain what was already queued (plus the one in-flight frame).
    for _ in 0..2 {
        tokio::time::sleep(Duration::from_millis(200)).await;
        while video_rx.try_recv().is_ok() {}
    }
    let late = tokio::time::timeout(Duration::from_millis(500), video_rx.recv()).await;
    assert!(late.is_err(), "superseded pipeline still sent a frame");

    // Without a newer generation the pipeline would have kept sending
    // (sanity check that the silence above comes from the gate).
    generation.store(0, Ordering::SeqCst);
    collect_frames(&mut video_rx, 1, Duration::from_secs(5)).await;
    pipeline.stop();
}

#[tokio::test(flavor = "multi_thread")]
async fn start_session_survives_a_failing_injector_and_reports_view_only() {
    let mut ctx = test_ctx();
    ctx.build_injector = Box::new(|| Err(anyhow::anyhow!("no Accessibility permission")));
    let (events_tx, _events_rx) = mpsc::channel(64);

    let parts = start_session(&ctx, &[], events_tx)
        .await
        .expect("a failing injector must not fail session start");

    assert!(!parts.input_available);
    assert_eq!(
        parts.input_reason.as_deref(),
        Some("no Accessibility permission")
    );

    // `parts.video`'s own frame channel is consumed internally by
    // `PeerSession::start_video`, which only forwards frames once DTLS
    // has actually negotiated -- unobservable from a unit test with no
    // real peer. Instead, prove the video path itself (built by
    // `start_session` the same way, from the same `ctx`/display, before
    // `build_injector` is ever called) really produces frames, the same
    // way `switching_display_restarts_pipeline_and_keeps_frames_flowing`
    // does above.
    let (video_tx, mut video_rx) = mpsc::channel(4);
    let keyframe_flag = Arc::new(AtomicBool::new(false));
    let pipeline = VideoPipeline::start(
        &ctx,
        parts.video.display.clone(),
        Arc::clone(&parts.peer),
        video_tx,
        keyframe_flag,
        0,
        Arc::new(AtomicU64::new(0)),
        &[],
    )
    .await
    .unwrap();
    let frames = collect_frames(&mut video_rx, 5, Duration::from_secs(5)).await;
    assert!(frames[0].keyframe, "first frame must be a keyframe");
    pipeline.stop();

    parts.video.stop();
    parts.cursor_task.abort();
    if let Some(task) = parts.elevation_task {
        task.abort();
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn start_session_reports_input_available_with_a_working_injector() {
    let ctx = test_ctx();
    let (events_tx, _events_rx) = mpsc::channel(64);

    let parts = start_session(&ctx, &[], events_tx).await.unwrap();

    assert!(parts.input_available);
    assert_eq!(parts.input_reason, None);

    parts.video.stop();
    parts.cursor_task.abort();
    if let Some(task) = parts.elevation_task {
        task.abort();
    }
}

#[tokio::test]
async fn empty_display_list_yields_bye_with_display_unavailable() {
    let mut ctx = test_ctx();
    ctx.list_displays = Arc::new(|| Ok(Vec::new()));
    let (events_tx, _events_rx) = mpsc::channel(64);

    let err = start_session_with_display_wait(&ctx, &[], events_tx, Duration::from_millis(30))
        .await
        .err()
        .expect("no display must fail the start");

    assert_eq!(
        bye_for_start_error("sess-1".to_string(), &err),
        SignalMessage::Bye {
            session_id: "sess-1".to_string(),
            reason: Some(proto::signal::ByeReason::DisplayUnavailable),
        }
    );
    let other = anyhow::anyhow!("synthetic source failure");
    assert_eq!(
        bye_for_start_error("sess-1".to_string(), &other),
        SignalMessage::Bye {
            session_id: "sess-1".to_string(),
            reason: None,
        }
    );
}

/// V-2: вычитывает исходящие сообщения и сообщает, был ли среди них `Bye`.
fn out_tx_has_bye(rx: &mut mpsc::UnboundedReceiver<SignalMessage>) -> bool {
    let mut found = false;
    while let Ok(msg) = rx.try_recv() {
        found |= matches!(msg, SignalMessage::Bye { .. });
    }
    found
}

/// D58 + V-2: a capture source that dies mid-session first gets the
/// pipeline restarted (up to `PIPELINE_RECOVERY_MAX` times); only when the
/// budget is exhausted does the session end with `Bye { VideoFailed }` and
/// the agent return to `Registered`.
#[tokio::test(flavor = "multi_thread")]
async fn capture_failure_restarts_pipeline_until_budget_is_exhausted() {
    struct DyingSource {
        inner: Box<dyn FrameSource>,
        left: u32,
    }
    impl FrameSource for DyingSource {
        fn next_frame(&mut self) -> Result<capture::RawFrame, capture::CaptureError> {
            if self.left == 0 {
                return Err(capture::CaptureError::Backend("boom".into()));
            }
            self.left -= 1;
            self.inner.next_frame()
        }
        fn size(&self) -> (u32, u32) {
            self.inner.size()
        }
    }

    let mut ctx = test_ctx();
    ctx.build_source = Arc::new(|id, _exclude| {
        Ok(Box::new(DyingSource {
            inner: Box::new(capture::synthetic::for_display(id, 30)?),
            left: 3,
        }) as Box<dyn FrameSource>)
    });
    let (out_tx, mut out_rx) = mpsc::unbounded_channel::<SignalMessage>();
    let (event_tx, mut event_rx) = mpsc::channel(64);
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
        event_tx.clone(),
        &mut active,
        &mut current_session_id,
        &mut None,
        &mut 0,
        &mut None,
    )
    .await;
    assert!(active.is_some(), "session must have started successfully");
    let tag = active.as_ref().unwrap().session_tag;

    // V-2: каждый новый конвейер снова умирает через 3 кадра. Первые
    // PIPELINE_RECOVERY_MAX сбоев -- перезапуск (сеанс жив), следующий --
    // конец сеанса.
    for attempt in 1..=PIPELINE_RECOVERY_MAX + 1 {
        let failed = tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                match event_rx.recv().await {
                    Some(SessionEvent::VideoFailed { tag }) => break Some(tag),
                    Some(_) => continue,
                    None => break None,
                }
            }
        })
        .await
        .expect("VideoFailed must arrive");
        assert_eq!(failed, Some(tag));

        handle_session_event(
            SessionEvent::VideoFailed { tag },
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

        if attempt <= PIPELINE_RECOVERY_MAX {
            assert!(active.is_some(), "attempt {attempt}: session must survive");
            assert_eq!(current_session_id.as_deref(), Some("sess-1"));
            assert!(
                !out_tx_has_bye(&mut out_rx),
                "attempt {attempt}: no Bye while recovering"
            );
        }
    }

    assert!(active.is_none());
    assert_eq!(current_session_id, None);
    assert_eq!(
        *status_rx.borrow(),
        AgentStatus::Registered {
            pin: "111111".to_string()
        }
    );
    let mut found = false;
    while let Ok(msg) = out_rx.try_recv() {
        if msg
            == (SignalMessage::Bye {
                session_id: "sess-1".to_string(),
                reason: Some(proto::signal::ByeReason::VideoFailed),
            })
        {
            found = true;
        }
    }
    assert!(found, "expected Bye with VideoFailed");
}

#[tokio::test(flavor = "multi_thread")]
async fn peer_joined_sends_bye_when_session_start_fails() {
    let mut ctx = test_ctx();
    ctx.build_source = Arc::new(|_id, _exclude| anyhow::bail!("synthetic source failure"));
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
        event_tx,
        &mut active,
        &mut current_session_id,
        &mut None,
        &mut 0,
        &mut None,
    )
    .await;

    assert!(active.is_none());
    assert_eq!(current_session_id, None);
    assert_eq!(
        out_rx.try_recv().expect("expected a Bye message"),
        SignalMessage::Bye {
            session_id: "sess-1".to_string(),
            reason: None,
        }
    );
}

/// M-2: `SessionParts::abort` закрывает `PeerConnection`.
#[tokio::test(flavor = "multi_thread")]
async fn session_parts_abort_closes_peer() {
    let ctx = test_ctx();
    let (events_tx, _events_rx) = mpsc::channel(64);
    let parts = start_session(&ctx, &[], events_tx)
        .await
        .expect("session must start");
    let peer = Arc::clone(&parts.peer);
    assert!(!peer.is_closed());

    parts.abort().await;

    assert!(peer.is_closed());
}

#[tokio::test(flavor = "multi_thread")]
async fn select_display_is_rate_limited_within_a_second() {
    let (ctx, _log, calls) = curtain_ctx(vec![], None);
    let (mut active, _out, _etx, _erx) = start_active_session(&ctx).await;
    let first = active.video.display.id;
    let other = capture::synthetic::list_displays()
        .into_iter()
        .find(|d| d.id != first)
        .unwrap()
        .id;
    let base = calls.lock().unwrap().len();

    switch_display(&ctx, &mut active, other).await;
    assert_eq!(active.video.display.id, other);
    assert_eq!(calls.lock().unwrap().len(), base + 1);

    // Сразу обратно: лимит, конвейер не пересобирается.
    switch_display(&ctx, &mut active, first).await;
    assert_eq!(active.video.display.id, other);
    assert_eq!(calls.lock().unwrap().len(), base + 1);

    active.last_display_switch =
        Some(std::time::Instant::now() - std::time::Duration::from_secs(2));
    switch_display(&ctx, &mut active, first).await;
    assert_eq!(active.video.display.id, first);
    assert_eq!(calls.lock().unwrap().len(), base + 2);
    active.shutdown().await;
}
