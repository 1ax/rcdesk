//! Keeps `rcdesk-agent` from being App Nap-throttled while a remote session
//! is active (slice 2.6c): the tray agent has no windows and no direct user
//! interaction -- exactly what App Nap targets -- so without this macOS may
//! coalesce/delay the capture and encode threads' timers, degrading fps
//! during an otherwise idle-looking background process.

use objc2::rc::Retained;
use objc2::runtime::{NSObjectProtocol, ProtocolObject};
use objc2_foundation::{NSActivityOptions, NSProcessInfo, NSString};

/// A live "stay awake" activity, begun by `begin` and ended when dropped.
/// `agent_main` holds one for as long as `AgentStatus::InSession` is
/// current, same lifetime as `SessionActivity` in Windows'
/// `platform::windows::power`.
pub struct SessionActivity(Retained<ProtocolObject<dyn NSObjectProtocol>>);

impl SessionActivity {
    /// Begins the activity. Call once when a session becomes active; drop
    /// the result when it ends.
    pub fn begin() -> Self {
        let info = NSProcessInfo::processInfo();
        let reason = NSString::from_str("rcdesk remote session");
        let token = info.beginActivityWithOptions_reason(
            NSActivityOptions::UserInitiated | NSActivityOptions::IdleSystemSleepDisabled,
            &reason,
        );
        Self(token)
    }
}

impl Drop for SessionActivity {
    fn drop(&mut self) {
        let info = NSProcessInfo::processInfo();
        // Safety: `self.0` was returned by the matching
        // `beginActivityWithOptions_reason` call in `begin` and is only ever
        // passed to `endActivity` here, once (`Drop` runs at most once).
        unsafe { info.endActivity(&self.0) };
    }
}
