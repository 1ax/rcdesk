use std::io::Write as _;
use std::path::PathBuf;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use clap::{Parser, Subcommand};
use tokio::sync::mpsc::error::TryRecvError;
use tokio::sync::watch;

use rcdesk_host::app::{self, CaptureBackend, EncoderBackend, ReconnectPolicy, ServeOptions};
use rcdesk_host::capture::{self, FrameSource};
use rcdesk_host::encode::{build_encoder, EncoderConfig, RateTarget};
use rcdesk_host::pipeline::Pipeline;
use rcdesk_host::platform;
use rcdesk_host::signaling::{AgentStatus, Keepalive};

#[derive(Parser)]
#[command(name = "rcdesk-host", version, about = "rcdesk host agent")]
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
    },
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // Must happen before any GDI/enigo/GetSystemMetrics call -- see
    // `platform::windows::dpi` for why.
    #[cfg(target_os = "windows")]
    rcdesk_host::platform::windows::dpi::set_dpi_aware();

    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    let version = env!("CARGO_PKG_VERSION");
    let platform = platform::name();
    tracing::info!("rcdesk-host {version} on {platform}");

    let cli = Cli::parse();
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
            no_adapt,
            capture,
            encoder,
        } => {
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
                no_adapt,
                capture,
                encoder,
            };
            run_serve(opts).await
        }
    }
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
        app::build_screen_source(display, fps, capture)?
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

    let (status_tx, mut status_rx) = watch::channel(AgentStatus::Connecting);
    let print_task = tokio::spawn(async move {
        let mut last_printed: Option<String> = None;
        while status_rx.changed().await.is_ok() {
            if let AgentStatus::Registered { pin } = &*status_rx.borrow() {
                if last_printed.as_deref() != Some(pin.as_str()) {
                    println!("PIN: {pin}");
                    last_printed = Some(pin.clone());
                }
            }
        }
    });

    let never = app::run_agent(
        &ctx,
        &opts.server,
        &name,
        ReconnectPolicy::default(),
        Keepalive::default(),
        status_tx,
        cmd_rx,
    )
    .await?;
    print_task.abort();
    match never {}
}
