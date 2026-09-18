pub(super) fn name() -> &'static str {
    "macos"
}

/// The Mac's user-visible computer name (System Settings -> General ->
/// Sharing -> Local hostname / Computer Name), read via
/// `NSHost.currentHost().localizedName()`.
///
/// Apple deprecated the whole `NSHost` class in favor of the Network
/// framework's connect-by-name APIs (see its class doc comment in
/// `objc2-foundation`) -- but the Network framework has no replacement for
/// "what is this computer's display name", only DNS-style resolution, so
/// `NSHost` remains the only way to read it. Hence `#[allow(deprecated)]`
/// below, the same situation as `cursor::macos`'s `currentSystemCursor`.
#[allow(deprecated)]
pub(super) fn computer_name() -> Option<String> {
    let name = objc2_foundation::NSHost::currentHost()
        .localizedName()?
        .to_string();
    if name.is_empty() {
        None
    } else {
        Some(name)
    }
}
