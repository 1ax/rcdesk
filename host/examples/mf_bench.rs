//! Measurement tool: how does the software Media Foundation H.264 encoder
//! behave on a weak Windows host (GDI `BitBlt` capture, no D3D11)? Runs a
//! fixed list of variants (1080p vs 720p via `StretchBlt` HALFTONE,
//! `AVEncCommonQualityVsSpeed`, `AVLowLatencyMode` off, Main/High profile)
//! through a production-like pipeline (paced capture thread -> single-slot
//! mailbox -> encode thread), parses slice QP from the Annex-B output and
//! prints one Markdown table. Not used by the production host.
//!
//! `cargo run --release -p rcdesk-host --example mf_bench -- --help`

// Shared with `vt_scroll_bench`; unused (but tested) off Windows.
#[path = "common/h264_qp.rs"]
mod qp;

#[cfg(target_os = "windows")]
mod win {
    use std::ffi::c_void;
    use std::fmt::Write as _;
    use std::mem::size_of;
    use std::slice;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Arc, Condvar, Mutex};
    use std::thread;
    use std::time::{Duration, Instant};

    use clap::Parser;
    use rcdesk_host::capture::RawFrame;
    use rcdesk_host::encode::mediafoundation::{MediaFoundationEncoder, MfProfile, MfTuning};
    use rcdesk_host::encode::{Encoder, EncoderConfig};
    use rcdesk_host::platform::windows::{dpi, timer};
    use windows::Win32::Graphics::Gdi::{
        BitBlt, CreateCompatibleDC, CreateDIBSection, DeleteDC, DeleteObject, GetDC, ReleaseDC,
        SelectObject, SetBrushOrgEx, SetStretchBltMode, StretchBlt, BITMAPINFO, BITMAPINFOHEADER,
        BI_RGB, DIB_RGB_COLORS, HALFTONE, HBITMAP, HDC, HGDIOBJ, SRCCOPY,
    };
    use windows::Win32::UI::WindowsAndMessaging::{GetSystemMetrics, SM_CXSCREEN, SM_CYSCREEN};

    use super::qp;

    /// Warm-up per variant, not counted in the statistics.
    const WARMUP: Duration = Duration::from_secs(2);

    #[derive(Parser)]
    #[command(about = "Media Foundation software H.264 encoder bench (Windows)")]
    struct Args {
        /// Measured seconds per variant (after the 2 s warm-up).
        #[arg(long, default_value_t = 15)]
        seconds: u64,
        /// Target bitrate, kbit/s.
        #[arg(long, default_value_t = 6000)]
        bitrate: u32,
        /// Capture pacing and encoder fps.
        #[arg(long, default_value_t = 30)]
        fps: u32,
        /// Run only the named variant(s); repeatable.
        #[arg(long = "only")]
        only: Vec<String>,
        /// Markdown report path (overwritten).
        #[arg(long, default_value = "mf_bench.md")]
        out: String,
    }

    #[derive(Clone, Copy)]
    enum Res {
        /// Native screen size (rounded down to even).
        Native,
        /// Height 720 via `StretchBlt` HALFTONE.
        P720,
    }

    struct Variant {
        name: &'static str,
        res: Res,
        tuning: MfTuning,
    }

    fn variants() -> Vec<Variant> {
        let t = |q: Option<u32>, low_latency: bool, profile: MfProfile| MfTuning {
            quality_vs_speed: q,
            low_latency,
            profile,
        };
        let base = MfProfile::ConstrainedBaseline;
        let v = |name, res, tuning| Variant { name, res, tuning };
        vec![
            v("base1080", Res::Native, MfTuning::default()),
            v("base720", Res::P720, MfTuning::default()),
            v("qvs0_1080", Res::Native, t(Some(0), true, base)),
            v("qvs50_1080", Res::Native, t(Some(50), true, base)),
            v("qvs100_1080", Res::Native, t(Some(100), true, base)),
            v("qvs0_720", Res::P720, t(Some(0), true, base)),
            v("qvs50_720", Res::P720, t(Some(50), true, base)),
            v("qvs100_720", Res::P720, t(Some(100), true, base)),
            v("nolat1080", Res::Native, t(None, false, base)),
            v("nolat720", Res::P720, t(None, false, base)),
            v("main1080", Res::Native, t(None, true, MfProfile::Main)),
            v("high1080", Res::Native, t(None, true, MfProfile::High)),
            v("high720", Res::P720, t(None, true, MfProfile::High)),
        ]
    }

    // ------------------------------------------------------------------
    // GDI capture
    // ------------------------------------------------------------------

    /// Screen grabber: `BitBlt` (same size) or `StretchBlt` HALFTONE (scaled)
    /// from the screen DC into a 32bpp top-down DIB section, then one copy
    /// into a `RawFrame::Bgra`. No duplicate-frame detection.
    struct Grabber {
        // Field order = drop order after `Drop::drop` (which un-selects the
        // bitmap): bitmap first, then the memory DC, then the screen DC.
        bitmap: HBITMAP,
        hdc_mem: HDC,
        hdc_screen: HDC,
        old_bitmap: HGDIOBJ,
        pixels: *const u8,
        src_w: i32,
        src_h: i32,
        width: u32,
        height: u32,
    }

    // SAFETY: a `Grabber` is created on the main thread and then moved into
    // the single capture thread, which is its only user for the rest of its
    // life (created, used and dropped one thread at a time). The GDI handles
    // are not thread-affine for a memory DC/DIB section used by one thread
    // at a time, and `pixels` is never touched from another thread.
    unsafe impl Send for Grabber {}

    impl Grabber {
        fn new(src_w: u32, src_h: u32, width: u32, height: u32) -> Result<Self, String> {
            // SAFETY: `GetDC(None)` asks for the DC of the whole screen; a
            // null return means "unavailable", not undefined behaviour.
            let hdc_screen = unsafe { GetDC(None) };
            if hdc_screen.is_invalid() {
                return Err("GetDC failed".to_string());
            }
            // SAFETY: `hdc_screen` was just validated.
            let hdc_mem = unsafe { CreateCompatibleDC(Some(hdc_screen)) };
            if hdc_mem.is_invalid() {
                // SAFETY: valid DC from above, not used again on this path.
                unsafe {
                    ReleaseDC(None, hdc_screen);
                }
                return Err("CreateCompatibleDC failed".to_string());
            }
            let bitmap_info = BITMAPINFO {
                bmiHeader: BITMAPINFOHEADER {
                    biSize: size_of::<BITMAPINFOHEADER>() as u32,
                    biWidth: width as i32,
                    // Negative height: top-down rows, no manual flip.
                    biHeight: -(height as i32),
                    biPlanes: 1,
                    biBitCount: 32,
                    biCompression: BI_RGB.0,
                    ..Default::default()
                },
                ..Default::default()
            };
            let mut pixels_ptr: *mut c_void = std::ptr::null_mut();
            // SAFETY: `hdc_screen` is valid, `bitmap_info` describes a 32bpp
            // top-down DIB and `pixels_ptr` is a valid out-parameter.
            let bitmap = unsafe {
                CreateDIBSection(
                    Some(hdc_screen),
                    &bitmap_info,
                    DIB_RGB_COLORS,
                    &mut pixels_ptr,
                    None,
                    0,
                )
            };
            let bitmap = match bitmap {
                Ok(bitmap) => bitmap,
                Err(err) => {
                    // SAFETY: both DCs were created above, unused afterwards.
                    unsafe {
                        let _ = DeleteDC(hdc_mem);
                        ReleaseDC(None, hdc_screen);
                    }
                    return Err(format!("CreateDIBSection: {err}"));
                }
            };
            // SAFETY: `hdc_mem` and `bitmap` were both just created.
            let old_bitmap = unsafe { SelectObject(hdc_mem, bitmap.into()) };
            if old_bitmap.is_invalid() {
                // SAFETY: all three were created above, unused afterwards.
                unsafe {
                    let _ = DeleteObject(bitmap.into());
                    let _ = DeleteDC(hdc_mem);
                    ReleaseDC(None, hdc_screen);
                }
                return Err("SelectObject failed".to_string());
            }
            if width != src_w || height != src_h {
                // SAFETY: `hdc_mem` is valid. HALFTONE requires the brush
                // origin to be reset right after selecting the mode.
                unsafe {
                    SetStretchBltMode(hdc_mem, HALFTONE);
                    let _ = SetBrushOrgEx(hdc_mem, 0, 0, None);
                }
            }
            Ok(Self {
                bitmap,
                hdc_mem,
                hdc_screen,
                old_bitmap,
                pixels: pixels_ptr.cast(),
                src_w: src_w as i32,
                src_h: src_h as i32,
                width,
                height,
            })
        }

        fn grab(&mut self) -> Result<RawFrame, String> {
            if self.width as i32 == self.src_w && self.height as i32 == self.src_h {
                // SAFETY: both DCs are valid for the whole lifetime of
                // `self`; the destination bitmap is selected into `hdc_mem`.
                unsafe {
                    BitBlt(
                        self.hdc_mem,
                        0,
                        0,
                        self.width as i32,
                        self.height as i32,
                        Some(self.hdc_screen),
                        0,
                        0,
                        SRCCOPY,
                    )
                }
                .map_err(|err| format!("BitBlt: {err}"))?;
            } else {
                // SAFETY: as above.
                let ok = unsafe {
                    StretchBlt(
                        self.hdc_mem,
                        0,
                        0,
                        self.width as i32,
                        self.height as i32,
                        Some(self.hdc_screen),
                        0,
                        0,
                        self.src_w,
                        self.src_h,
                        SRCCOPY,
                    )
                };
                if !ok.as_bool() {
                    return Err("StretchBlt failed".to_string());
                }
            }
            let len = self.width as usize * self.height as usize * 4;
            // SAFETY: `pixels` points into the DIB section, alive and sized
            // `width * height * 4` bytes for the lifetime of `self`; the GDI
            // call above is synchronous and finished writing.
            let data = unsafe { slice::from_raw_parts(self.pixels, len) }.to_vec();
            Ok(RawFrame::Bgra {
                width: self.width,
                height: self.height,
                data,
                stride: self.width as usize * 4,
                ts: Instant::now(),
            })
        }
    }

    impl Drop for Grabber {
        fn drop(&mut self) {
            // SAFETY: the handles were created in `new` and are not used
            // after this; the bitmap is un-selected before it is deleted,
            // then the memory DC and the screen DC are released.
            unsafe {
                SelectObject(self.hdc_mem, self.old_bitmap);
                let _ = DeleteObject(self.bitmap.into());
                let _ = DeleteDC(self.hdc_mem);
                ReleaseDC(None, self.hdc_screen);
            }
        }
    }

    // ------------------------------------------------------------------
    // Pipeline
    // ------------------------------------------------------------------

    #[derive(Default)]
    struct SlotState {
        frame: Option<RawFrame>,
        closed: bool,
    }

    /// Single-frame mailbox, as in the production pipeline: a new frame
    /// replaces an unread one.
    struct Slot {
        state: Mutex<SlotState>,
        cv: Condvar,
    }

    struct CaptureOut {
        /// (when, blt + copy ms)
        samples: Vec<(Instant, f64)>,
        /// When an unread frame was overwritten.
        overwritten: Vec<Instant>,
    }

    struct EncSample {
        at: Instant,
        ms: f64,
        data: Vec<u8>,
        keyframe: bool,
    }

    fn capture_loop(
        mut grabber: Grabber,
        fps: u32,
        slot: Arc<Slot>,
        stop: Arc<AtomicBool>,
    ) -> Result<CaptureOut, String> {
        let period = Duration::from_secs_f64(1.0 / f64::from(fps.max(1)));
        let mut out = CaptureOut {
            samples: Vec::new(),
            overwritten: Vec::new(),
        };
        let mut next = Instant::now();
        let mut result = Ok(());
        while !stop.load(Ordering::Acquire) {
            let now = Instant::now();
            if next > now {
                thread::sleep(next - now);
            }
            // After a late tick re-anchor instead of bursting to catch up.
            next = next.max(Instant::now()) + period;
            let t0 = Instant::now();
            let frame = match grabber.grab() {
                Ok(frame) => frame,
                Err(err) => {
                    result = Err(err);
                    break;
                }
            };
            let done = Instant::now();
            out.samples
                .push((done, done.duration_since(t0).as_secs_f64() * 1000.0));
            let mut state = slot.state.lock().unwrap_or_else(|e| e.into_inner());
            if state.frame.replace(frame).is_some() {
                out.overwritten.push(done);
            }
            drop(state);
            slot.cv.notify_one();
        }
        let mut state = slot.state.lock().unwrap_or_else(|e| e.into_inner());
        state.closed = true;
        drop(state);
        slot.cv.notify_one();
        result.map(|()| out)
    }

    fn encode_loop(
        mut encoder: MediaFoundationEncoder,
        slot: Arc<Slot>,
    ) -> Result<Vec<EncSample>, String> {
        let mut out = Vec::new();
        loop {
            let frame = {
                let mut state = slot.state.lock().unwrap_or_else(|e| e.into_inner());
                loop {
                    if let Some(frame) = state.frame.take() {
                        break Some(frame);
                    }
                    if state.closed {
                        break None;
                    }
                    state = slot.cv.wait(state).unwrap_or_else(|e| e.into_inner());
                }
            };
            let Some(frame) = frame else {
                return Ok(out);
            };
            let t0 = Instant::now();
            let encoded = encoder.encode(&frame, false).map_err(|e| e.to_string())?;
            let at = Instant::now();
            if let Some(encoded) = encoded {
                out.push(EncSample {
                    at,
                    ms: at.duration_since(t0).as_secs_f64() * 1000.0,
                    data: encoded.data,
                    keyframe: encoded.keyframe,
                });
            }
        }
    }

    // ------------------------------------------------------------------
    // Statistics
    // ------------------------------------------------------------------

    fn avg(v: &[f64]) -> f64 {
        if v.is_empty() {
            0.0
        } else {
            v.iter().sum::<f64>() / v.len() as f64
        }
    }

    /// `q` in 0..=1, nearest-rank percentile of an unsorted slice.
    fn percentile(v: &[f64], q: f64) -> f64 {
        if v.is_empty() {
            return 0.0;
        }
        let mut sorted = v.to_vec();
        sorted.sort_by(|a, b| a.total_cmp(b));
        let rank = ((q * sorted.len() as f64).ceil() as usize).clamp(1, sorted.len());
        sorted[rank - 1]
    }

    struct Row {
        name: &'static str,
        size: (u32, u32),
        result: Result<Stats, String>,
    }

    struct Stats {
        captured: usize,
        encoded: usize,
        fps: f64,
        overwritten: usize,
        cap_avg: f64,
        cap_p95: f64,
        enc_avg: f64,
        enc_p95: f64,
        kbps: f64,
        keyframes: usize,
        max_kb: f64,
        qp: Option<(f64, f64)>,
    }

    fn run_variant(
        args: &Args,
        var: &Variant,
        screen: (u32, u32),
    ) -> Result<(Stats, (u32, u32)), (String, (u32, u32))> {
        let (sw, sh) = screen;
        let size = match var.res {
            Res::Native => (sw & !1, sh & !1),
            Res::P720 => {
                let w = (720.0 * f64::from(sw) / f64::from(sh) / 2.0).round() as u32 * 2;
                (w, 720)
            }
        };
        let fail = |e: String| (e, size);
        let cfg = EncoderConfig {
            width: size.0,
            height: size.1,
            fps: args.fps,
            bitrate_kbps: args.bitrate,
            keyframe_interval_frames: args.fps.max(1) * 2,
            max_qp: None,
        };
        let encoder = MediaFoundationEncoder::new_tuned(cfg, true, var.tuning)
            .map_err(|e| fail(e.to_string()))?;
        let grabber = Grabber::new(sw, sh, size.0, size.1).map_err(fail)?;

        let slot = Arc::new(Slot {
            state: Mutex::new(SlotState::default()),
            cv: Condvar::new(),
        });
        let stop = Arc::new(AtomicBool::new(false));
        let start = Instant::now();
        let cap = {
            let (slot, stop, fps) = (slot.clone(), stop.clone(), args.fps);
            thread::spawn(move || capture_loop(grabber, fps, slot, stop))
        };
        let enc = {
            let slot = slot.clone();
            thread::spawn(move || encode_loop(encoder, slot))
        };

        eprintln!(
            "{} ... warm-up {} s + {} s",
            var.name,
            WARMUP.as_secs(),
            args.seconds
        );
        let t0 = start + WARMUP;
        let t_end = t0 + Duration::from_secs(args.seconds);
        let wait = t_end.saturating_duration_since(Instant::now());
        thread::sleep(wait);
        stop.store(true, Ordering::Release);

        let cap = cap
            .join()
            .map_err(|_| fail("capture thread panicked".to_string()))?
            .map_err(fail)?;
        let enc = enc
            .join()
            .map_err(|_| fail("encode thread panicked".to_string()))?
            .map_err(fail)?;

        // QP: parse every output in order (the parser needs the SPS/PPS
        // from warm-up frames), keep only measured frames.
        let mut parser = qp::QpParser::default();
        let mut qps = Vec::new();
        let mut cap_ms = Vec::new();
        let mut enc_ms = Vec::new();
        let (mut encoded, mut keyframes, mut bytes, mut max_bytes) = (0usize, 0usize, 0usize, 0);
        for s in &enc {
            let q = parser.frame_qp(&s.data);
            if s.at < t0 || s.at > t_end {
                continue;
            }
            encoded += 1;
            keyframes += usize::from(s.keyframe);
            bytes += s.data.len();
            max_bytes = max_bytes.max(s.data.len());
            enc_ms.push(s.ms);
            if let Some(q) = q {
                qps.push(f64::from(q));
            }
        }
        for &(at, ms) in &cap.samples {
            if at >= t0 && at <= t_end {
                cap_ms.push(ms);
            }
        }
        let overwritten = cap
            .overwritten
            .iter()
            .filter(|&&at| at >= t0 && at <= t_end)
            .count();
        let secs = args.seconds.max(1) as f64;
        Ok((
            Stats {
                captured: cap_ms.len(),
                encoded,
                fps: encoded as f64 / secs,
                overwritten,
                cap_avg: avg(&cap_ms),
                cap_p95: percentile(&cap_ms, 0.95),
                enc_avg: avg(&enc_ms),
                enc_p95: percentile(&enc_ms, 0.95),
                kbps: bytes as f64 * 8.0 / secs / 1000.0,
                keyframes,
                max_kb: max_bytes as f64 / 1024.0,
                qp: (!qps.is_empty()).then(|| (avg(&qps), percentile(&qps, 0.90))),
            },
            size,
        ))
    }

    fn render(args: &Args, screen: (u32, u32), rows: &[Row]) -> String {
        let mut s = String::new();
        let _ = writeln!(
            s,
            "Screen {}x{}, bitrate {} kbit/s, fps {}, seconds {} (+{} s warm-up)\n",
            screen.0,
            screen.1,
            args.bitrate,
            args.fps,
            args.seconds,
            WARMUP.as_secs()
        );
        s.push_str(
            "| variant | size | captured | encoded | fps | overwritten | capture ms avg/p95 \
             | encode ms avg/p95 | kbit/s | keyframes | max frame kB | QP avg | QP p90 |\n",
        );
        s.push_str("|---|---|---:|---:|---:|---:|---|---|---:|---:|---:|---:|---:|\n");
        for row in rows {
            let size = format!("{}x{}", row.size.0, row.size.1);
            match &row.result {
                Ok(st) => {
                    let (qa, qp90) = st.qp.map_or(("-".to_string(), "-".to_string()), |(a, p)| {
                        (format!("{a:.1}"), format!("{p:.0}"))
                    });
                    let _ = writeln!(
                        s,
                        "| {} | {} | {} | {} | {:.1} | {} | {:.1} / {:.1} | {:.1} / {:.1} \
                         | {:.0} | {} | {:.1} | {} | {} |",
                        row.name,
                        size,
                        st.captured,
                        st.encoded,
                        st.fps,
                        st.overwritten,
                        st.cap_avg,
                        st.cap_p95,
                        st.enc_avg,
                        st.enc_p95,
                        st.kbps,
                        st.keyframes,
                        st.max_kb,
                        qa,
                        qp90
                    );
                }
                Err(err) => {
                    let err = err.replace('|', "/").replace('\n', " ");
                    let _ = writeln!(
                        s,
                        "| {} | {} | ERROR: {} | - | - | - | - | - | - | - | - | - | - |",
                        row.name, size, err
                    );
                }
            }
        }
        s
    }

    pub fn main() {
        let args = Args::parse();
        dpi::set_dpi_aware();
        let _timer = timer::high_resolution();

        // SAFETY: `GetSystemMetrics` has no preconditions.
        let (sw, sh) = unsafe {
            (
                GetSystemMetrics(SM_CXSCREEN).max(0) as u32,
                GetSystemMetrics(SM_CYSCREEN).max(0) as u32,
            )
        };
        if sw < 2 || sh < 2 {
            eprintln!("mf_bench: cannot read the screen size ({sw}x{sh})");
            std::process::exit(1);
        }

        let all = variants();
        for name in &args.only {
            if !all.iter().any(|v| v.name == name) {
                eprintln!("mf_bench: unknown variant {name:?}");
                std::process::exit(2);
            }
        }
        let mut rows = Vec::new();
        for var in all
            .iter()
            .filter(|v| args.only.is_empty() || args.only.iter().any(|n| n == v.name))
        {
            let row = match run_variant(&args, var, (sw, sh)) {
                Ok((stats, size)) => Row {
                    name: var.name,
                    size,
                    result: Ok(stats),
                },
                Err((err, size)) => {
                    eprintln!("{}: {err}", var.name);
                    Row {
                        name: var.name,
                        size,
                        result: Err(err),
                    }
                }
            };
            rows.push(row);
        }

        let report = render(&args, (sw, sh), &rows);
        print!("{report}");
        if let Err(err) = std::fs::write(&args.out, &report) {
            eprintln!("mf_bench: cannot write {}: {err}", args.out);
            std::process::exit(1);
        }
    }
}

#[cfg(target_os = "windows")]
fn main() {
    win::main();
}

#[cfg(not(target_os = "windows"))]
fn main() {
    println!("mf_bench: Windows only");
}
