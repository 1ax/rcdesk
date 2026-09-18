//! macOS clipboard backend: text via `arboard::Clipboard`, change detection
//! via `NSPasteboard.generalPasteboard().changeCount()` -- a cheap integer
//! read, bumped by the OS on every write to the general pasteboard by any
//! application (including our own `set_text` calls, see
//! `ClipboardSync::apply_remote`).

use objc2_app_kit::NSPasteboard;

use super::ClipboardBackend;

pub struct MacClipboardBackend {
    // `arboard::Clipboard`'s macOS backend is `unsafe impl Send + Sync`
    // (see arboard 3.6.1 `src/platform/osx.rs`), so a single instance can be
    // held here and reused across calls rather than rebuilt every time.
    clipboard: arboard::Clipboard,
}

impl MacClipboardBackend {
    pub fn new() -> anyhow::Result<Self> {
        Ok(Self {
            clipboard: arboard::Clipboard::new()?,
        })
    }
}

impl ClipboardBackend for MacClipboardBackend {
    fn change_count(&mut self) -> u64 {
        let pasteboard = NSPasteboard::generalPasteboard();
        pasteboard.changeCount() as u64
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

#[cfg(test)]
mod tests {
    use super::*;

    /// Manual check, not run in CI: reads the real clipboard change count
    /// twice (with a prompt to copy something in between). Requires no TCC
    /// permission -- clipboard access is not gated on macOS.
    ///
    /// Run with:
    /// `cargo test -p rcdesk-host prints_clipboard_change_count -- --ignored --nocapture`
    #[test]
    #[cfg(target_os = "macos")]
    #[ignore]
    fn prints_clipboard_change_count() {
        let mut backend = MacClipboardBackend::new().expect("clipboard available");
        println!("change_count = {}", backend.change_count());
    }
}
