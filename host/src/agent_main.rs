#![cfg_attr(target_os = "windows", windows_subsystem = "windows")]
//! `rcdesk-agent`: the host as a background tray/menu-bar application
//! (slice 2.6c) -- macOS menu-bar item / Windows tray icon, on top of the
//! exact same `crate::app::run_agent` pipeline `rcdesk-host serve` drives.
//! `rcdesk-host` itself is unchanged: this is a second, independent binary.
//!
//! No `#[tokio::main]`: the main thread is needed for the platform UI event
//! loop (`platform::event_loop::pump`, AppKit/Win32 messages, and the
//! `tray-icon`/`muda` menu machinery, all of which are main-thread-only) --
//! see `host/Cargo.toml`'s `tray-icon` entry for why there's no winit/tao
//! doing this instead. A `tokio::runtime::Runtime` is built explicitly and
//! `app::run_agent` is spawned onto it; the main thread drives the UI loop
//! and talks to it only through a `watch::Receiver<AgentStatus>` and an
//! `mpsc::UnboundedSender<AgentCommand>`, both plain synchronous types.
//!
//! See `docs/dev-run.md`'s "Агент в трее" section for how to run this and
//! where its log/lock files live (`agent::paths`).

#[cfg(not(any(target_os = "macos", target_os = "windows")))]
fn main() {
    eprintln!("rcdesk-agent supports macOS and Windows only");
    std::process::exit(1);
}

#[cfg(any(target_os = "macos", target_os = "windows"))]
fn main() {
    imp::run();
}

#[cfg(any(target_os = "macos", target_os = "windows"))]
mod imp {
    use std::path::Path;
    use std::sync::Arc;
    use std::time::{Duration, Instant};

    use clap::Parser;
    use tokio::sync::{mpsc, watch};
    use tray_icon::menu::{CheckMenuItem, Menu, MenuEvent, MenuItem, PredefinedMenuItem};
    use tray_icon::{Icon, TrayIconBuilder};

    use rcdesk_host::agent::{self, menu_model, probe_permissions, render_icon, IconState};
    use rcdesk_host::app::{self, CaptureBackend, EncoderBackend, ReconnectPolicy, ServeOptions};
    use rcdesk_host::platform;
    use rcdesk_host::signaling::{AgentCommand, AgentStatus, Keepalive};

    /// Menu-bar/tray icon size in pixels. macOS: tray-icon draws the image
    /// 18pt tall, so 44px stays sharp on Retina (2x). Windows: the shell
    /// scales one 32px image down to 16/20/24px by DPI.
    #[cfg(target_os = "macos")]
    const ICON_SIZE: u32 = 44;
    #[cfg(target_os = "windows")]
    const ICON_SIZE: u32 = 32;

    /// How often the main-thread loop pumps the platform event loop between
    /// checks of agent status/menu events -- see `platform::event_loop::pump`.
    const PUMP_INTERVAL: Duration = Duration::from_millis(100);

    /// How often permissions are re-probed (slice 2.6c plan): once at
    /// startup (`Instant::now()` below, i.e. immediately on the first loop
    /// iteration) and then on this interval.
    const PERMISSIONS_POLL_INTERVAL: Duration = Duration::from_secs(10);

    #[derive(Parser)]
    #[command(name = "rcdesk-agent", version, about = "rcdesk tray agent")]
    struct Cli {
        /// Signaling server WebSocket URL.
        #[arg(long, default_value = "wss://rcdesk.app/ws")]
        server: String,
        /// Host name shown to clients. Defaults the same way `rcdesk-host
        /// serve` does -- see `app::resolve_host_name`.
        #[arg(long)]
        name: Option<String>,
    }

    pub fn run() {
        let cli = Cli::parse();

        let log_dir = agent::paths::log_dir();
        let log_path = match agent::logging::init(&log_dir) {
            Ok(path) => path,
            Err(err) => {
                eprintln!(
                    "rcdesk-agent: failed to set up logging in {}: {err}",
                    log_dir.display()
                );
                std::process::exit(1);
            }
        };

        let data_dir = agent::paths::data_dir();
        let _lock = match agent::lock::acquire(&data_dir) {
            Ok(Some(lock)) => lock,
            Ok(None) => {
                tracing::info!("another rcdesk-agent is already running, exiting");
                std::process::exit(0);
            }
            Err(err) => {
                tracing::error!(error = %err, "failed to acquire the single-instance lock");
                std::process::exit(1);
            }
        };

        tracing::info!(
            version = env!("CARGO_PKG_VERSION"),
            platform = platform::name(),
            log_path = %log_path.display(),
            "rcdesk-agent starting"
        );

        // Must happen before any GDI/enigo/GetSystemMetrics call, same as
        // `rcdesk-host`'s CLI -- see `platform::windows::dpi` for why.
        #[cfg(target_os = "windows")]
        platform::windows::dpi::set_dpi_aware();

        let runtime = match tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
        {
            Ok(runtime) => runtime,
            Err(err) => {
                tracing::error!(error = %err, "failed to build the tokio runtime");
                std::process::exit(1);
            }
        };

        let opts = ServeOptions {
            server: cli.server.clone(),
            name: cli.name.clone(),
            synthetic: false,
            display: None,
            fps: 30,
            bitrate: 6000,
            max_qp: None,
            stun: vec![],
            no_input: false,
            no_clipboard: false,
            no_adapt: false,
            capture: CaptureBackend::Auto,
            encoder: EncoderBackend::Auto,
        };
        let host_name = app::resolve_host_name(cli.name.as_deref());
        let ctx = match app::build_host_context(&opts) {
            Ok(ctx) => Arc::new(ctx),
            Err(err) => {
                tracing::error!(error = %err, "failed to build the host context");
                std::process::exit(1);
            }
        };

        let (status_tx, mut status_rx) = watch::channel(AgentStatus::Connecting);
        let (cmd_tx, cmd_rx) = mpsc::unbounded_channel::<AgentCommand>();

        let agent_ctx = Arc::clone(&ctx);
        let server = cli.server.clone();
        runtime.spawn(async move {
            let _ = app::run_agent(
                &agent_ctx,
                &server,
                &host_name,
                ReconnectPolicy::default(),
                Keepalive::default(),
                status_tx,
                cmd_rx,
            )
            .await;
        });

        // macOS only: bring up `NSApplication` on this (main) thread before
        // the first pump -- `tray-icon` requires the event loop already
        // running before a `TrayIcon` is built. Windows' `tray-icon` backend
        // needs no separate init step.
        #[cfg(target_os = "macos")]
        platform::event_loop::init();
        platform::event_loop::pump(Duration::ZERO);

        let mut perms = probe_permissions();
        let mut last_status = status_rx.borrow_and_update().clone();
        let mut model = menu_model(&last_status, &perms);

        let status_item = MenuItem::with_id("status", &model.status_text, false, None);
        #[cfg(target_os = "macos")]
        let screen_item = MenuItem::with_id(
            "open_screen_prefs",
            screen_item_label(perms.screen),
            !perms.screen,
            None,
        );
        #[cfg(target_os = "macos")]
        let input_item = MenuItem::with_id(
            "open_accessibility_prefs",
            input_item_label(perms.input),
            !perms.input,
            None,
        );
        let copy_pin_item = MenuItem::with_id("copy_pin", "Copy PIN", model.pin.is_some(), None);
        let end_session_item =
            MenuItem::with_id("end_session", "End session", model.can_end_session, None);
        let open_log_item = MenuItem::with_id("open_log", "Open log", true, None);
        // Initial checked state: best-effort -- if `agent_path`/`is_enabled`
        // fails (should only happen if `current_exe()` itself fails), show
        // unchecked rather than block the menu on it; a click still retries
        // both.
        let autostart_enabled = agent::autostart::agent_path()
            .ok()
            .and_then(|path| platform::autostart::is_enabled(&path).ok())
            .unwrap_or(false);
        let autostart_item =
            CheckMenuItem::with_id("autostart", "Start at login", true, autostart_enabled, None);
        let quit_item = MenuItem::with_id("quit", "Quit rcdesk", true, None);

        let menu = Menu::new();
        menu.append(&status_item).expect("append status menu item");
        #[cfg(target_os = "macos")]
        {
            menu.append(&screen_item)
                .expect("append screen permission item");
            menu.append(&input_item)
                .expect("append accessibility permission item");
        }
        menu.append(&PredefinedMenuItem::separator())
            .expect("append separator");
        menu.append(&copy_pin_item).expect("append copy pin item");
        menu.append(&end_session_item)
            .expect("append end session item");
        menu.append(&PredefinedMenuItem::separator())
            .expect("append separator");
        menu.append(&open_log_item).expect("append open log item");
        menu.append(&autostart_item)
            .expect("append start-at-login item");
        menu.append(&PredefinedMenuItem::separator())
            .expect("append separator");
        menu.append(&quit_item).expect("append quit item");

        let is_macos = cfg!(target_os = "macos");
        let icon = Icon::from_rgba(render_icon(model.icon, ICON_SIZE), ICON_SIZE, ICON_SIZE)
            .expect("render_icon always produces a valid-sized RGBA buffer");
        let tray_icon = TrayIconBuilder::new()
            .with_menu(Box::new(menu))
            .with_tooltip(&model.tooltip)
            .with_icon(icon)
            .with_icon_as_template(is_macos)
            .build()
            .expect("failed to build the tray icon");

        // Holds the "stay awake" activity for as long as a session is
        // active on macOS (see `platform::activity`'s doc comment); the
        // Windows equivalent (`platform::windows::power`) is a pair of
        // plain function calls with no guard object to hold. Never read --
        // its whole purpose is the side effect of *when* it's dropped
        // (`endActivity`), which neither `unused_variables` nor
        // `unused_assignments` can see.
        #[cfg(target_os = "macos")]
        #[allow(unused_variables, unused_assignments)]
        let mut session_activity: Option<platform::activity::SessionActivity> = None;

        let mut last_perms_check = Instant::now();

        loop {
            platform::event_loop::pump(PUMP_INTERVAL);

            while let Ok(event) = MenuEvent::receiver().try_recv() {
                let id = event.id().0.as_str();
                match id {
                    "copy_pin" => {
                        if let Some(pin) = &model.pin {
                            match arboard::Clipboard::new() {
                                Ok(mut clipboard) => {
                                    if let Err(err) = clipboard.set_text(pin.clone()) {
                                        tracing::warn!(error = %err, "failed to copy PIN to clipboard");
                                    }
                                }
                                Err(err) => {
                                    tracing::warn!(error = %err, "failed to open clipboard");
                                }
                            }
                        }
                    }
                    "end_session" => {
                        let _ = cmd_tx.send(AgentCommand::EndSession);
                    }
                    "open_log" => open_log(&log_path),
                    "autostart" => toggle_autostart(&autostart_item),
                    #[cfg(target_os = "macos")]
                    "open_screen_prefs" => open_system_settings_pane(
                        "x-apple.systempreferences:com.apple.preference.security?Privacy_ScreenCapture",
                    ),
                    #[cfg(target_os = "macos")]
                    "open_accessibility_prefs" => open_system_settings_pane(
                        "x-apple.systempreferences:com.apple.preference.security?Privacy_Accessibility",
                    ),
                    "quit" => quit(cmd_tx, status_rx, runtime),
                    _ => {}
                }
            }

            let status = status_rx.borrow_and_update().clone();
            let mut perms_changed = false;
            if last_perms_check.elapsed() >= PERMISSIONS_POLL_INTERVAL {
                last_perms_check = Instant::now();
                let new_perms = probe_permissions();
                if new_perms != perms {
                    perms = new_perms;
                    perms_changed = true;
                }
            }

            if status != last_status || perms_changed {
                let new_model = menu_model(&status, &perms);

                if new_model.status_text != model.status_text {
                    status_item.set_text(&new_model.status_text);
                }
                if new_model.tooltip != model.tooltip {
                    let _ = tray_icon.set_tooltip(Some(&new_model.tooltip));
                }
                copy_pin_item.set_enabled(new_model.pin.is_some());
                end_session_item.set_enabled(new_model.can_end_session);

                #[cfg(target_os = "macos")]
                {
                    screen_item.set_text(screen_item_label(perms.screen));
                    screen_item.set_enabled(!perms.screen);
                    input_item.set_text(input_item_label(perms.input));
                    input_item.set_enabled(!perms.input);
                }

                if new_model.icon != model.icon {
                    if let Ok(icon) = Icon::from_rgba(
                        render_icon(new_model.icon, ICON_SIZE),
                        ICON_SIZE,
                        ICON_SIZE,
                    ) {
                        let _ = tray_icon.set_icon_with_as_template(Some(icon), is_macos);
                    }
                }

                // Keep the system/display awake for as long as a session is
                // active -- see `platform::activity`/`platform::windows::power`'s
                // doc comments for why (App Nap / system sleep would
                // otherwise starve capture/encode timers in this
                // window-less background process).
                let now_in_session = matches!(status, AgentStatus::InSession { .. });
                let was_in_session = model.icon == IconState::Session;
                if now_in_session != was_in_session {
                    // Only the *drop* of the previous value (ending the
                    // prior activity, if any) and the timing of the new
                    // one's creation matter here -- `session_activity`
                    // itself is otherwise never read, hence the allow.
                    #[cfg(target_os = "macos")]
                    #[allow(unused_assignments)]
                    {
                        session_activity = if now_in_session {
                            Some(platform::activity::SessionActivity::begin())
                        } else {
                            None
                        };
                    }
                    #[cfg(target_os = "windows")]
                    {
                        if now_in_session {
                            platform::windows::power::begin_session_activity();
                        } else {
                            platform::windows::power::end_session_activity();
                        }
                    }
                }

                model = new_model;
                last_status = status;
            }
        }
    }

    /// Opens the log file for viewing: `open` on macOS (launches Console.app
    /// or whatever handles `.log` files), `notepad.exe` on Windows.
    fn open_log(path: &Path) {
        #[cfg(target_os = "macos")]
        {
            if let Err(err) = std::process::Command::new("open").arg(path).spawn() {
                tracing::warn!(error = %err, "failed to open the log file");
            }
        }
        #[cfg(target_os = "windows")]
        {
            if let Err(err) = std::process::Command::new("notepad.exe").arg(path).spawn() {
                tracing::warn!(error = %err, "failed to open the log file");
            }
        }
    }

    /// Handles a click on "Start at login": `muda` has already flipped
    /// `autostart_item`'s `checked` state by the time this runs, so
    /// `is_checked()` below reads the *requested* new state. Enables or
    /// disables accordingly, then re-reads the actual on-disk/registry state
    /// and forces `set_checked` to it -- so a failure (logged via `warn!`,
    /// never shown to the user otherwise: this menu has no error dialog)
    /// leaves the checkbox reflecting reality, not the click.
    fn toggle_autostart(autostart_item: &CheckMenuItem) {
        let agent_path = match agent::autostart::agent_path() {
            Ok(path) => Some(path),
            Err(err) => {
                tracing::warn!(error = %err, "failed to resolve rcdesk-agent's own path for autostart");
                None
            }
        };

        if let Some(path) = &agent_path {
            let want_enabled = autostart_item.is_checked();
            let result = if want_enabled {
                platform::autostart::enable(path)
            } else {
                platform::autostart::disable()
            };
            if let Err(err) = result {
                tracing::warn!(error = %err, "failed to change the start-at-login registration");
            }
        }

        let actual = agent_path
            .as_ref()
            .and_then(|path| platform::autostart::is_enabled(path).ok())
            .unwrap_or(false);
        autostart_item.set_checked(actual);
    }

    #[cfg(target_os = "macos")]
    fn open_system_settings_pane(url: &str) {
        if let Err(err) = std::process::Command::new("open").arg(url).spawn() {
            tracing::warn!(error = %err, url, "failed to open System Settings pane");
        }
    }

    #[cfg(target_os = "macos")]
    fn screen_item_label(screen_ok: bool) -> String {
        if screen_ok {
            "Screen Recording: granted".to_string()
        } else {
            "Grant Screen Recording permission…".to_string()
        }
    }

    #[cfg(target_os = "macos")]
    fn input_item_label(input_ok: bool) -> String {
        if input_ok {
            "Accessibility: granted".to_string()
        } else {
            "Grant Accessibility permission…".to_string()
        }
    }

    /// "Quit rcdesk": if a session is active, asks it to end
    /// (`AgentCommand::EndSession`) and waits up to 1s for the status to
    /// leave `InSession` before tearing the runtime down -- gives the peer
    /// a chance to see the session close cleanly instead of just vanishing.
    /// Never returns.
    fn quit(
        cmd_tx: mpsc::UnboundedSender<AgentCommand>,
        status_rx: watch::Receiver<AgentStatus>,
        runtime: tokio::runtime::Runtime,
    ) -> ! {
        if matches!(*status_rx.borrow(), AgentStatus::InSession { .. }) {
            let _ = cmd_tx.send(AgentCommand::EndSession);
            let deadline = Instant::now() + Duration::from_secs(1);
            while Instant::now() < deadline {
                if !matches!(*status_rx.borrow(), AgentStatus::InSession { .. }) {
                    break;
                }
                std::thread::sleep(Duration::from_millis(20));
            }
        }
        tracing::info!("rcdesk-agent quitting");
        runtime.shutdown_timeout(Duration::from_secs(1));
        std::process::exit(0);
    }
}
