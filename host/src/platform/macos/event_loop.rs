//! `rcdesk-agent`'s own minimal main-thread event loop (slice 2.6c). No
//! winit/tao (see `host/Cargo.toml`'s `tray-icon` entry for why): the main
//! thread just parks in `pump` between polls, driving whatever AppKit needs
//! (menu clicks, the tray icon itself) without owning the whole run loop the
//! way `NSApplication::run()` would -- `agent_main` also has to poll status/
//! permission changes and `MenuEvent`/`TrayIconEvent` on the same thread.
//!
//! `tray-icon` requires the event loop to already be pumping before a
//! `TrayIcon` is built (see its crate-level doc comment: "You must make sure
//! that the event loop is already running ... before creating a TrayIcon"),
//! so `agent_main` calls `init()` then `pump()` at least once before
//! building the tray icon.

use std::time::Duration;

use objc2::MainThreadMarker;
use objc2_app_kit::{NSApplication, NSApplicationActivationPolicy, NSEventMask};
use objc2_foundation::{NSDate, NSDefaultRunLoopMode};

/// Brings up `NSApplication` on the calling thread and hides it from the
/// Dock/Cmd+Tab app switcher (`Accessory`): the tray agent is a background
/// utility with no windows of its own. Must be called once, from the main
/// thread, before the first `pump`.
pub fn init() {
    let mtm = MainThreadMarker::new()
        .expect("platform::macos::event_loop::init must run on the main thread");
    let app = NSApplication::sharedApplication(mtm);
    app.setActivationPolicy(NSApplicationActivationPolicy::Accessory);
    app.finishLaunching();
}

/// Drains every AppKit event currently queued, waiting up to `timeout` for
/// the first one to arrive (so a caller with nothing else to do doesn't
/// busy-loop) -- the polling half of what `NSApplication::run()` does on its
/// own, since `rcdesk-agent` also needs to check tray/menu events and agent
/// status between iterations instead of blocking in `run()` forever. Must be
/// called from the same thread `init` ran on.
pub fn pump(timeout: Duration) {
    let mtm = MainThreadMarker::new()
        .expect("platform::macos::event_loop::pump must run on the main thread");
    let app = NSApplication::sharedApplication(mtm);
    let deadline = NSDate::dateWithTimeIntervalSinceNow(timeout.as_secs_f64());
    // Safety: `NSDefaultRunLoopMode` is a constant `NSString *` exported by
    // AppKit, valid for the whole process lifetime -- reading the `extern
    // static` merely dereferences that fixed, always-valid pointer.
    let mode = unsafe { NSDefaultRunLoopMode };

    loop {
        let event = app.nextEventMatchingMask_untilDate_inMode_dequeue(
            NSEventMask::Any,
            Some(&deadline),
            mode,
            true,
        );
        match event {
            Some(event) => app.sendEvent(&event),
            // No more events queued before `deadline` -- done for this pump.
            None => break,
        }
    }
}
