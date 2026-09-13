use std::io::Write as _;
use std::path::PathBuf;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use clap::{Parser, Subcommand, ValueEnum};
use tokio::sync::mpsc::error::TryRecvError;

use rcdesk_host::capture::{self, FrameSource};
use rcdesk_host::cursor::CursorSource;
use rcdesk_host::encode::openh264::OpenH264Encoder;
use rcdesk_host::encode::{Encoder, EncoderConfig};
use rcdesk_host::input::{Injector, NoopInjector};
use rcdesk_host::pipeline::Pipeline;
use rcdesk_host::platform;
use rcdesk_host::signaling::{HostContext, SignalingClient};
use rcdesk_host::transport::SessionConfig;

#[derive(Parser)]
#[command(name = "rcdesk-host", version, about = "rcdesk host agent")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

/// Which screen-capture backend to use on Windows. No effect on
/// macOS/other (there is only `scap`/synthetic there) beyond `Gdi` being
/// rejected -- see `build_screen_source`.
#[derive(Clone, Copy, ValueEnum)]
enum CaptureBackend {
    /// Windows Graphics Capture (`scap`) if the video driver reports
    /// Direct3D 11 support, GDI (`BitBlt`) otherwise.
    Auto,
    /// Force Windows Graphics Capture (`scap`), even if the driver looks
    /// like it can't do Direct3D 11 -- for comparing against `gdi` on
    /// hardware where `auto` already falls back.
    Wgc,
    /// Force GDI (`BitBlt`): works on any driver/VM, higher CPU cost, no
    /// "yellow border" capture indicator.
    Gdi,
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
        /// Display id to capture (scap only); defaults to the first display.
        #[arg(long)]
        display: Option<u32>,
        #[arg(long, default_value_t = 30)]
        fps: u32,
        #[arg(long, default_value_t = 6000)]
        bitrate: u32,
        #[arg(long, default_value_t = 5)]
        seconds: u32,
        /// Hard ceiling on encoder QP (0..=51); unset leaves the encoder's
        /// own default.
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
    },
    /// Connect to a signaling server, register as a host, and serve
    /// incoming WebRTC sessions.
    Serve {
        /// Signaling server WebSocket URL.
        #[arg(long, default_value = "ws://127.0.0.1:8080/ws")]
        server: String,
        /// Host name shown to clients. Defaults to $HOSTNAME, or
        /// "rcdesk-host" if that isn't set.
        #[arg(long)]
        name: Option<String>,
        /// Use the synthetic frame source instead of real screen capture.
        #[arg(long)]
        synthetic: bool,
        /// Display id to capture (scap only); defaults to the first display.
        #[arg(long)]
        display: Option<u32>,
        #[arg(long, default_value_t = 30)]
        fps: u32,
        #[arg(long, default_value_t = 6000)]
        bitrate: u32,
        /// Hard ceiling on encoder QP (0..=51); unset leaves the encoder's
        /// own default.
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
        /// Windows only: which screen-capture backend to use. `auto` picks
        /// Windows Graphics Capture when the video driver reports Direct3D
        /// 11 support, GDI otherwise; `gdi`/`wgc` force one or the other.
        /// Ignored (and ignored by `--synthetic`) on other platforms.
        #[arg(long, value_enum, default_value_t = CaptureBackend::Auto)]
        capture: CaptureBackend,
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
        } => run_bench(
            synthetic, display, fps, bitrate, seconds, max_qp, dump, capture,
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
            capture,
        } => {
            run_serve(
                server, name, synthetic, display, fps, bitrate, max_qp, stun, no_input, capture,
            )
            .await
        }
    }
}

fn run_list_displays() -> anyhow::Result<()> {
    let displays = capture::list_displays()?;
    if displays.is_empty() {
        println!("no capturable displays found");
    }
    for display in displays {
        println!("{}\t{}", display.id, display.title);
    }
    Ok(())
}

/// Builds the real screen-capture source. On Windows this is where
/// `--capture auto` decides between Windows Graphics Capture (`scap`) and
/// the GDI fallback (`capture::gdi`) by probing Direct3D 11 support first
/// (`platform::windows::d3d::wgc_supported`) -- see that module and
/// `capture::gdi` for why: `scap` 0.0.8 panics outright on drivers below
/// feature level 11_0 instead of returning an error.
#[cfg(target_os = "windows")]
fn build_screen_source(
    display: Option<u32>,
    fps: u32,
    backend: CaptureBackend,
) -> anyhow::Result<Box<dyn FrameSource>> {
    let use_wgc = match backend {
        CaptureBackend::Wgc => true,
        CaptureBackend::Gdi => false,
        CaptureBackend::Auto => rcdesk_host::platform::windows::d3d::wgc_supported(),
    };
    if use_wgc {
        tracing::info!(backend = "wgc", "screen capture backend");
        Ok(Box::new(capture::scap::ScapSource::new(display, fps)?))
    } else {
        tracing::info!(backend = "gdi", "screen capture backend");
        Ok(Box::new(capture::gdi::GdiSource::new(display, fps)?))
    }
}

#[cfg(target_os = "macos")]
fn build_screen_source(
    display: Option<u32>,
    fps: u32,
    backend: CaptureBackend,
) -> anyhow::Result<Box<dyn FrameSource>> {
    match backend {
        CaptureBackend::Gdi => Err(anyhow::anyhow!(
            "--capture gdi is a Windows-only fallback, not supported on macOS"
        )),
        CaptureBackend::Auto | CaptureBackend::Wgc => {
            Ok(Box::new(capture::scap::ScapSource::new(display, fps)?))
        }
    }
}

#[cfg(not(any(target_os = "macos", target_os = "windows")))]
fn build_screen_source(
    _display: Option<u32>,
    _fps: u32,
    _backend: CaptureBackend,
) -> anyhow::Result<Box<dyn FrameSource>> {
    Err(capture::CaptureError::Unsupported.into())
}

/// The real, platform-backed injector (see `rcdesk_host::input::enigo`).
/// Falls back to `NoopInjector` on platforms with no such backend, exactly
/// like `build_screen_source` falls back to an error for capture -- except
/// here a no-op is the correct behavior rather than a failure, since a host
/// with no way to inject input isn't a broken host, just one that can only
/// be watched.
#[cfg(any(target_os = "macos", target_os = "windows"))]
fn build_real_injector() -> anyhow::Result<Box<dyn Injector>> {
    Ok(Box::new(rcdesk_host::input::enigo::EnigoInjector::new()?))
}

#[cfg(not(any(target_os = "macos", target_os = "windows")))]
fn build_real_injector() -> anyhow::Result<Box<dyn Injector>> {
    Ok(Box::new(NoopInjector::new()))
}

/// The real, platform-backed cursor-shape source (see
/// `rcdesk_host::cursor::macos`/`::windows`), the same fallback pattern as
/// `build_real_injector`.
#[cfg(target_os = "macos")]
fn build_real_cursor_source() -> Box<dyn CursorSource> {
    Box::new(rcdesk_host::cursor::macos::MacCursorSource::new())
}

#[cfg(target_os = "windows")]
fn build_real_cursor_source() -> Box<dyn CursorSource> {
    Box::new(rcdesk_host::cursor::windows::WinCursorSource::new())
}

#[cfg(not(any(target_os = "macos", target_os = "windows")))]
fn build_real_cursor_source() -> Box<dyn CursorSource> {
    Box::new(rcdesk_host::cursor::NoopCursorSource::new())
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
) -> anyhow::Result<()> {
    let source: Box<dyn FrameSource> = if synthetic {
        Box::new(capture::synthetic::SyntheticSource::new(1280, 720, fps))
    } else {
        build_screen_source(display, fps, capture)?
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
    let encoder: Box<dyn Encoder> = Box::new(OpenH264Encoder::new(cfg)?);

    let mut handle = Pipeline::start(source, encoder);
    let mut dump_file = dump.as_ref().map(std::fs::File::create).transpose()?;

    let mut sizes: Vec<usize> = Vec::new();
    let mut keyframe_sizes: Vec<usize> = Vec::new();
    let mut delta_sizes: Vec<usize> = Vec::new();
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

#[allow(clippy::too_many_arguments)]
async fn run_serve(
    server: String,
    name: Option<String>,
    synthetic: bool,
    display: Option<u32>,
    fps: u32,
    bitrate: u32,
    max_qp: Option<u8>,
    stun: Vec<String>,
    no_input: bool,
    capture: CaptureBackend,
) -> anyhow::Result<()> {
    let name = name
        .or_else(|| std::env::var("HOSTNAME").ok())
        .unwrap_or_else(|| "rcdesk-host".to_string());

    let client = SignalingClient::connect(&server, &name).await?;
    println!("PIN: {}", client.pin());
    tracing::info!(
        pin = client.pin(),
        host_id = client.host_id(),
        "registered with signaling server"
    );

    let runtime = webrtc::runtime::default_runtime()
        .ok_or_else(|| anyhow::anyhow!("no webrtc runtime available"))?;

    let build_source: Box<dyn Fn() -> anyhow::Result<Box<dyn FrameSource>> + Send + Sync> =
        if synthetic {
            Box::new(move || {
                Ok(
                    Box::new(capture::synthetic::SyntheticSource::new(1280, 720, fps))
                        as Box<dyn FrameSource>,
                )
            })
        } else {
            Box::new(move || build_screen_source(display, fps, capture))
        };

    let build_injector: Box<dyn Fn() -> anyhow::Result<Box<dyn Injector>> + Send + Sync> =
        if no_input {
            Box::new(|| Ok(Box::new(NoopInjector::new()) as Box<dyn Injector>))
        } else {
            Box::new(build_real_injector)
        };

    // Server-provided ICE servers (STUN, and TURN when configured -- see
    // `server/src/ice.rs`) plus any `--stun` overrides from the CLI.
    let mut ice_servers: Vec<proto::signal::IceServer> = client.ice_servers().to_vec();
    ice_servers.extend(stun.into_iter().map(|url| proto::signal::IceServer {
        urls: vec![url],
        username: None,
        credential: None,
    }));

    let ctx = HostContext {
        session: SessionConfig {
            ice_servers,
            udp_addrs: vec!["0.0.0.0:0".to_string()],
            fps,
        },
        bitrate_kbps: bitrate,
        max_qp,
        build_source,
        build_injector,
        build_cursor_source: Box::new(build_real_cursor_source),
        runtime,
    };

    client.run(ctx).await
}
