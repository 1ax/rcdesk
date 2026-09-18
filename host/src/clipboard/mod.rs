//! Clipboard text sync (slice 2.5b): the host watches its own clipboard on
//! a dedicated thread -- like `crate::cursor`'s `CursorSource`/`watch`, so a
//! blocking platform call never stalls the tokio runtime -- and reports
//! every change to its owner (`crate::signaling`), which sends it down the
//! `control` data channel as `proto::control::ControlMessage::ClipboardText`.
//! The reverse direction (client -> host, `proto::input::InputMessage::ClipboardText`
//! on the `input` channel) is applied directly by `crate::signaling::handle_session_event`,
//! not through this module's watcher.
//!
//! Privacy: clipboard *content* must never reach the logs, at any level,
//! including `Debug` output of the wire messages -- only lengths are ever
//! logged. See `crate::signaling` for how `ControlMessage`/`InputMessage`
//! logging sites are guarded against printing a `ClipboardText` payload.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use tokio::sync::mpsc;

#[cfg(target_os = "macos")]
pub mod macos;
#[cfg(target_os = "windows")]
pub mod windows;

/// Clipboard text longer than this (UTF-8 bytes) is never synchronized in
/// either direction -- dropped with a `warn` giving only the length.
pub const MAX_CLIPBOARD_BYTES: usize = 200_000;

/// How often `watch` polls the OS change counter. Cheap (a single integer
/// read), so 250ms is frequent enough to feel instant without wasting CPU.
pub const CLIPBOARD_POLL_INTERVAL: Duration = Duration::from_millis(250);

/// Something that can read/write the host OS's clipboard *text* and report
/// whether it changed, via a cheap OS-provided change counter (`NSPasteboard`
/// `changeCount` on macOS, `GetClipboardSequenceNumber` on Windows) rather
/// than reading the clipboard itself on every poll.
pub trait ClipboardBackend: Send {
    /// The OS's current clipboard change counter. Monotonically increasing
    /// in practice, but callers should only ever compare it for equality
    /// (see `ClipboardSync::poll`), never assume a fixed step size.
    fn change_count(&mut self) -> u64;
    /// Reads the current clipboard text. `None` means there is no text on
    /// the clipboard right now (e.g. an image was copied instead, or it's
    /// empty) -- a normal, expected outcome, not logged here. A genuine
    /// platform failure is logged (`warn`, without any clipboard content) by
    /// the implementation itself before returning `None`.
    fn get_text(&mut self) -> Option<String>;
    /// Writes `text` to the clipboard.
    fn set_text(&mut self, text: &str) -> anyhow::Result<()>;
}

/// Pure change-detection/dedup logic over a `ClipboardBackend`, with no
/// threads of its own -- `watch` below drives it from a dedicated thread the
/// same way `crate::cursor::watch` drives a `CursorSource`.
pub struct ClipboardSync {
    backend: Box<dyn ClipboardBackend>,
    /// The change counter as of the last time it was observed (construction,
    /// or the last `poll`/`apply_remote`). Never `None` -- the base is taken
    /// at construction precisely so the clipboard's *starting* content is
    /// never treated as a "change" (see `new`).
    last_count: u64,
    /// The last text this session either sent (`poll`) or applied
    /// (`apply_remote`), used to dedup identical text arriving again from
    /// either direction.
    last_text: Option<String>,
}

impl ClipboardSync {
    /// Wraps `backend`. Reads only the current change counter, not the
    /// clipboard's text -- so whatever is already on the clipboard when a
    /// session starts is never sent to the client; only a *subsequent*
    /// change is.
    pub fn new(mut backend: Box<dyn ClipboardBackend>) -> Self {
        let last_count = backend.change_count();
        ClipboardSync {
            backend,
            last_count,
            last_text: None,
        }
    }

    /// Checks whether the clipboard changed since the last `poll`/`new`, and
    /// if so, whether that change is worth sending: `None` in every case
    /// that isn't a new, non-empty, size-limited, actually-different piece
    /// of text (unchanged counter, read failure, empty, over
    /// `MAX_CLIPBOARD_BYTES`, or identical to what was last seen).
    pub fn poll(&mut self) -> Option<String> {
        let count = self.backend.change_count();
        if count == self.last_count {
            return None;
        }
        self.last_count = count;

        let text = self.backend.get_text()?;
        if text.is_empty() || Some(&text) == self.last_text.as_ref() {
            return None;
        }
        if text.len() > MAX_CLIPBOARD_BYTES {
            tracing::warn!(
                len = text.len(),
                "clipboard text too large to sync, skipping"
            );
            return None;
        }

        self.last_text = Some(text.clone());
        Some(text)
    }

    /// Applies clipboard text that arrived from the remote side (client ->
    /// host) to this host's clipboard.
    pub fn apply_remote(&mut self, text: &str) {
        if text.len() > MAX_CLIPBOARD_BYTES {
            tracing::warn!(
                len = text.len(),
                "incoming clipboard text too large to apply, skipping"
            );
            return;
        }
        if Some(text) == self.last_text.as_deref() {
            return;
        }
        if let Err(err) = self.backend.set_text(text) {
            // Not recorded as `last_text`: the client resending the same
            // text (e.g. on its next Cmd+V) must get another try.
            tracing::warn!(error = %err, "failed to write clipboard text");
            return;
        }
        self.last_text = Some(text.to_string());
        // Re-read the counter *after* writing so this session's own write
        // doesn't come back around as a "remote change" on the next `poll`
        // (an echo) -- writing the clipboard bumps the OS's change counter
        // too, same as any other write to it.
        self.last_count = self.backend.change_count();
    }
}

/// Owns the dedicated thread started by `watch`. Dropping it stops the
/// thread (mirrors `crate::cursor::CursorWatcher`).
pub struct ClipboardWatcher {
    /// Every text change `ClipboardSync::poll` reported, most recent last.
    pub rx: mpsc::Receiver<String>,
    stop: Arc<AtomicBool>,
    thread: Option<thread::JoinHandle<()>>,
}

impl Drop for ClipboardWatcher {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(handle) = self.thread.take() {
            let _ = handle.join();
        }
    }
}

/// Starts a dedicated thread that calls `sync.poll()` every `period` and
/// pushes every `Some(text)` result onto the returned watcher's `rx`.
///
/// The channel has capacity 4 and uses `try_send`, the same best-effort
/// reasoning as `crate::cursor::watch`: if a consumer falls behind, only the
/// most recent unsent text actually matters, and `ClipboardSync` itself
/// already dedups, so a dropped send is harmless.
pub fn watch(sync: Arc<Mutex<ClipboardSync>>, period: Duration) -> ClipboardWatcher {
    let (tx, rx) = mpsc::channel(4);
    let stop = Arc::new(AtomicBool::new(false));
    let thread_stop = Arc::clone(&stop);
    let thread = thread::spawn(move || watch_loop(sync, period, &tx, &thread_stop));
    ClipboardWatcher {
        rx,
        stop,
        thread: Some(thread),
    }
}

fn watch_loop(
    sync: Arc<Mutex<ClipboardSync>>,
    period: Duration,
    tx: &mpsc::Sender<String>,
    stop: &AtomicBool,
) {
    while !stop.load(Ordering::Relaxed) {
        let changed = {
            let mut sync = sync.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
            sync.poll()
        };
        if let Some(text) = changed {
            // Best-effort: see `watch`'s doc comment.
            let _ = tx.try_send(text);
        }
        thread::sleep(period);
    }
}

/// A `ClipboardBackend` used only in tests: a fake clipboard whose change
/// counter and text are driven explicitly by the test (`set_remote_change`),
/// with an observable log of `set_text` calls -- the same shape as
/// `crate::input`'s `FakeInjector`. Exposed here (not nested in a `tests`
/// submodule) so `crate::signaling`'s tests can drive a `ClipboardSync`
/// end-to-end through `HostContext::build_clipboard` too.
#[cfg(test)]
#[derive(Clone, Default)]
pub(crate) struct FakeClipboardBackend {
    state: Arc<std::sync::Mutex<FakeClipboardState>>,
}

#[cfg(test)]
#[derive(Default)]
struct FakeClipboardState {
    change_count: u64,
    text: Option<String>,
    set_calls: Vec<String>,
}

#[cfg(test)]
impl FakeClipboardBackend {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// Simulates the OS-side clipboard changing (as if another application
    /// copied something): bumps the change counter and sets the text a
    /// following `get_text` will return. `None` simulates a non-text copy.
    pub(crate) fn set_remote_change(&self, text: Option<&str>) {
        let mut state = self.state.lock().unwrap();
        state.change_count += 1;
        state.text = text.map(str::to_string);
    }

    /// Every `text` ever passed to `set_text`, in call order.
    pub(crate) fn set_calls(&self) -> Vec<String> {
        self.state.lock().unwrap().set_calls.clone()
    }
}

#[cfg(test)]
impl ClipboardBackend for FakeClipboardBackend {
    fn change_count(&mut self) -> u64 {
        self.state.lock().unwrap().change_count
    }

    fn get_text(&mut self) -> Option<String> {
        self.state.lock().unwrap().text.clone()
    }

    fn set_text(&mut self, text: &str) -> anyhow::Result<()> {
        let mut state = self.state.lock().unwrap();
        state.set_calls.push(text.to_string());
        state.text = Some(text.to_string());
        // Real OSes bump their change counter on any write, including this
        // session's own -- `ClipboardSync::apply_remote` relies on exactly
        // this to avoid echoing its own write back as an incoming change.
        state.change_count += 1;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sync_with(backend: FakeClipboardBackend) -> ClipboardSync {
        ClipboardSync::new(Box::new(backend))
    }

    #[test]
    fn starting_clipboard_content_is_never_sent() {
        let backend = FakeClipboardBackend::new();
        backend.set_remote_change(Some("already there before the session started"));
        // `new` reads only the counter that already reflects this change --
        // constructed *after* the change, exactly like a session starting
        // with something already on the clipboard.
        let mut sync = sync_with(backend);

        assert_eq!(sync.poll(), None);
    }

    #[test]
    fn unchanged_counter_returns_none() {
        let backend = FakeClipboardBackend::new();
        let mut sync = sync_with(backend);

        assert_eq!(sync.poll(), None);
        assert_eq!(sync.poll(), None);
    }

    #[test]
    fn a_change_is_reported_exactly_once() {
        let backend = FakeClipboardBackend::new();
        let mut sync = sync_with(backend.clone());
        // The change happens *after* construction -- see
        // `starting_clipboard_content_is_never_sent` for the "before" case.
        backend.set_remote_change(Some("hello"));

        assert_eq!(sync.poll(), Some("hello".to_string()));
        assert_eq!(sync.poll(), None);
    }

    #[test]
    fn identical_text_is_not_reported_again() {
        let backend = FakeClipboardBackend::new();
        let mut sync = sync_with(backend.clone());
        backend.set_remote_change(Some("same"));
        assert_eq!(sync.poll(), Some("same".to_string()));

        // Counter changes again (e.g. re-copying the same selection) but the
        // text is identical to what was already sent.
        backend.set_remote_change(Some("same"));
        assert_eq!(sync.poll(), None);
    }

    #[test]
    fn oversized_text_is_skipped_on_poll() {
        let backend = FakeClipboardBackend::new();
        let mut sync = sync_with(backend.clone());
        let huge = "x".repeat(MAX_CLIPBOARD_BYTES + 1);
        backend.set_remote_change(Some(&huge));

        assert_eq!(sync.poll(), None);
    }

    #[test]
    fn non_text_clipboard_content_returns_none() {
        let backend = FakeClipboardBackend::new();
        let mut sync = sync_with(backend.clone());
        backend.set_remote_change(None);

        assert_eq!(sync.poll(), None);
    }

    #[test]
    fn apply_remote_writes_through_to_the_backend() {
        let backend = FakeClipboardBackend::new();
        let calls_handle = backend.clone();
        let mut sync = sync_with(backend);

        sync.apply_remote("from the client");

        assert_eq!(
            calls_handle.set_calls(),
            vec!["from the client".to_string()]
        );
    }

    #[test]
    fn apply_remote_does_not_echo_back_through_poll() {
        let backend = FakeClipboardBackend::new();
        let mut sync = sync_with(backend);

        sync.apply_remote("from the client");

        // The backend's `set_text` bumped its own change counter (as a real
        // OS clipboard would); `apply_remote` must have re-synced against
        // that so this doesn't look like an incoming remote change.
        assert_eq!(sync.poll(), None);
    }

    #[test]
    fn apply_remote_skips_identical_text() {
        let backend = FakeClipboardBackend::new();
        let calls_handle = backend.clone();
        let mut sync = sync_with(backend);

        sync.apply_remote("same text");
        sync.apply_remote("same text");

        assert_eq!(calls_handle.set_calls(), vec!["same text".to_string()]);
    }

    #[test]
    fn apply_remote_skips_oversized_text() {
        let backend = FakeClipboardBackend::new();
        let calls_handle = backend.clone();
        let mut sync = sync_with(backend);
        let huge = "x".repeat(MAX_CLIPBOARD_BYTES + 1);

        sync.apply_remote(&huge);

        assert_eq!(calls_handle.set_calls(), Vec::<String>::new());
    }
}
