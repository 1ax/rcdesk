use super::*;

fn journal(log: &Arc<Mutex<Vec<String>>>) -> Vec<String> {
    log.lock().unwrap().clone()
}

fn curtain_status(on: bool, reason: Option<&str>) -> ControlMessage {
    ControlMessage::CurtainStatus {
        on,
        reason: reason.map(str::to_string),
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn set_curtain_without_curtain_is_refused() {
    let ctx = test_ctx();
    let (mut active, _out, _etx, _erx) = start_active_session(&ctx).await;

    let reply = set_curtain(&ctx, &mut active, true).await;

    assert_eq!(
        reply,
        curtain_status(false, Some("шторка недоступна на этом хосте"))
    );
    assert!(active.curtain_windows.is_empty());
    active.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn set_curtain_on_restarts_pipeline_excluding_curtain_windows() {
    let (ctx, log, calls) = curtain_ctx(vec![101, 102], None);
    let (mut active, _out, _etx, _erx) = start_active_session(&ctx).await;
    let display_id = active.video.display.id;
    assert_eq!(*calls.lock().unwrap(), vec![(display_id, vec![])]);

    let reply = set_curtain(&ctx, &mut active, true).await;

    assert_eq!(reply, curtain_status(true, None));
    assert_eq!(journal(&log), vec!["show"]);
    assert_eq!(active.curtain_windows, vec![101, 102]);
    assert_eq!(
        *calls.lock().unwrap(),
        vec![(display_id, vec![]), (display_id, vec![101, 102])],
        "pipeline must be rebuilt on the same display with the curtain windows excluded"
    );

    // Повторное включение -- просто повтор статуса, без нового show.
    let again = set_curtain(&ctx, &mut active, true).await;
    assert_eq!(again, curtain_status(true, None));
    assert_eq!(journal(&log), vec!["show"]);
    assert_eq!(calls.lock().unwrap().len(), 2);

    active.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn set_curtain_show_error_reports_reason_and_keeps_pipeline() {
    let (ctx, log, calls) = curtain_ctx(vec![101], Some("нет прав"));
    let (mut active, _out, _etx, _erx) = start_active_session(&ctx).await;

    let reply = set_curtain(&ctx, &mut active, true).await;

    assert_eq!(reply, curtain_status(false, Some("нет прав")));
    assert!(active.curtain_windows.is_empty());
    assert_eq!(calls.lock().unwrap().len(), 1, "no pipeline rebuild");
    assert_eq!(journal(&log), vec!["show"]);

    active.shutdown().await;
    assert_eq!(
        journal(&log),
        vec!["show"],
        "curtain is off: nothing at shutdown"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn set_curtain_rolls_back_when_pipeline_restart_fails() {
    let (mut ctx, log, _calls) = curtain_ctx(vec![101], None);
    let (mut active, _out, _etx, _erx) = start_active_session(&ctx).await;
    ctx.build_source = Arc::new(|_id, _exclude| anyhow::bail!("restart failure"));

    let reply = set_curtain(&ctx, &mut active, true).await;

    assert_eq!(reply, curtain_status(false, Some("restart failure")));
    assert!(active.curtain_windows.is_empty());
    assert_eq!(
        active.pipeline_generation.load(Ordering::SeqCst),
        0,
        "failed restart must hand the generation back to the running pipeline"
    );
    assert_eq!(journal(&log), vec!["show", "hide"]);

    active.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn set_curtain_off_hides_without_restarting_pipeline() {
    let (ctx, log, calls) = curtain_ctx(vec![101], None);
    let (mut active, _out, _etx, _erx) = start_active_session(&ctx).await;
    set_curtain(&ctx, &mut active, true).await;
    assert_eq!(calls.lock().unwrap().len(), 2);

    let reply = set_curtain(&ctx, &mut active, false).await;

    assert_eq!(reply, curtain_status(false, None));
    assert_eq!(journal(&log), vec!["show", "hide"]);
    assert!(active.curtain_windows.is_empty());
    assert_eq!(calls.lock().unwrap().len(), 2, "no pipeline rebuild on off");

    active.shutdown().await;
    assert_eq!(journal(&log), vec!["show", "hide"]);
}

#[tokio::test(flavor = "multi_thread")]
async fn shutdown_with_curtain_on_locks_screen_before_hiding() {
    let (ctx, log, _calls) = curtain_ctx(vec![101], None);
    let (mut active, _out, _etx, _erx) = start_active_session(&ctx).await;
    set_curtain(&ctx, &mut active, true).await;

    active.shutdown().await;

    assert_eq!(journal(&log), vec!["show", "lock_screen", "hide"]);
}

#[tokio::test(flavor = "multi_thread")]
async fn shutdown_keeps_curtain_when_lock_fails() {
    let (ctx, log, _calls) = curtain_ctx_with_lock_error(vec![101], None, Some("no lock"));
    let (mut active, _out, _etx, _erx) = start_active_session(&ctx).await;
    set_curtain(&ctx, &mut active, true).await;

    active.shutdown().await;

    assert_eq!(journal(&log), vec!["show", "lock_screen"]);
    assert!(!ctx.curtain.as_ref().unwrap().shown().is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn new_session_excludes_already_shown_curtain_windows() {
    let (ctx, _log, calls) = curtain_ctx(vec![101, 102], None);
    ctx.curtain.as_ref().unwrap().show().unwrap();

    let (active, _out, _etx, _erx) = start_active_session(&ctx).await;

    assert_eq!(active.curtain_windows, vec![101, 102]);
    let first = calls.lock().unwrap().first().cloned().unwrap();
    assert_eq!(first.1, vec![101, 102]);
    active.shutdown().await;
}

/// Перехватить control-сообщения в тестах нельзя без расширения
/// инфраструктуры `PeerSession`, поэтому проверяем чистую функцию,
/// из которой `DataChannelOpen` берёт сообщение.
#[test]
fn control_open_announces_curtain_on_when_already_shown() {
    assert_eq!(
        curtain_announce_message(&[101, 102]),
        Some(curtain_status(true, None))
    );
    assert_eq!(curtain_announce_message(&[]), None);
}

#[tokio::test(flavor = "multi_thread")]
async fn shutdown_without_curtain_never_touches_it() {
    let (ctx, log, _calls) = curtain_ctx(vec![101], None);
    let (active, _out, _etx, _erx) = start_active_session(&ctx).await;

    active.shutdown().await;

    assert!(journal(&log).is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn switching_display_with_curtain_on_excludes_curtain_windows() {
    let (ctx, _log, calls) = curtain_ctx(vec![101, 102], None);
    let (mut active, _out, _etx, _erx) = start_active_session(&ctx).await;
    set_curtain(&ctx, &mut active, true).await;
    let other = capture::synthetic::list_displays()
        .into_iter()
        .find(|d| d.id != active.video.display.id)
        .unwrap()
        .id;

    switch_display(&ctx, &mut active, other).await;

    assert_eq!(active.video.display.id, other);
    assert_eq!(
        calls.lock().unwrap().last().cloned(),
        Some((other, vec![101, 102]))
    );
    // set_curtain + switch_display: two replacements -> generation 2.
    assert_eq!(active.pipeline_generation.load(Ordering::SeqCst), 2);
    active.shutdown().await;
}
