//! Mouse/keyboard input: turns `proto::input::InputMessage`s arriving over
//! the `input`/`pointer` data channels (see `crate::signaling`) into calls on
//! an `Injector`, on a dedicated thread so a slow/blocking platform call
//! (e.g. waiting on the macOS accessibility permission) never stalls the
//! tokio runtime driving the rest of the session.

use std::collections::HashSet;
use std::sync::mpsc as std_mpsc;
use std::sync::{Arc, Mutex};
use std::thread;

use proto::input::{InputMessage, PointerButton};

pub mod keymap;

#[cfg(any(target_os = "macos", target_os = "windows"))]
pub mod enigo;

/// Something that can inject synthetic mouse/keyboard input into the host
/// OS.
///
/// No method returns a `Result`: a failed injection (permission missing,
/// platform call error) is logged by the implementation and otherwise
/// ignored -- bad input must never take down the session.
pub trait Injector: Send {
    /// Moves the pointer to an absolute position, in the same pixel units as
    /// `screen_size()`.
    fn pointer_move(&mut self, x: i32, y: i32);
    /// Presses or releases a mouse button at the pointer's current position.
    fn button(&mut self, button: PointerButton, pressed: bool);
    /// Scrolls, in "lines/clicks" (see `proto::input::InputMessage::Wheel`).
    fn wheel(&mut self, dx: i32, dy: i32);
    /// Presses or releases a key by platform-specific raw keycode (see
    /// `keymap::to_keycode`).
    fn key(&mut self, keycode: u16, pressed: bool);
    /// The size, in the same units `pointer_move` expects, of the screen
    /// normalized pointer coordinates are relative to.
    fn screen_size(&self) -> (i32, i32);
}

/// An `Injector` that only logs what it would have done. Used on platforms
/// without a real backend (anything but macOS/Windows), so a session can be
/// exercised end to end without touching the real mouse/keyboard, and by
/// `crate::signaling::start_session` as the fallback when `HostContext::build_injector`
/// fails (e.g. missing the macOS Accessibility permission, or `serve
/// --no-input`, which fails on purpose -- see `main.rs` -- so the client
/// gets a clear `ControlMessage::InputStatus` reason instead of a generic
/// one; slice 2.5a, debt D26): the session still streams video, just
/// view-only.
pub struct NoopInjector {
    screen: (i32, i32),
}

impl NoopInjector {
    /// `screen` defaults to a plausible 1080p desktop; nothing here actually
    /// depends on it being accurate since no real injection happens.
    pub fn new() -> Self {
        Self {
            screen: (1920, 1080),
        }
    }
}

impl Default for NoopInjector {
    fn default() -> Self {
        Self::new()
    }
}

impl Injector for NoopInjector {
    fn pointer_move(&mut self, x: i32, y: i32) {
        tracing::debug!(x, y, "no-op injector: pointer_move");
    }

    fn button(&mut self, button: PointerButton, pressed: bool) {
        tracing::debug!(?button, pressed, "no-op injector: button");
    }

    fn wheel(&mut self, dx: i32, dy: i32) {
        tracing::debug!(dx, dy, "no-op injector: wheel");
    }

    fn key(&mut self, keycode: u16, pressed: bool) {
        tracing::debug!(keycode, pressed, "no-op injector: key");
    }

    fn screen_size(&self) -> (i32, i32) {
        self.screen
    }
}

/// Scales a normalized `[0,1]` coordinate to a pixel index in `[0, size-1]`,
/// rounding to the nearest pixel and clamping out-of-range input (a client
/// can legitimately report a point right at/just past the edge of its
/// capture rect due to rounding on its side).
fn scale_and_clamp(value: f64, size: i32) -> i32 {
    if size <= 0 {
        return 0;
    }
    let scaled = (value * f64::from(size)).round() as i32;
    scaled.clamp(0, size - 1)
}

/// The rectangle, in the same global coordinate units `Injector::pointer_move`
/// expects, of the display currently being captured/streamed. Normalized
/// `[0,1]` pointer coordinates from the client are scaled against this rect
/// (not the whole screen) so a click lands on the right monitor when the
/// captured display isn't the primary one -- see `to_pixel`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CaptureRect {
    pub x: i32,
    pub y: i32,
    pub width: i32,
    pub height: i32,
}

impl From<&crate::capture::DisplayInfo> for CaptureRect {
    fn from(display: &crate::capture::DisplayInfo) -> Self {
        CaptureRect {
            x: display.x,
            y: display.y,
            width: display.width as i32,
            height: display.height as i32,
        }
    }
}

/// Scales a normalized `[0,1]` point to an absolute pixel position within
/// `rect`, by offsetting `scale_and_clamp`'s result by the rect's origin.
fn to_pixel(x: f64, y: f64, rect: CaptureRect) -> (i32, i32) {
    (
        rect.x + scale_and_clamp(x, rect.width),
        rect.y + scale_and_clamp(y, rect.height),
    )
}

/// Applies one `InputMessage` to `injector`, keeping `pressed_keys` and
/// `pressed_buttons` in sync so `release_all` can undo exactly what's
/// currently held. `rect` is the currently captured display's rectangle (see
/// `CaptureRect`), used to place normalized pointer coordinates.
fn apply(
    msg: &InputMessage,
    injector: &mut dyn Injector,
    pressed_keys: &mut HashSet<u16>,
    pressed_buttons: &mut HashSet<PointerButton>,
    rect: CaptureRect,
) {
    match msg {
        InputMessage::PointerMove { x, y } => {
            let (px, py) = to_pixel(*x, *y, rect);
            injector.pointer_move(px, py);
        }
        InputMessage::PointerButton {
            button,
            pressed,
            x,
            y,
        } => {
            // Move first: `pointer` and `input` are independent, unordered
            // channels, so the last `PointerMove` the host applied may be
            // stale relative to this click's position.
            let (px, py) = to_pixel(*x, *y, rect);
            injector.pointer_move(px, py);
            injector.button(*button, *pressed);
            if *pressed {
                pressed_buttons.insert(*button);
            } else {
                pressed_buttons.remove(button);
            }
        }
        InputMessage::Wheel { dx, dy, x, y } => {
            let (px, py) = to_pixel(*x, *y, rect);
            injector.pointer_move(px, py);
            injector.wheel(dx.round() as i32, dy.round() as i32);
        }
        InputMessage::Key { code, pressed } => match keymap::to_keycode(code) {
            Some(keycode) => {
                injector.key(keycode, *pressed);
                if *pressed {
                    pressed_keys.insert(keycode);
                } else {
                    pressed_keys.remove(&keycode);
                }
            }
            None => {
                tracing::debug!(code, "unknown key code, ignoring");
            }
        },
        InputMessage::ReleaseAll => {
            release_all(injector, pressed_keys, pressed_buttons);
        }
        InputMessage::ClipboardText { .. } => {
            // Never reaches here: `crate::signaling::handle_session_event`
            // intercepts `ClipboardText` on the `input` channel and applies
            // it directly (synchronously, ahead of whatever `Key` follows)
            // instead of handing it to the router. This arm exists only to
            // keep the match exhaustive over `InputMessage`.
        }
    }
}

/// Releases every key/button currently tracked as held, then clears the
/// tracking sets. Used for an explicit `ReleaseAll` from the client and when
/// the router itself shuts down (see `InputRouter`'s `Drop` impl).
fn release_all(
    injector: &mut dyn Injector,
    pressed_keys: &mut HashSet<u16>,
    pressed_buttons: &mut HashSet<PointerButton>,
) {
    for keycode in pressed_keys.drain() {
        injector.key(keycode, false);
    }
    for button in pressed_buttons.drain() {
        injector.button(button, false);
    }
}

/// If `first` is a `PointerMove`, drains every immediately-available
/// consecutive `PointerMove` off `rx` and returns the last one, applying only
/// that one to the injector -- an intermediate position is worthless once a
/// newer one is queued right behind it, and this keeps input latency from
/// growing if the router ever falls behind a burst of mouse movement. The
/// first non-`PointerMove` message encountered while draining is *not*
/// discarded: it's returned as `pending` for the caller to process on the
/// next iteration, since (unlike `PointerMove`) every other message matters.
fn coalesce_pointer_move(
    first: InputMessage,
    rx: &std_mpsc::Receiver<InputMessage>,
    pending: &mut Option<InputMessage>,
) -> InputMessage {
    let InputMessage::PointerMove { .. } = first else {
        return first;
    };
    let mut latest = first;
    loop {
        match rx.try_recv() {
            Ok(next @ InputMessage::PointerMove { .. }) => latest = next,
            Ok(other) => {
                *pending = Some(other);
                break;
            }
            Err(_) => break,
        }
    }
    latest
}

/// Owns the dedicated thread that turns queued `InputMessage`s into
/// `Injector` calls.
pub struct InputRouter {
    // `Option` so `Drop` can take it before joining the thread: dropping the
    // router's own sender is what makes the worker thread's `recv()` return
    // `Err` and fall through to a final `release_all` before exiting. This
    // assumes nobody keeps a `sender()` clone alive past the router itself
    // (the signaling layer only ever holds one transiently, for the
    // duration of a single `send` call -- see `crate::signaling`).
    sender: Option<std_mpsc::Sender<InputMessage>>,
    thread: Option<thread::JoinHandle<()>>,
    /// The currently captured display's rectangle, read fresh by
    /// `router_loop` on every message. Shared (rather than passed once at
    /// construction) so `set_capture_rect` can update it live when
    /// `crate::signaling::switch_display` changes which display is streamed,
    /// without restarting the router/injector thread.
    capture_rect: Arc<Mutex<CaptureRect>>,
}

impl InputRouter {
    pub fn new(injector: Box<dyn Injector>) -> InputRouter {
        let (w, h) = injector.screen_size();
        let capture_rect = Arc::new(Mutex::new(CaptureRect {
            x: 0,
            y: 0,
            width: w,
            height: h,
        }));
        let (tx, rx) = std_mpsc::channel::<InputMessage>();
        let thread_rect = Arc::clone(&capture_rect);
        let thread = thread::spawn(move || router_loop(injector, rx, thread_rect));
        InputRouter {
            sender: Some(tx),
            thread: Some(thread),
            capture_rect,
        }
    }

    /// Updates the rect normalized pointer coordinates are scaled against
    /// (see `CaptureRect`), taking effect for every message the router
    /// applies from this point on.
    pub fn set_capture_rect(&self, rect: CaptureRect) {
        *self
            .capture_rect
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = rect;
    }

    /// A sender for feeding messages to the router's worker thread. Cheap to
    /// call repeatedly -- cloning an `mpsc::Sender` is cheap -- so callers
    /// aren't expected to hold onto the result.
    pub fn sender(&self) -> std_mpsc::Sender<InputMessage> {
        self.sender
            .as_ref()
            .expect("sender is only taken in Drop, after which InputRouter is gone")
            .clone()
    }
}

impl Drop for InputRouter {
    fn drop(&mut self) {
        // Drop this sender first (before joining): with it gone -- assuming
        // no other clone outlives it, see the field comment above -- the
        // worker thread's blocking `recv()` returns `Err`, it releases
        // everything still held, and its loop exits.
        self.sender.take();
        if let Some(handle) = self.thread.take() {
            let _ = handle.join();
        }
    }
}

fn router_loop(
    mut injector: Box<dyn Injector>,
    rx: std_mpsc::Receiver<InputMessage>,
    capture_rect: Arc<Mutex<CaptureRect>>,
) {
    let mut pressed_keys: HashSet<u16> = HashSet::new();
    let mut pressed_buttons: HashSet<PointerButton> = HashSet::new();
    let mut pending: Option<InputMessage> = None;

    loop {
        let msg = match pending.take() {
            Some(msg) => msg,
            None => match rx.recv() {
                Ok(msg) => msg,
                Err(_) => break,
            },
        };
        let msg = coalesce_pointer_move(msg, &rx, &mut pending);
        // Locked synchronously and released immediately: cheap, and this
        // thread never awaits anything (see the module doc comment).
        let rect = *capture_rect
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        apply(
            &msg,
            injector.as_mut(),
            &mut pressed_keys,
            &mut pressed_buttons,
            rect,
        );
    }

    release_all(injector.as_mut(), &mut pressed_keys, &mut pressed_buttons);
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    #[derive(Debug, PartialEq)]
    enum Call {
        PointerMove(i32, i32),
        Button(PointerButton, bool),
        Wheel(i32, i32),
        Key(u16, bool),
    }

    struct FakeInjector {
        calls: Arc<Mutex<Vec<Call>>>,
        screen: (i32, i32),
        /// Held locked by a test to stall every call this injector makes
        /// until the test releases it -- see `coalesces_rapid_pointer_moves`.
        gate: Arc<Mutex<()>>,
    }

    /// `(injector, its call log, its stall gate)`, as built by
    /// `FakeInjector::new`.
    type FakeInjectorParts = (FakeInjector, Arc<Mutex<Vec<Call>>>, Arc<Mutex<()>>);

    impl FakeInjector {
        fn new(screen: (i32, i32)) -> FakeInjectorParts {
            let calls = Arc::new(Mutex::new(Vec::new()));
            let gate = Arc::new(Mutex::new(()));
            (
                Self {
                    calls: Arc::clone(&calls),
                    screen,
                    gate: Arc::clone(&gate),
                },
                calls,
                gate,
            )
        }
    }

    impl Injector for FakeInjector {
        fn pointer_move(&mut self, x: i32, y: i32) {
            let _held = self.gate.lock().unwrap();
            self.calls.lock().unwrap().push(Call::PointerMove(x, y));
        }
        fn button(&mut self, button: PointerButton, pressed: bool) {
            let _held = self.gate.lock().unwrap();
            self.calls
                .lock()
                .unwrap()
                .push(Call::Button(button, pressed));
        }
        fn wheel(&mut self, dx: i32, dy: i32) {
            let _held = self.gate.lock().unwrap();
            self.calls.lock().unwrap().push(Call::Wheel(dx, dy));
        }
        fn key(&mut self, keycode: u16, pressed: bool) {
            let _held = self.gate.lock().unwrap();
            self.calls.lock().unwrap().push(Call::Key(keycode, pressed));
        }
        fn screen_size(&self) -> (i32, i32) {
            self.screen
        }
    }

    /// Polls `calls` until it has at least `n` entries or a generous timeout
    /// elapses (the router thread applies messages asynchronously).
    fn wait_for_calls(calls: &Arc<Mutex<Vec<Call>>>, n: usize) {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        while calls.lock().unwrap().len() < n {
            assert!(
                std::time::Instant::now() < deadline,
                "timed out waiting for {n} call(s), got {:?}",
                calls.lock().unwrap()
            );
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
    }

    #[test]
    fn pointer_move_scales_normalized_coordinates_to_pixels() {
        let (injector, calls, _gate) = FakeInjector::new((1000, 500));
        let router = InputRouter::new(Box::new(injector));
        router
            .sender()
            .send(InputMessage::PointerMove { x: 0.5, y: 0.5 })
            .unwrap();

        wait_for_calls(&calls, 1);
        assert_eq!(calls.lock().unwrap()[0], Call::PointerMove(500, 250));
    }

    #[test]
    fn pointer_move_clamps_out_of_range_coordinates() {
        let (injector, calls, _gate) = FakeInjector::new((1000, 500));
        let router = InputRouter::new(Box::new(injector));
        router
            .sender()
            .send(InputMessage::PointerMove { x: 1.2, y: 0.0 })
            .unwrap();

        wait_for_calls(&calls, 1);
        assert_eq!(calls.lock().unwrap()[0], Call::PointerMove(999, 0));
    }

    #[test]
    fn pointer_move_uses_capture_rect_offset_and_size() {
        let (injector, calls, _gate) = FakeInjector::new((1000, 500));
        let router = InputRouter::new(Box::new(injector));
        router.set_capture_rect(CaptureRect {
            x: 1920,
            y: -100,
            width: 1000,
            height: 500,
        });
        router
            .sender()
            .send(InputMessage::PointerMove { x: 0.5, y: 0.5 })
            .unwrap();

        wait_for_calls(&calls, 1);
        assert_eq!(calls.lock().unwrap()[0], Call::PointerMove(2420, 150));
    }

    #[test]
    fn pointer_button_uses_capture_rect() {
        let (injector, calls, _gate) = FakeInjector::new((1000, 500));
        let router = InputRouter::new(Box::new(injector));
        router.set_capture_rect(CaptureRect {
            x: 1920,
            y: -100,
            width: 1000,
            height: 500,
        });
        router
            .sender()
            .send(InputMessage::PointerButton {
                button: PointerButton::Left,
                pressed: true,
                x: 1.0,
                y: 0.0,
            })
            .unwrap();

        wait_for_calls(&calls, 2);
        let calls = calls.lock().unwrap();
        assert_eq!(calls[0], Call::PointerMove(2919, -100));
        assert_eq!(calls[1], Call::Button(PointerButton::Left, true));
    }

    #[test]
    fn release_all_releases_exactly_the_pressed_keys_and_buttons() {
        let (injector, calls, _gate) = FakeInjector::new((1000, 500));
        let router = InputRouter::new(Box::new(injector));
        let sender = router.sender();

        // Use a pointer button rather than `Key` here: `keymap::to_keycode`
        // is empty on platforms without a real `Injector` backend (Linux
        // CI), so a button press exercises `release_all` the same way on
        // every platform this test runs on.
        sender
            .send(InputMessage::PointerButton {
                button: PointerButton::Left,
                pressed: true,
                x: 0.0,
                y: 0.0,
            })
            .unwrap();
        // pointer_move (from the button press) + button press
        wait_for_calls(&calls, 2);

        sender.send(InputMessage::ReleaseAll).unwrap();
        wait_for_calls(&calls, 3);

        let calls = calls.lock().unwrap();
        assert_eq!(
            calls.last(),
            Some(&Call::Button(PointerButton::Left, false))
        );
    }

    #[test]
    fn drop_releases_everything_still_held() {
        let (injector, calls, _gate) = FakeInjector::new((1000, 500));
        let router = InputRouter::new(Box::new(injector));
        router
            .sender()
            .send(InputMessage::PointerButton {
                button: PointerButton::Right,
                pressed: true,
                x: 0.0,
                y: 0.0,
            })
            .unwrap();
        wait_for_calls(&calls, 2); // pointer_move + button press

        drop(router);

        let calls = calls.lock().unwrap();
        assert_eq!(
            calls.last(),
            Some(&Call::Button(PointerButton::Right, false))
        );
    }

    #[test]
    fn coalesces_rapid_pointer_moves() {
        let (injector, calls, gate) = FakeInjector::new((1000, 500));
        // Hold the gate before the router thread can process anything, so
        // every message sent below lands in the channel before the first
        // `pointer_move` call is allowed to complete -- deterministically
        // exercising the coalescing path instead of racing it.
        let held = gate.lock().unwrap();
        let router = InputRouter::new(Box::new(injector));
        let sender = router.sender();
        for i in 0..50 {
            sender
                .send(InputMessage::PointerMove {
                    x: f64::from(i) / 49.0,
                    y: 0.5,
                })
                .unwrap();
        }
        drop(held);

        wait_for_calls(&calls, 1);
        // Give the router a moment to apply any further coalesced batch,
        // then assert it settled.
        std::thread::sleep(std::time::Duration::from_millis(100));

        let calls = calls.lock().unwrap();
        assert!(
            calls.len() <= 3,
            "expected coalescing to keep pointer_move calls low, got {}",
            calls.len()
        );
        // x = 49/49 = 1.0 -> scaled to the screen width (1000), then clamped
        // into [0, width-1].
        assert_eq!(calls.last(), Some(&Call::PointerMove(999, 250)));
    }

    #[cfg(any(target_os = "macos", target_os = "windows"))]
    #[test]
    fn key_message_uses_platform_keycode() {
        let (injector, calls, _gate) = FakeInjector::new((1000, 500));
        let router = InputRouter::new(Box::new(injector));
        router
            .sender()
            .send(InputMessage::Key {
                code: "KeyA".to_string(),
                pressed: true,
            })
            .unwrap();

        wait_for_calls(&calls, 1);
        let expected = keymap::to_keycode("KeyA").expect("KeyA must be mapped on this platform");
        assert_eq!(calls.lock().unwrap()[0], Call::Key(expected, true));
    }
}
