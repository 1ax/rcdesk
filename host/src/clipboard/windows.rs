//! Windows clipboard backend: text via `arboard::Clipboard`, change
//! detection via `GetClipboardSequenceNumber` -- a cheap Win32 query, bumped
//! by the OS on every clipboard write by any application (including our own
//! `set_text` calls, see `ClipboardSync::apply_remote`).
//!
//! Compiles only on `windows-latest` in CI: there is no Windows machine
//! available to this executor run, so `GetClipboardSequenceNumber`'s
//! signature below comes from reading the `windows` 0.61.3 source fetched
//! into the local registry cache, not from memory -- see executor rule 3.

use windows::Win32::System::DataExchange::GetClipboardSequenceNumber;

use super::ClipboardBackend;

pub struct WinClipboardBackend {
    // `arboard::Clipboard`'s Windows backend is a zero-sized `Clipboard(())`
    // (see arboard 3.6.1 `src/platform/windows.rs`), trivially `Send`, so a
    // single instance can be held here and reused across calls.
    clipboard: arboard::Clipboard,
}

impl WinClipboardBackend {
    pub fn new() -> anyhow::Result<Self> {
        Ok(Self {
            clipboard: arboard::Clipboard::new()?,
        })
    }
}

impl ClipboardBackend for WinClipboardBackend {
    fn change_count(&mut self) -> u64 {
        // Safety: no preconditions -- a plain Win32 query with no
        // out-parameters to validate.
        u64::from(unsafe { GetClipboardSequenceNumber() })
    }

    fn get_text(&mut self) -> Option<String> {
        match self.clipboard.get_text() {
            Ok(text) => Some(text),
            // Genuinely "nothing to sync" (empty clipboard, or non-text
            // content like an image) -- not a failure, not logged.
            Err(arboard::Error::ContentNotAvailable) => None,
            Err(err) => {
                tracing::warn!(error = %err, "failed to read clipboard text");
                None
            }
        }
    }

    fn set_text(&mut self, text: &str) -> anyhow::Result<()> {
        self.clipboard.set_text(text)?;
        Ok(())
    }
}
