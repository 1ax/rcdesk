use std::io::Write as _;
use std::path::PathBuf;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use clap::{Parser, Subcommand, ValueEnum};
use tokio::sync::mpsc::error::TryRecvError;

use rcdesk_host::capture::{self, FrameSource};
use rcdesk_host::cursor::CursorSource;
use rcdesk_host::encode::{build_encoder, EncoderConfig, EncoderKind, RateTarget};
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

/// Which H.264 encoder backend to use.
#[derive(Clone, Copy, ValueEnum)]
enum EncoderBackend {
    /// The platform's native encoder (VideoToolbox on macOS; Media
    /// Foundation on Windows, hardware MFT preferred, Microsoft's software
    /// one otherwise), openh264 if that fails -- see
    /// `encode::build_encoder`'s doc comment.
    Auto,
    /// Software encoder, built from source. Works on every platform.
    Openh264,
    /// macOS hardware encoder (VideoToolbox). Selecting it on another OS
    /// fails with a "not available" error.
    Videotoolbox,
    /// Windows Media Foundation H.264 MFT (hardware if present, otherwise
    /// Microsoft's software encoder). Selecting it on another OS fails with
    /// a "not available" error.
    Mediafoundation,
}

impl EncoderBackend {
    fn kind(self) -> Option<EncoderKind> {
        match self {
            EncoderBackend::Auto => None,
            EncoderBackend::Openh264 => Some(EncoderKind::OpenH264),
            EncoderBackend::Videotoolbox => Some(EncoderKind::VideoToolbox),
            EncoderBackend::Mediafoundation => Some(EncoderKind::MediaFoundation),
        }
    }
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
            no_adapt,
            capture,
            encoder,
        } => {
            run_serve(
                server, name, synthetic, display, fps, bitrate, max_qp, stun, no_input, no_adapt,
                capture, encoder,
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
        let primary = if display.primary { "primary" } else { "-" };
        println!(
            "{}\t{}\t{}x{}@{},{}\t{}",
            display.id, display.title, display.width, display.height, display.x, display.y, primary
        );
    }
    Ok(())
}

/// Resolves `--display` to a concrete display id before it reaches a
/// backend's `new`: an explicit `--display N` passes through unchanged
/// (an id that turns out not to exist is still that backend's error to
/// report), `None` picks `capture::default_display` out of
/// `capture::list_displays()` (propagating a `list_displays` error, e.g. no
/// screen-recording permission, as-is) and logs which display was picked.
#[cfg(any(target_os = "macos", target_os = "windows"))]
fn resolve_display(display: Option<u32>) -> anyhow::Result<u32> {
    if let Some(id) = display {
        return Ok(id);
    }
    let displays = capture::list_displays()?;
    let chosen = capture::default_display(&displays)
        .ok_or_else(|| anyhow::anyhow!("no capturable display found"))?;
    tracing::info!(display_id = chosen.id, title = %chosen.title, "capturing display");
    Ok(chosen.id)
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
    let display = Some(resolve_display(display)?);
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
            let display = Some(resolve_display(display)?);
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
    encoder: EncoderBackend,
) -> anyhow::Result<()> {
    let source: Box<dyn FrameSource> = if synthetic {
        Box::new(capture::synthetic::for_display(display.unwrap_or(1), fps)?)
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
    no_adapt: bool,
    capture: CaptureBackend,
    encoder: EncoderBackend,
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
                    Box::new(capture::synthetic::for_display(display.unwrap_or(1), fps)?)
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
        adapt: !no_adapt,
        max_qp,
        encoder: encoder.kind(),
        build_source,
        build_injector,
        build_cursor_source: Box::new(build_real_cursor_source),
        runtime,
    };

    client.run(ctx).await
}
