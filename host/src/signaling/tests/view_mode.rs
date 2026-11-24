//! Режим просмотра (VM-1): на хосте ничего не делается от имени зрителя.

use super::*;
use crate::input::Injector;
use crate::signaling::control::set_view_mode;
use crate::signaling::handlers::on_data_channel_message;
use proto::input::{InputMessage, PointerButton};

type Calls = Arc<Mutex<Vec<String>>>;

/// Фейковый инжектор: пишет вызовы в общий журнал.
struct Recording(Calls);

impl Injector for Recording {
    fn pointer_move(&mut self, x: i32, y: i32) {
        self.0.lock().unwrap().push(format!("move {x} {y}"));
    }
    fn button(&mut self, button: PointerButton, pressed: bool) {
        self.0
            .lock()
            .unwrap()
            .push(format!("button {button:?} {pressed}"));
    }
    fn wheel(&mut self, dx: i32, dy: i32) {
        self.0.lock().unwrap().push(format!("wheel {dx} {dy}"));
    }
    fn key(&mut self, keycode: u16, pressed: bool) {
        self.0
            .lock()
            .unwrap()
            .push(format!("key {keycode} {pressed}"));
    }
    fn text(&mut self, text: &str) {
        self.0.lock().unwrap().push(format!("text {text}"));
    }
    fn screen_size(&self) -> (i32, i32) {
        (1920, 1080)
    }
}

fn recording_ctx() -> (HostContext, Calls) {
    let calls: Calls = Arc::new(Mutex::new(Vec::new()));
    let mut ctx = test_ctx();
    let shared = Arc::clone(&calls);
    ctx.build_injector =
        Box::new(move || Ok(Box::new(Recording(Arc::clone(&shared))) as Box<dyn Injector>));
    (ctx, calls)
}

fn snapshot(calls: &Calls) -> Vec<String> {
    calls.lock().unwrap().clone()
}

async fn wait_for(calls: &Calls, want: impl Fn(&[String]) -> bool) -> bool {
    for _ in 0..200 {
        if want(&snapshot(calls)) {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    false
}

async fn send_input(ctx: &HostContext, active: &mut Option<ActiveSession>, msg: &InputMessage) {
    let data = bytes::Bytes::from(serde_json::to_vec(msg).unwrap());
    on_data_channel_message("input".to_string(), data, true, ctx, active).await;
}

async fn send_control(ctx: &HostContext, active: &mut Option<ActiveSession>, msg: &ControlMessage) {
    let data = bytes::Bytes::from(serde_json::to_vec(msg).unwrap());
    on_data_channel_message("control".to_string(), data, true, ctx, active).await;
}

fn scratch_settings_dir(name: &str, content: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "rcdesk-host-view-mode-test-{name}-{}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join(crate::settings::SETTINGS_FILE), content).unwrap();
    dir
}

#[tokio::test(flavor = "multi_thread")]
async fn input_passes_in_control_mode_and_is_dropped_in_view_mode() {
    let (ctx, calls) = recording_ctx();
    let (active, _out, _etx, _erx) = start_active_session(&ctx).await;
    assert!(!active.view_mode.load(Ordering::Relaxed));
    let mut active = Some(active);

    send_input(&ctx, &mut active, &InputMessage::Text { text: "a".into() }).await;
    assert!(
        wait_for(&calls, |c| c.contains(&"text a".to_string())).await,
        "в режиме управления ввод проходит: {:?}",
        snapshot(&calls)
    );

    let replies = set_view_mode(&ctx, active.as_mut().unwrap(), true).await;
    assert_eq!(replies, vec![ControlMessage::ViewMode { on: true }]);
    send_input(&ctx, &mut active, &InputMessage::Text { text: "b".into() }).await;
    send_input(
        &ctx,
        &mut active,
        &InputMessage::ClipboardText { text: "x".into() },
    )
    .await;
    tokio::time::sleep(Duration::from_millis(150)).await;
    assert!(
        !snapshot(&calls).contains(&"text b".to_string()),
        "в просмотре ввод отброшен: {:?}",
        snapshot(&calls)
    );

    let replies = set_view_mode(&ctx, active.as_mut().unwrap(), false).await;
    assert_eq!(replies, vec![ControlMessage::ViewMode { on: false }]);
    send_input(&ctx, &mut active, &InputMessage::Text { text: "c".into() }).await;
    assert!(wait_for(&calls, |c| c.contains(&"text c".to_string())).await);
    active.unwrap().shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn entering_view_mode_releases_held_buttons() {
    let (ctx, calls) = recording_ctx();
    let (active, _out, _etx, _erx) = start_active_session(&ctx).await;
    let mut active = Some(active);
    send_input(
        &ctx,
        &mut active,
        &InputMessage::PointerButton {
            button: PointerButton::Left,
            pressed: true,
            x: 0.5,
            y: 0.5,
        },
    )
    .await;
    assert!(wait_for(&calls, |c| c.iter().any(|s| s.starts_with("button"))).await);

    set_view_mode(&ctx, active.as_mut().unwrap(), true).await;

    assert!(
        wait_for(&calls, |c| c
            .iter()
            .any(|s| s.ends_with("false") && s.starts_with("button")))
        .await,
        "зажатая кнопка должна отпуститься: {:?}",
        snapshot(&calls)
    );
    active.unwrap().shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn set_view_mode_without_input_is_ignored() {
    let mut ctx = test_ctx();
    ctx.build_injector = Box::new(|| Err(anyhow::anyhow!("нет ввода")));
    let (mut active, _out, _etx, _erx) = start_active_session(&ctx).await;
    assert!(!active.input_available);

    let replies = set_view_mode(&ctx, &mut active, true).await;

    assert!(replies.is_empty());
    assert!(!active.view_mode.load(Ordering::Relaxed));
    active.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn setting_default_starts_session_in_view_mode_only_with_input() {
    let dir = scratch_settings_dir("default", r#"{"view_only":true}"#);

    let mut ctx = test_ctx();
    ctx.settings_dir = dir.clone();
    let (active, _out, _etx, _erx) = start_active_session(&ctx).await;
    assert!(active.input_available);
    assert!(active.view_mode.load(Ordering::Relaxed));
    active.shutdown().await;

    let mut no_input = test_ctx();
    no_input.settings_dir = dir.clone();
    no_input.build_injector = Box::new(|| Err(anyhow::anyhow!("нет ввода")));
    let (active, _out, _etx, _erx) = start_active_session(&no_input).await;
    assert!(!active.view_mode.load(Ordering::Relaxed));
    active.shutdown().await;

    // Настройка читается в начале каждого сеанса: смена действует сразу.
    std::fs::write(
        dir.join(crate::settings::SETTINGS_FILE),
        r#"{"view_only":false}"#,
    )
    .unwrap();
    let (active, _out, _etx, _erx) = start_active_session(&ctx).await;
    assert!(!active.view_mode.load(Ordering::Relaxed));
    active.shutdown().await;
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test(flavor = "multi_thread")]
async fn entering_view_mode_drops_the_curtain() {
    let (ctx, log, _calls) = curtain_ctx(vec![101], None);
    let (mut active, _out, _etx, _erx) = start_active_session(&ctx).await;
    assert_eq!(
        set_curtain(&ctx, &mut active, true).await,
        ControlMessage::CurtainStatus {
            on: true,
            reason: None
        }
    );

    let replies = set_view_mode(&ctx, &mut active, true).await;

    assert_eq!(
        replies,
        vec![
            ControlMessage::CurtainStatus {
                on: false,
                reason: None
            },
            ControlMessage::ViewMode { on: true }
        ]
    );
    assert!(active.curtain_windows.is_empty());
    assert!(log.lock().unwrap().contains(&"hide".to_string()));
    active.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn curtain_request_is_refused_in_view_mode() {
    let (ctx, log, _calls) = curtain_ctx(vec![101], None);
    let (active, _out, _etx, _erx) = start_active_session(&ctx).await;
    let mut active = Some(active);
    set_view_mode(&ctx, active.as_mut().unwrap(), true).await;

    send_control(&ctx, &mut active, &ControlMessage::SetCurtain { on: true }).await;

    let active = active.unwrap();
    assert!(active.curtain_windows.is_empty());
    assert!(
        !log.lock().unwrap().contains(&"show".to_string()),
        "шторка не должна показываться в просмотре"
    );
    active.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn set_view_mode_message_toggles_the_shared_flag() {
    let ctx = test_ctx();
    let (active, _out, _etx, _erx) = start_active_session(&ctx).await;
    let mut active = Some(active);

    send_control(&ctx, &mut active, &ControlMessage::SetViewMode { on: true }).await;
    assert!(active.as_ref().unwrap().view_mode.load(Ordering::Relaxed));
    send_control(
        &ctx,
        &mut active,
        &ControlMessage::SetViewMode { on: false },
    )
    .await;
    assert!(!active.as_ref().unwrap().view_mode.load(Ordering::Relaxed));
    active.unwrap().shutdown().await;
}

/// Источник положения, считающий обращения к платформе.
struct CountingPosition(Arc<std::sync::atomic::AtomicUsize>);

impl crate::cursor::PositionSource for CountingPosition {
    fn position(&mut self) -> Option<(i32, i32)> {
        self.0.fetch_add(1, Ordering::Relaxed);
        Some((10, 10))
    }
}

/// VM-2: в режиме управления платформа не читается вообще, в просмотре --
/// читается, а выход из просмотра снова её замолкает.
#[tokio::test(flavor = "multi_thread")]
async fn position_is_read_only_in_view_mode() {
    let reads = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let mut ctx = test_ctx();
    let counter = Arc::clone(&reads);
    ctx.build_position_source = Box::new(move || {
        Box::new(CountingPosition(Arc::clone(&counter))) as Box<dyn crate::cursor::PositionSource>
    });
    let (mut active, _out, _etx, _erx) = start_active_session(&ctx).await;

    tokio::time::sleep(Duration::from_millis(150)).await;
    assert_eq!(reads.load(Ordering::Relaxed), 0, "control mode: no reads");

    set_view_mode(&ctx, &mut active, true).await;
    tokio::time::sleep(Duration::from_millis(250)).await;
    assert!(reads.load(Ordering::Relaxed) > 0, "view mode: polling");

    set_view_mode(&ctx, &mut active, false).await;
    tokio::time::sleep(Duration::from_millis(100)).await;
    let after_off = reads.load(Ordering::Relaxed);
    tokio::time::sleep(Duration::from_millis(150)).await;
    assert_eq!(
        reads.load(Ordering::Relaxed),
        after_off,
        "stopped after exit"
    );
    active.shutdown().await;
}
