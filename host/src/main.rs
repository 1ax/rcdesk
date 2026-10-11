use std::io::Write as _;
use std::path::PathBuf;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use clap::{Parser, Subcommand};
use tokio::sync::mpsc::error::TryRecvError;
use tokio::sync::watch;

use rcdesk_host::access::{self, AccessStore};
use rcdesk_host::agent;
use rcdesk_host::app::{self, CaptureBackend, EncoderBackend, ReconnectPolicy, ServeOptions};
use rcdesk_host::capture::{self, FrameSource};
use rcdesk_host::device::DeviceStore;
use rcdesk_host::encode::{build_encoder, EncoderConfig, RateTarget};
use rcdesk_host::pipeline::Pipeline;
use rcdesk_host::platform;
use rcdesk_host::signaling::{AgentStatus, Keepalive};

#[derive(Parser)]
#[command(name = "rcdesk-host", version = rcdesk_host::build_info::FULL_VERSION, about = "rcdesk host agent")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// List capturable displays.
    ListDisplays,
    /// Run the capture -> convert -> encode pipeline without a network
    /// transport, and print throughput/size statistics.
    Bench {
        /// Use the synthetic frame source instead of real screen capture.
        #[arg(long)]
        synthetic: bool,
        /// Display id to capture (see `list-displays`); defaults to the primary display.
        #[arg(long)]
        display: Option<u32>,
        #[arg(long, default_value_t = 30)]
        fps: u32,
        #[arg(long, default_value_t = 6000)]
        bitrate: u32,
        #[arg(long, default_value_t = 5)]
        seconds: u32,
        /// Hard ceiling on encoder QP (0..=51); unset leaves the encoder's
        /// own default (openh264: 30; VideoToolbox/Media Foundation: none).
        #[arg(long, value_parser = clap::value_parser!(u8).range(0..=51))]
        max_qp: Option<u8>,
        /// Optional path to dump the raw Annex-B stream to.
        #[arg(long)]
        dump: Option<PathBuf>,
        /// Windows only: which screen-capture backend to use. `auto` picks
        /// Windows Graphics Capture when the video driver reports Direct3D
        /// 11 support, GDI otherwise; `gdi`/`wgc` force one or the other.
        /// Ignored (and ignored by `--synthetic`) on other platforms.
        #[arg(long, value_enum, default_value_t = CaptureBackend::Auto)]
        capture: CaptureBackend,
        /// Which H.264 encoder backend to use. `auto` picks the platform's
        /// hardware encoder if available, openh264 otherwise.
        #[arg(long, value_enum, default_value_t = EncoderBackend::Auto)]
        encoder: EncoderBackend,
    },
    /// Connect to a signaling server, register as a host, and serve
    /// incoming WebRTC sessions. Reconnects with backoff if the connection
    /// is lost or never comes up -- see `docs/dev-run.md`.
    Serve {
        /// Signaling server WebSocket URL.
        #[arg(long, default_value = "ws://127.0.0.1:8080/ws")]
        server: String,
        /// Host name shown to clients. Defaults to this computer's own name,
        /// then $HOSTNAME, then "rcdesk-host" -- see `docs/dev-run.md`.
        #[arg(long)]
        name: Option<String>,
        /// Use the synthetic frame source instead of real screen capture.
        #[arg(long)]
        synthetic: bool,
        /// Display id to capture (see `list-displays`); defaults to the primary display.
        #[arg(long)]
        display: Option<u32>,
        #[arg(long, default_value_t = 30)]
        fps: u32,
        #[arg(long, default_value_t = 6000)]
        bitrate: u32,
        /// Hard ceiling on encoder QP (0..=51); unset leaves the encoder's
        /// own default (openh264: 30; VideoToolbox/Media Foundation: none).
        #[arg(long, value_parser = clap::value_parser!(u8).range(0..=51))]
        max_qp: Option<u8>,
        /// Extra STUN/TURN server URL, added on top of whatever the
        /// signaling server sends in `Registered` (see `docs/dev-run.md`).
        /// Repeatable. Empty by default: with no `--stun`, ICE servers come
        /// entirely from the signaling server.
        #[arg(long = "stun")]
        stun: Vec<String>,
        /// Disable mouse/keyboard injection: input messages are received and
        /// logged but never touch the real mouse/keyboard. See
        /// `docs/dev-run.md` -- useful for verifying a session without the
        /// macOS "Universal Access" permission granted.
        #[arg(long)]
        no_input: bool,
        /// Disable clipboard text sync (slice 2.5b): the host neither reads
        /// nor writes the system clipboard for any session. See
        /// `docs/dev-run.md`.
        #[arg(long)]
        no_clipboard: bool,
        /// Disable file transfer (slice 3.4): the host refuses every file the
        /// client tries to send and tells the client up front
        /// (`HostInfo.files_enabled = false`).
        #[arg(long)]
        no_files: bool,
        /// Disable the bitrate/fps adaptation controller (slice 2.3): the
        /// encoder keeps `--bitrate`/`--fps` for the whole session. For
        /// before/after measurements.
        #[arg(long)]
        no_adapt: bool,
        /// Windows only: which screen-capture backend to use. `auto` picks
        /// Windows Graphics Capture when the video driver reports Direct3D
        /// 11 support, GDI otherwise; `gdi`/`wgc` force one or the other.
        /// Ignored (and ignored by `--synthetic`) on other platforms.
        #[arg(long, value_enum, default_value_t = CaptureBackend::Auto)]
        capture: CaptureBackend,
        /// Which H.264 encoder backend to use. `auto` picks the platform's
        /// hardware encoder if available, openh264 otherwise.
        #[arg(long, value_enum, default_value_t = EncoderBackend::Auto)]
        encoder: EncoderBackend,
        /// Windows, полный режим (слайс 4.1a): помощник службы `rcdesk`. Данные
        /// -- в `%ProgramData%\rcdesk\`, лог -- `helper.log`, PIN -- в `pin.txt`
        /// вместо stdout. Запускается только службой.
        #[cfg(target_os = "windows")]
        #[arg(long, hide = true)]
        machine: bool,
    },
    /// Manage `rcdesk-agent`'s "start at login" registration (a LaunchAgent
    /// plist on macOS, an `HKCU\...\Run` value on Windows) -- no admin
    /// rights needed on either OS. Off by default; this and the agent's own
    /// "Start at login" menu item are the only two ways to turn it on. See
    /// docs/dev-run.md / docs/host-windows.md.
    Autostart {
        #[command(subcommand)]
        action: AutostartAction,
    },
    /// Управление паролем доступа к хосту (слайс 3.2): пароль хранится на
    /// хосте только в виде записи OPAQUE (`access.json` рядом с
    /// `device.json`) -- сам пароль хост не сохраняет.
    Password {
        #[command(subcommand)]
        action: PasswordAction,
    },
    /// Windows, полный режим (слайс 4.1a): служба SYSTEM `rcdesk` + помощник
    /// в консольной сессии. install/uninstall/start/stop требуют cmd от имени
    /// администратора.
    #[cfg(target_os = "windows")]
    Service {
        #[command(subcommand)]
        action: ServiceAction,
    },
}

#[cfg(target_os = "windows")]
#[derive(Subcommand)]
enum ServiceAction {
    /// Установить службу (автозапуск, перезапуск при сбое) и запустить её.
    Install {
        /// Signaling server WebSocket URL.
        #[arg(long, default_value = "wss://rcdesk.app/ws")]
        server: String,
    },
    /// Остановить и удалить службу; данные в %ProgramData%\rcdesk не трогаются.
    Uninstall,
    /// Запустить установленную службу.
    Start,
    /// Остановить службу.
    Stop,
    /// Состояние службы, PIN и пути данных.
    Status,
    /// Точка входа SCM (binpath службы); руками не запускается.
    #[command(hide = true)]
    Run {
        #[arg(long, default_value = "wss://rcdesk.app/ws")]
        server: String,
    },
}

#[derive(Subcommand)]
enum AutostartAction {
    /// Register `rcdesk-agent` to start automatically at the next login.
    On,
    /// Remove the "start at login" registration.
    Off,
    /// Print whether autostart is currently on, and for which path.
    Status,
}

#[derive(Subcommand)]
enum PasswordAction {
    /// Задать (или сменить) пароль доступа.
    Set,
    /// Убрать пароль доступа.
    Clear,
    /// Показать, задан ли пароль доступа.
    Status,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // Must happen before any GDI/enigo/GetSystemMetrics call -- see
    // `platform::windows::dpi` for why.
    #[cfg(target_os = "windows")]
    rcdesk_host::platform::windows::dpi::set_dpi_aware();

    let cli = Cli::parse();
    init_logging(&cli.command);

    let version = rcdesk_host::build_info::FULL_VERSION;
    let platform = platform::name();
    tracing::info!("rcdesk-host {version} on {platform}");

    match cli.command {
        Command::ListDisplays => run_list_displays(),
        Command::Bench {
            synthetic,
            display,
            fps,
            bitrate,
            seconds,
            max_qp,
            dump,
            capture,
            encoder,
        } => run_bench(
            synthetic, display, fps, bitrate, seconds, max_qp, dump, capture, encoder,
        ),
        Command::Serve {
            server,
            name,
            synthetic,
            display,
            fps,
            bitrate,
            max_qp,
            stun,
            no_input,
            no_clipboard,
            no_files,
            no_adapt,
            capture,
            encoder,
            #[cfg(target_os = "windows")]
            machine,
        } => {
            #[cfg(target_os = "windows")]
            let data_dir = machine.then(agent::paths::machine_data_dir);
            #[cfg(not(target_os = "windows"))]
            let data_dir = None;
            let opts = ServeOptions {
                server,
                name,
                synthetic,
                display,
                fps,
                bitrate,
                max_qp,
                stun,
                no_input,
                no_clipboard,
                no_files,
                no_adapt,
                capture,
                encoder,
                data_dir,
            };
            run_serve(opts).await
        }
        Command::Autostart { action } => run_autostart(action),
        Command::Password { action } => run_password(action),
        #[cfg(target_os = "windows")]
        Command::Service { action } => run_service(action),
    }
}

/// Лог-приёмник по команде. Обычные команды -- stderr, как раньше. Полный
/// режим Windows (4.1a) пишет в файлы машинного каталога логов: `service run`
/// -- `service.log` (инициализирует сама `platform::windows::service::run`),
/// `serve --machine` -- `helper.log`. Консоли у них нет.
fn init_logging(command: &Command) {
    #[cfg(target_os = "windows")]
    {
        match command {
            Command::Service {
                action: ServiceAction::Run { .. },
            } => return,
            Command::Serve { machine: true, .. } => {
                let dir = agent::paths::machine_log_dir();
                if let Err(err) = agent::logging::init_named(&dir, rcdesk_host::service::HELPER_LOG)
                {
                    eprintln!(
                        "rcdesk-host: не удалось открыть лог в {}: {err}",
                        dir.display()
                    );
                    std::process::exit(1);
                }
                return;
            }
            _ => {}
        }
    }
    #[cfg(not(target_os = "windows"))]
    let _ = command;

    tracing_subscriber::fmt()
        // Slice 3.2 fix: the classic Windows console (cmd.exe on the Win10
        // stand) doesn't interpret ANSI escapes unless VT processing is
        // switched on, so colors came out as raw `←[2m` noise. Plain text
        // there; colors stay on macOS terminals.
        .with_ansi(cfg!(not(target_os = "windows")))
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| {
                tracing_subscriber::EnvFilter::new(rcdesk_host::DEFAULT_LOG_FILTER)
            }),
        )
        .init();
}

/// `rcdesk-host service ...` (Windows, слайс 4.1a).
#[cfg(target_os = "windows")]
fn run_service(action: ServiceAction) -> anyhow::Result<()> {
    use rcdesk_host::platform::windows::service as svc;
    use rcdesk_host::service as common;

    match action {
        ServiceAction::Install { server } => svc::install(&server),
        ServiceAction::Uninstall => svc::uninstall(),
        ServiceAction::Start => svc::start(),
        ServiceAction::Stop => svc::stop(),
        ServiceAction::Run { server } => svc::run(&server),
        ServiceAction::Status => {
            let state = svc::status()?;
            println!("Служба {}: {}", common::SERVICE_NAME, state.label());
            let data = agent::paths::machine_data_dir();
            match common::read_pin(&data) {
                Some(pin) => println!("PIN: {pin}"),
                None => println!(
                    "PIN: нет (помощник ещё не зарегистрирован, либо нужны права администратора)"
                ),
            }
            println!("Данные: {}", data.display());
            println!("Логи: {}", agent::paths::machine_log_dir().display());
            Ok(())
        }
    }
}

/// Resolves `rcdesk-agent`'s path relative to this `rcdesk-host` binary
/// (`agent::autostart::agent_path`) and applies `action` to it -- see that
/// function's doc comment for why `rcdesk-host` and `rcdesk-agent` resolve
/// the same "which path" question differently.
fn run_autostart(action: AutostartAction) -> anyhow::Result<()> {
    let agent = rcdesk_host::agent::autostart::agent_path()?;
    match action {
        AutostartAction::On => {
            rcdesk_host::platform::autostart::enable(&agent)?;
            println!("autostart: on ({})", agent.display());
        }
        AutostartAction::Off => {
            rcdesk_host::platform::autostart::disable()?;
            println!("autostart: off");
        }
        AutostartAction::Status => {
            if rcdesk_host::platform::autostart::is_enabled(&agent)? {
                println!("autostart: on ({})", agent.display());
            } else {
                println!("autostart: off");
            }
        }
    }
    Ok(())
}

/// `rcdesk-host password set|clear|status` -- see `rcdesk_host::access`'s
/// module doc comment for what's actually stored (an OPAQUE registration
/// record, never the password itself). Uses the same data directory as
/// `DeviceStore` (`agent::paths::data_dir()`). На Windows при установленной
/// службе (полный режим, 4.1a) -- машинный каталог `%ProgramData%\rcdesk`:
/// туда смотрит помощник; запись требует прав администратора.
fn run_password(action: PasswordAction) -> anyhow::Result<()> {
    let (data_dir, machine) = password_data_dir();
    let store = AccessStore::new(&data_dir);
    let admin_hint = |err: anyhow::Error| -> anyhow::Error {
        if machine {
            err.context("служба установлена, пароль хранится в машинном каталоге: запустите cmd от имени администратора")
        } else {
            err
        }
    };
    match action {
        PasswordAction::Set => {
            let password = read_new_password()?;
            if let Err(message) = access::validate_password(&password) {
                anyhow::bail!(message);
            }
            let record = access::register(&password)?;
            store.save(&record).map_err(admin_hint)?;
            println!("Пароль задан: {}", store.path().display());
            println!("Хост применит его при следующем подключении -- перезапуск агента не нужен.");
        }
        PasswordAction::Clear => {
            let had_password = store.load().is_some();
            store.clear().map_err(admin_hint)?;
            if had_password {
                println!("Пароль снят");
            } else {
                println!("Пароль не был задан");
            }
        }
        PasswordAction::Status => {
            if store.load().is_some() {
                println!("Пароль: задан ({})", store.path().display());
            } else {
                println!("Пароль: не задан");
            }
        }
    }
    Ok(())
}

/// Каталог для `password`: машинный, если на Windows установлена служба
/// (второе значение `true`), иначе пользовательский.
fn password_data_dir() -> (PathBuf, bool) {
    #[cfg(target_os = "windows")]
    {
        if rcdesk_host::platform::windows::service::is_installed() {
            return (agent::paths::machine_data_dir(), true);
        }
    }
    (agent::paths::data_dir(), false)
}

/// Reads the new password: one line from stdin when it isn't a terminal
/// (scripts/tests -- e.g. this sub-step's own manual CLI check pipes a
/// password in), or `rpassword`'s hidden-input prompt plus a confirmation
/// repeat when it is a real terminal.
fn read_new_password() -> anyhow::Result<String> {
    use std::io::IsTerminal as _;

    if std::io::stdin().is_terminal() {
        let first = prompt_hidden("Пароль доступа: ")?;
        let second = prompt_hidden("Ещё раз: ")?;
        if first != second {
            anyhow::bail!("Пароли не совпадают");
        }
        Ok(first)
    } else {
        let mut line = String::new();
        std::io::stdin().read_line(&mut line)?;
        Ok(line.trim_end_matches(['\n', '\r']).to_string())
    }
}

/// Hidden password prompt (slice 3.2 fix). `rpassword::prompt_password`
/// writes the prompt to the console as raw UTF-8 bytes, which the Windows
/// console renders as mojibake under its OEM code page (cp866 on the
/// stand). The prompt goes through `std::io::stderr` instead -- Rust's
/// stdio converts to UTF-16 for a Windows console -- and only the hidden
/// read is left to `rpassword`.
fn prompt_hidden(prompt: &str) -> anyhow::Result<String> {
    use std::io::Write as _;

    eprint!("{prompt}");
    std::io::stderr().flush()?;
    Ok(rpassword::read_password()?)
}

fn run_list_displays() -> anyhow::Result<()> {
    let displays = capture::list_displays()?;
    if displays.is_empty() {
        println!("no capturable displays found");
    }
    for display in displays {
        let primary = if display.primary { "primary" } else { "-" };
        println!(
            "{}\t{}\t{}x{}@{},{}\t{}",
            display.id, display.title, display.width, display.height, display.x, display.y, primary
        );
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn run_bench(
    synthetic: bool,
    display: Option<u32>,
    fps: u32,
    bitrate_kbps: u32,
    seconds: u32,
    max_qp: Option<u8>,
    dump: Option<PathBuf>,
    capture: CaptureBackend,
    encoder: EncoderBackend,
) -> anyhow::Result<()> {
    let source: Box<dyn FrameSource> = if synthetic {
        Box::new(capture::synthetic::for_display(display.unwrap_or(1), fps)?)
    } else {
        app::build_screen_source(display, fps, capture, &[])?
    };

    let (width, height) = source.size();
    let cfg = EncoderConfig {
        width,
        height,
        fps,
        bitrate_kbps,
        keyframe_interval_frames: fps.max(1) * 2,
        max_qp,
    };
    let (encoder, encoder_kind) = build_encoder(encoder.kind(), cfg)?;
    println!("encoder={}", encoder_kind.name());

    let mut handle = Pipeline::start(source, encoder, RateTarget { bitrate_kbps, fps });
    let mut dump_file = dump.as_ref().map(std::fs::File::create).transpose()?;

    let mut sizes: Vec<usize> = Vec::new();
    let mut keyframe_sizes: Vec<usize> = Vec::new();
    let mut delta_sizes: Vec<usize> = Vec::new();
    let mut capture_to_encoded_us: Vec<usize> = Vec::new();
    let start = Instant::now();
    let run_for = Duration::from_secs(u64::from(seconds));
    // Exercise request_keyframe() (mirrors a PLI/FIR from a client, see
    // ARCHITECTURE.md §4.1) partway through the run rather than only on
    // connect, so the bench also demonstrates forced-keyframe behaviour.
    let keyframe_request_at = run_for / 2;
    let mut requested_keyframe = false;

    while start.elapsed() < run_for {
        if !requested_keyframe && start.elapsed() >= keyframe_request_at {
            handle.request_keyframe();
            requested_keyframe = true;
        }
        match handle.frames.try_recv() {
            Ok(frame) => {
                if let Some(file) = dump_file.as_mut() {
                    file.write_all(&frame.data)?;
                }
                sizes.push(frame.data.len());
                if frame.keyframe {
                    keyframe_sizes.push(frame.data.len());
                } else {
                    delta_sizes.push(frame.data.len());
                }
                capture_to_encoded_us.push((frame.ts - frame.captured_at).as_micros() as usize);
            }
            Err(TryRecvError::Empty) => std::thread::sleep(Duration::from_millis(5)),
            Err(TryRecvError::Disconnected) => break,
        }
    }

    // Pick up whatever already landed in the channel without waiting further.
    while let Ok(frame) = handle.frames.try_recv() {
        if let Some(file) = dump_file.as_mut() {
            file.write_all(&frame.data)?;
        }
        sizes.push(frame.data.len());
        if frame.keyframe {
            keyframe_sizes.push(frame.data.len());
        } else {
            delta_sizes.push(frame.data.len());
        }
        capture_to_encoded_us.push((frame.ts - frame.captured_at).as_micros() as usize);
    }

    handle.stop();
    let elapsed = start.elapsed().as_secs_f64().max(1e-9);

    let captured = handle.stats.captured.load(Ordering::Relaxed);
    let encoded = handle.stats.encoded.load(Ordering::Relaxed);
    let dropped = handle.stats.dropped.load(Ordering::Relaxed);
    let keyframes = handle.stats.keyframes.load(Ordering::Relaxed);

    let total_bytes: usize = sizes.iter().sum();
    let avg_fps = encoded as f64 / elapsed;
    let avg_kbps = (total_bytes as f64 * 8.0 / 1000.0) / elapsed;
    let avg_size = if sizes.is_empty() {
        0.0
    } else {
        total_bytes as f64 / sizes.len() as f64
    };
    let p95_size = percentile(&sizes, 95.0);

    println!("captured={captured} encoded={encoded} dropped={dropped} keyframes={keyframes}");
    println!("avg_fps={avg_fps:.2} avg_bitrate_kbps={avg_kbps:.1}");
    println!("avg_frame_size_bytes={avg_size:.1} p95_frame_size_bytes={p95_size}");

    let avg_keyframe_bytes = if keyframe_sizes.is_empty() {
        0.0
    } else {
        keyframe_sizes.iter().sum::<usize>() as f64 / keyframe_sizes.len() as f64
    };
    let avg_delta_bytes = if delta_sizes.is_empty() {
        0.0
    } else {
        delta_sizes.iter().sum::<usize>() as f64 / delta_sizes.len() as f64
    };
    println!("avg_keyframe_bytes={avg_keyframe_bytes:.1} avg_delta_bytes={avg_delta_bytes:.1}");

    let avg_capture_to_encoded_ms = if capture_to_encoded_us.is_empty() {
        0.0
    } else {
        capture_to_encoded_us.iter().sum::<usize>() as f64
            / capture_to_encoded_us.len() as f64
            / 1000.0
    };
    let p95_capture_to_encoded_ms = percentile(&capture_to_encoded_us, 95.0) as f64 / 1000.0;
    println!(
        "avg_capture_to_encoded_ms={avg_capture_to_encoded_ms:.2} \
         p95_capture_to_encoded_ms={p95_capture_to_encoded_ms:.2}"
    );

    if let Some(path) = dump.as_ref() {
        println!("dumped Annex-B stream to {}", path.display());
    }

    Ok(())
}

fn percentile(sizes: &[usize], pct: f64) -> usize {
    if sizes.is_empty() {
        return 0;
    }
    let mut sorted = sizes.to_vec();
    sorted.sort_unstable();
    let idx = ((pct / 100.0) * (sorted.len() as f64 - 1.0)).round() as usize;
    sorted[idx.min(sorted.len() - 1)]
}

/// Builds the long-lived `HostContext` once, then hands it to
/// `app::run_agent`, which connects, registers, serves sessions and
/// reconnects with backoff for as long as the process runs -- see
/// `docs/dev-run.md`. A second task prints `PIN: NNNNNN` (same format as
/// before slice 2.6a) whenever the agent reports a `Registered` status whose
/// PIN differs from the last one printed -- a session ending drops the
/// status back to `Registered` with the *same* PIN it already had (see
/// `signaling::run`), which must not print again; a real reconnect gets a
/// fresh PIN from the server and does.
async fn run_serve(opts: ServeOptions) -> anyhow::Result<()> {
    let name = app::resolve_host_name(opts.name.as_deref());
    let ctx = app::build_host_context(&opts)?;

    // `serve` has no tray UI to send `AgentCommand`s from -- `_cmd_tx` is
    // just kept alive so `cmd_rx.recv()` inside `run_agent` parks instead of
    // seeing a closed channel (see `run_agent`'s doc comment); nothing ever
    // sends on it.
    let (_cmd_tx, cmd_rx) = tokio::sync::mpsc::unbounded_channel();

    let device_store = DeviceStore::new(&opts.resolved_data_dir());

    // Машинный режим (`--machine`, 4.1a): PIN уходит в `pin.txt` каталога
    // данных, а не в stdout (у помощника службы консоли нет).
    let machine_dir = opts.data_dir.clone();
    // Windows, машинный режим: удержание системы от сна на время сеанса из
    // одного выделенного потока (`SetThreadExecutionState` потоковый).
    #[cfg(target_os = "windows")]
    let power = machine_dir
        .is_some()
        .then(rcdesk_host::platform::windows::power::SessionActivityThread::spawn);

    let (status_tx, mut status_rx) = watch::channel(AgentStatus::Connecting);
    let print_task = tokio::spawn(async move {
        let mut last_printed: Option<String> = None;
        let mut in_session = false;
        while status_rx.changed().await.is_ok() {
            let now_in_session;
            {
                let status = status_rx.borrow();
                now_in_session = matches!(&*status, AgentStatus::InSession { .. });
                if let AgentStatus::Registered { pin } = &*status {
                    if last_printed.as_deref() != Some(pin.as_str()) {
                        match &machine_dir {
                            Some(dir) => {
                                if let Err(err) = rcdesk_host::service::write_pin(dir, pin) {
                                    tracing::warn!(error = %err, "не удалось записать pin.txt");
                                }
                            }
                            None => println!("PIN: {pin}"),
                        }
                        last_printed = Some(pin.clone());
                    }
                }
            }
            if now_in_session != in_session {
                #[cfg(target_os = "windows")]
                {
                    if let Some(power) = &power {
                        power.set(now_in_session);
                    }
                }
                in_session = now_in_session;
            }
        }
    });

    let never = app::run_agent(
        &ctx,
        &opts.server,
        &name,
        ReconnectPolicy::default(),
        Keepalive::default(),
        &device_store,
        status_tx,
        cmd_rx,
    )
    .await?;
    print_task.abort();
    match never {}
}
