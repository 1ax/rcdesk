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

/// CPU bilinear downscale used by replay mode; a plain function so it is
/// unit-tested on every platform (unused off Windows).
#[cfg_attr(not(target_os = "windows"), allow(dead_code))]
mod scale {
    /// Per destination index: (src index 0, src index 1, weight of index 1),
    /// sampling at pixel centres (like `drawImage` with bilinear smoothing).
    fn axis(dst: usize, src: usize) -> Vec<(usize, usize, f32)> {
        (0..dst)
            .map(|d| {
                let pos =
                    ((d as f32 + 0.5) * src as f32 / dst as f32 - 0.5).clamp(0.0, (src - 1) as f32);
                let i0 = pos.floor() as usize;
                let i1 = (i0 + 1).min(src - 1);
                (i0, i1, pos - i0 as f32)
            })
            .collect()
    }

    /// Bilinear downscale of a tightly packed BGRA image (stride = `sw * 4`)
    /// to `dw * dh * 4` bytes. Returns an empty vector for a zero dimension.
    pub fn downscale_bgra(src: &[u8], sw: usize, sh: usize, dw: usize, dh: usize) -> Vec<u8> {
        if sw == 0 || sh == 0 || dw == 0 || dh == 0 || src.len() < sw * sh * 4 {
            return Vec::new();
        }
        let xs = axis(dw, sw);
        let ys = axis(dh, sh);
        let mut out = Vec::with_capacity(dw * dh * 4);
        for &(y0, y1, fy) in &ys {
            let r0 = &src[y0 * sw * 4..(y0 + 1) * sw * 4];
            let r1 = &src[y1 * sw * 4..(y1 + 1) * sw * 4];
            for &(x0, x1, fx) in &xs {
                for c in 0..4 {
                    let p = |row: &[u8], x: usize| f32::from(row[x * 4 + c]);
                    let top = p(r0, x0) + (p(r0, x1) - p(r0, x0)) * fx;
                    let bot = p(r1, x0) + (p(r1, x1) - p(r1, x0)) * fx;
                    let v = top + (bot - top) * fy;
                    out.push((v + 0.5) as u8);
                }
            }
        }
        out
    }

    #[cfg(test)]
    mod tests {
        use super::downscale_bgra;

        #[test]
        fn flat_image_stays_flat() {
            let src: Vec<u8> = [10u8, 20, 30, 255]
                .iter()
                .copied()
                .cycle()
                .take(16 * 9 * 4)
                .collect();
            let out = downscale_bgra(&src, 16, 9, 8, 5);
            assert!(out.chunks(4).all(|p| p == [10, 20, 30, 255]));
        }

        #[test]
        fn output_size_is_dw_dh_4() {
            let src = vec![7u8; 10 * 6 * 4];
            assert_eq!(downscale_bgra(&src, 10, 6, 4, 3).len(), 4 * 3 * 4);
            assert_eq!(downscale_bgra(&src, 10, 6, 10, 6).len(), 10 * 6 * 4);
        }

        #[test]
        fn two_by_two_to_one_is_the_mean() {
            // Channel 0 of the four pixels: 0, 100, 200, 100 -> mean 100.
            let mut src = vec![0u8; 16];
            for (i, v) in [0u8, 100, 200, 100].iter().enumerate() {
                src[i * 4] = *v;
            }
            let out = downscale_bgra(&src, 2, 2, 1, 1);
            assert_eq!(out, vec![100, 0, 0, 0]);
        }
    }
}

#[cfg(target_os = "windows")]
mod win {
    use std::collections::VecDeque;
    use std::ffi::c_void;
    use std::fmt::Write as _;
    use std::mem::size_of;
    use std::slice;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Arc, Condvar, Mutex};
    use std::thread;
    use std::time::{Duration, Instant};

    use clap::{Parser, ValueEnum};
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

    use super::{qp, scale};

    /// Warm-up per variant, not counted in the statistics.
    const WARMUP: Duration = Duration::from_secs(2);

    #[derive(Clone, Copy, PartialEq, Eq, ValueEnum)]
    enum Mode {
        /// Every variant encodes the same recorded frames, plus latency and dumps.
        Replay,
        /// Every variant captures the live screen (frames differ between variants).
        Live,
    }

    #[derive(Parser)]
    #[command(about = "Media Foundation software H.264 encoder bench (Windows)")]
    struct Args {
        /// Bench mode.
        #[arg(long, value_enum, default_value_t = Mode::Replay)]
        mode: Mode,
        /// Replay: frames recorded from the screen before the run.
        #[arg(long, default_value_t = 40)]
        record_frames: usize,
        /// Replay: pacing of recording and of feeding the encoder, fps.
        #[arg(long, default_value_t = 20)]
        replay_fps: u32,
        /// Replay: seconds of each variant's stream dumped to a .h264 file (0 = none).
        #[arg(long, default_value_t = 5)]
        dump_seconds: u64,
        /// Replay: directory for the .h264 dumps.
        #[arg(long, default_value = ".")]
        dump_dir: String,
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
        #[arg(long, default_value = "mfbench.md")]
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

    /// Encoded frame size (width, height) of a variant.
    type Size = (u32, u32);

    /// Encoded frame size of a resolution choice on a given screen.
    fn variant_size(res: Res, screen: (u32, u32)) -> Size {
        let (sw, sh) = screen;
        match res {
            Res::Native => (sw & !1, sh & !1),
            Res::P720 => {
                let w = (720.0 * f64::from(sw) / f64::from(sh) / 2.0).round() as u32 * 2;
                (w, 720)
            }
        }
    }

    fn run_variant(
        args: &Args,
        var: &Variant,
        screen: (u32, u32),
    ) -> Result<(Stats, Size), (String, Size)> {
        let (sw, sh) = screen;
        let size = variant_size(var.res, screen);
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

    // ------------------------------------------------------------------
    // Replay mode: every variant encodes the same recorded frames
    // ------------------------------------------------------------------

    fn replay_variants() -> Vec<Variant> {
        let t = |low_latency: bool, profile: MfProfile| MfTuning {
            quality_vs_speed: None,
            low_latency,
            profile,
        };
        let base = MfProfile::ConstrainedBaseline;
        let v = |name, res, tuning| Variant { name, res, tuning };
        vec![
            v("r1080", Res::Native, MfTuning::default()),
            v("r720", Res::P720, MfTuning::default()),
            v("nolat1080", Res::Native, t(false, base)),
            v("nolat720", Res::P720, t(false, base)),
            v("high1080", Res::Native, t(true, MfProfile::High)),
            v("high720", Res::P720, t(true, MfProfile::High)),
            v("nolat-high720", Res::P720, t(false, MfProfile::High)),
        ]
    }

    /// A recorded clip: tightly packed BGRA frames of one size.
    struct Clip {
        width: u32,
        height: u32,
        frames: Vec<Vec<u8>>,
    }

    /// Crop (top-left) a tightly packed BGRA image.
    fn crop_bgra(src: &[u8], sw: usize, dw: usize, dh: usize) -> Vec<u8> {
        let mut out = Vec::with_capacity(dw * dh * 4);
        for y in 0..dh {
            out.extend_from_slice(&src[y * sw * 4..(y * sw + dw) * 4]);
        }
        out
    }

    /// Feeds the clip into the mailbox at `fps`, looping; returns feed times.
    fn feed_loop(
        clip: Arc<Clip>,
        fps: u32,
        slot: Arc<Slot>,
        stop: Arc<AtomicBool>,
    ) -> Vec<Instant> {
        let period = Duration::from_secs_f64(1.0 / f64::from(fps.max(1)));
        let mut fed = Vec::new();
        let mut next = Instant::now();
        let mut i = 0usize;
        while !stop.load(Ordering::Acquire) {
            let now = Instant::now();
            if next > now {
                thread::sleep(next - now);
            }
            next = next.max(Instant::now()) + period;
            let mut frame = RawFrame::Bgra {
                width: clip.width,
                height: clip.height,
                data: clip.frames[i % clip.frames.len()].clone(),
                stride: clip.width as usize * 4,
                ts: Instant::now(),
            };
            i += 1;
            let at = Instant::now();
            frame.set_ts(at);
            fed.push(at);
            let mut state = slot.state.lock().unwrap_or_else(|e| e.into_inner());
            state.frame = Some(frame);
            drop(state);
            slot.cv.notify_one();
        }
        let mut state = slot.state.lock().unwrap_or_else(|e| e.into_inner());
        state.closed = true;
        drop(state);
        slot.cv.notify_one();
        fed
    }

    struct ReplaySample {
        at: Instant,
        ms: f64,
        data: Vec<u8>,
        keyframe: bool,
        /// Frames given to the encoder minus outputs received, at this output.
        lag: usize,
        /// Output time minus the capture ts of the matching (FIFO) input.
        latency_ms: f64,
    }

    fn replay_encode_loop(
        mut encoder: MediaFoundationEncoder,
        slot: Arc<Slot>,
    ) -> Result<Vec<ReplaySample>, String> {
        let mut out = Vec::new();
        let mut inputs: VecDeque<Instant> = VecDeque::new();
        let (mut submitted, mut outputs) = (0usize, 0usize);
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
            inputs.push_back(frame.ts());
            submitted += 1;
            let t0 = Instant::now();
            let encoded = encoder.encode(&frame, false).map_err(|e| e.to_string())?;
            let at = Instant::now();
            if let Some(encoded) = encoded {
                outputs += 1;
                let latency_ms = inputs.pop_front().map_or(0.0, |ts| {
                    at.saturating_duration_since(ts).as_secs_f64() * 1000.0
                });
                out.push(ReplaySample {
                    at,
                    ms: at.duration_since(t0).as_secs_f64() * 1000.0,
                    data: encoded.data,
                    keyframe: encoded.keyframe,
                    lag: submitted - outputs,
                    latency_ms,
                });
            }
        }
    }

    struct ReplayStats {
        fed: usize,
        encoded: usize,
        fps: f64,
        enc_avg: f64,
        enc_p95: f64,
        kbps: f64,
        bits_per_pixel: f64,
        keyframes: usize,
        max_kb: f64,
        qp: Option<(f64, f64)>,
        lag_avg: f64,
        lag_max: usize,
        lat_avg: f64,
        lat_p95: f64,
    }

    struct ReplayRow {
        name: &'static str,
        size: Size,
        result: Result<ReplayStats, String>,
    }

    type ReplayRun = Result<(ReplayStats, Size), (String, Size)>;

    /// Does the Annex-B stream start with SPS, PPS and an IDR slice?
    fn has_sps_pps_idr(annexb: &[u8]) -> bool {
        let types: Vec<u8> = qp::split_nals(annexb)
            .iter()
            .filter_map(|n| n.first().map(|b| b & 0x1f))
            .collect();
        types.contains(&7) && types.contains(&8) && types.contains(&5)
    }

    fn run_replay(args: &Args, var: &Variant, clip: Arc<Clip>, size: Size) -> ReplayRun {
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

        let slot = Arc::new(Slot {
            state: Mutex::new(SlotState::default()),
            cv: Condvar::new(),
        });
        let stop = Arc::new(AtomicBool::new(false));
        let start = Instant::now();
        let feeder = {
            let (slot, stop, fps) = (slot.clone(), stop.clone(), args.replay_fps);
            thread::spawn(move || feed_loop(clip, fps, slot, stop))
        };
        let enc = {
            let slot = slot.clone();
            thread::spawn(move || replay_encode_loop(encoder, slot))
        };

        eprintln!(
            "{} ... warm-up {} s + {} s",
            var.name,
            WARMUP.as_secs(),
            args.seconds
        );
        let t0 = start + WARMUP;
        let t_end = t0 + Duration::from_secs(args.seconds);
        thread::sleep(t_end.saturating_duration_since(Instant::now()));
        stop.store(true, Ordering::Release);

        let fed = feeder
            .join()
            .map_err(|_| fail("feed thread panicked".to_string()))?;
        let enc = enc
            .join()
            .map_err(|_| fail("encode thread panicked".to_string()))?
            .map_err(fail)?;

        if args.dump_seconds > 0 {
            let path = std::path::Path::new(&args.dump_dir).join(format!("r-{}.h264", var.name));
            let limit = Duration::from_secs(args.dump_seconds);
            let mut bytes = Vec::new();
            for s in enc.iter().filter(|s| s.at.duration_since(start) < limit) {
                bytes.extend_from_slice(&s.data);
            }
            if let Some(first) = enc.first() {
                if !has_sps_pps_idr(&first.data) {
                    eprintln!("{}: first output has no SPS/PPS/IDR", var.name);
                }
            }
            if let Err(err) = std::fs::write(&path, &bytes) {
                eprintln!("{}: cannot write {}: {err}", var.name, path.display());
            } else {
                eprintln!(
                    "{}: dumped {} bytes to {}",
                    var.name,
                    bytes.len(),
                    path.display()
                );
            }
        }

        let mut parser = qp::QpParser::default();
        let (mut qps, mut enc_ms, mut lags, mut lats) =
            (Vec::new(), Vec::new(), Vec::new(), Vec::new());
        let (mut encoded, mut keyframes, mut bytes, mut max_bytes, mut lag_max) =
            (0usize, 0usize, 0usize, 0usize, 0usize);
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
            lags.push(s.lag as f64);
            lag_max = lag_max.max(s.lag);
            lats.push(s.latency_ms);
            if let Some(q) = q {
                qps.push(f64::from(q));
            }
        }
        let fed = fed.iter().filter(|&&at| at >= t0 && at <= t_end).count();
        let secs = args.seconds.max(1) as f64;
        let pixels = f64::from(size.0) * f64::from(size.1);
        Ok((
            ReplayStats {
                fed,
                encoded,
                fps: encoded as f64 / secs,
                enc_avg: avg(&enc_ms),
                enc_p95: percentile(&enc_ms, 0.95),
                kbps: bytes as f64 * 8.0 / secs / 1000.0,
                bits_per_pixel: if encoded == 0 {
                    0.0
                } else {
                    bytes as f64 * 8.0 / encoded as f64 / pixels
                },
                keyframes,
                max_kb: max_bytes as f64 / 1024.0,
                qp: (!qps.is_empty()).then(|| (avg(&qps), percentile(&qps, 0.90))),
                lag_avg: avg(&lags),
                lag_max,
                lat_avg: avg(&lats),
                lat_p95: percentile(&lats, 0.95),
            },
            size,
        ))
    }

    /// Header facts of a replay run.
    struct ReplayInfo {
        recorded: usize,
        cap_avg_ms: f64,
        /// (source, destination, ms per frame), when a 720 variant ran.
        downscale: Option<(Size, Size, f64)>,
    }

    fn render_replay(args: &Args, screen: Size, info: &ReplayInfo, rows: &[ReplayRow]) -> String {
        let mut s = String::new();
        let downscale = info.downscale.map_or("n/a".to_string(), |(a, b, ms)| {
            format!("{}x{}->{}x{}: {ms:.1} ms/frame", a.0, a.1, b.0, b.1)
        });
        let _ = writeln!(
            s,
            "Screen {}x{}, recorded {} frames @ {} fps (avg capture {:.1} ms), downscale {}, \
             bitrate {} kbit/s, fps cfg {}, seconds {} (+{} s warm-up)\n",
            screen.0,
            screen.1,
            info.recorded,
            args.replay_fps,
            info.cap_avg_ms,
            downscale,
            args.bitrate,
            args.fps,
            args.seconds,
            WARMUP.as_secs()
        );
        s.push_str(
            "| variant | size | fed | encoded | fps | encode ms avg/p95 | kbit/s \
             | bits/pixel/frame | keyframes | max frame kB | QP avg/p90 \
             | lag frames avg/max | latency ms avg/p95 |\n",
        );
        s.push_str("|---|---|---:|---:|---:|---|---:|---:|---:|---:|---|---|---|\n");
        for row in rows {
            let size = format!("{}x{}", row.size.0, row.size.1);
            match &row.result {
                Ok(st) => {
                    let qp = st
                        .qp
                        .map_or("-".to_string(), |(a, p)| format!("{a:.1} / {p:.0}"));
                    let _ = writeln!(
                        s,
                        "| {} | {} | {} | {} | {:.1} | {:.1} / {:.1} | {:.0} | {:.3} | {} | {:.1} \
                         | {} | {:.1} / {} | {:.1} / {:.1} |",
                        row.name,
                        size,
                        st.fed,
                        st.encoded,
                        st.fps,
                        st.enc_avg,
                        st.enc_p95,
                        st.kbps,
                        st.bits_per_pixel,
                        st.keyframes,
                        st.max_kb,
                        qp,
                        st.lag_avg,
                        st.lag_max,
                        st.lat_avg,
                        st.lat_p95
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

    /// Records the clip at native size, paced at `--replay-fps`.
    fn record_clip(args: &Args, screen: Size) -> Result<(Clip, f64), String> {
        let (sw, sh) = screen;
        eprintln!("recording {} frames...", args.record_frames);
        let mut grabber = Grabber::new(sw, sh, sw, sh)?;
        let period = Duration::from_secs_f64(1.0 / f64::from(args.replay_fps.max(1)));
        let mut raw: Vec<RawFrame> = Vec::with_capacity(args.record_frames);
        let mut cap_ms = Vec::new();
        let mut next = Instant::now();
        while raw.len() < args.record_frames {
            let now = Instant::now();
            if next > now {
                thread::sleep(next - now);
            }
            next = next.max(Instant::now()) + period;
            let t0 = Instant::now();
            raw.push(grabber.grab()?);
            cap_ms.push(t0.elapsed().as_secs_f64() * 1000.0);
        }
        let frames = raw
            .into_iter()
            .filter_map(|f| match f {
                RawFrame::Bgra { data, .. } => Some(data),
                RawFrame::Nv12 { .. } => None,
            })
            .collect();
        eprintln!("recorded, avg capture {:.1} ms", avg(&cap_ms));
        Ok((
            Clip {
                width: sw,
                height: sh,
                frames,
            },
            avg(&cap_ms),
        ))
    }

    fn replay_main(args: &Args, screen: Size) -> String {
        let (sw, sh) = screen;
        let all = replay_variants();
        for name in &args.only {
            if !all.iter().any(|v| v.name == name) {
                eprintln!("mf_bench: unknown variant {name:?}");
                std::process::exit(2);
            }
        }
        let chosen: Vec<&Variant> = all
            .iter()
            .filter(|v| args.only.is_empty() || args.only.iter().any(|n| n == v.name))
            .collect();
        if args.record_frames == 0 {
            eprintln!("mf_bench: --record-frames must be > 0");
            std::process::exit(2);
        }
        let (clip, cap_avg_ms) = match record_clip(args, screen) {
            Ok(v) => v,
            Err(err) => {
                eprintln!("mf_bench: recording failed: {err}");
                std::process::exit(1);
            }
        };
        let recorded = clip.frames.len();

        // Pre-scaled / pre-cropped clips, outside the encoder measurement.
        let native_size = variant_size(Res::Native, screen);
        let native = if native_size == (sw, sh) {
            clip
        } else {
            Clip {
                width: native_size.0,
                height: native_size.1,
                frames: clip
                    .frames
                    .iter()
                    .map(|f| {
                        crop_bgra(
                            f,
                            sw as usize,
                            native_size.0 as usize,
                            native_size.1 as usize,
                        )
                    })
                    .collect(),
            }
        };
        let small_size = variant_size(Res::P720, screen);
        let mut downscale = None;
        let small = if chosen.iter().any(|v| matches!(v.res, Res::P720)) {
            eprintln!("downscaling to {}x{}...", small_size.0, small_size.1);
            let t0 = Instant::now();
            // Downscale from the native recording (the crop keeps the origin).
            let frames: Vec<Vec<u8>> = native
                .frames
                .iter()
                .map(|f| {
                    scale::downscale_bgra(
                        f,
                        native_size.0 as usize,
                        native_size.1 as usize,
                        small_size.0 as usize,
                        small_size.1 as usize,
                    )
                })
                .collect();
            let ms = t0.elapsed().as_secs_f64() * 1000.0 / recorded as f64;
            downscale = Some((native_size, small_size, ms));
            Some(Arc::new(Clip {
                width: small_size.0,
                height: small_size.1,
                frames,
            }))
        } else {
            None
        };
        let native = Arc::new(native);

        let mut rows = Vec::new();
        for var in chosen {
            let (clip, size) = match (var.res, &small) {
                (Res::P720, Some(small)) => (small.clone(), small_size),
                _ => (native.clone(), native_size),
            };
            let row = match run_replay(args, var, clip, size) {
                Ok((stats, size)) => ReplayRow {
                    name: var.name,
                    size,
                    result: Ok(stats),
                },
                Err((err, size)) => {
                    eprintln!("{}: {err}", var.name);
                    ReplayRow {
                        name: var.name,
                        size,
                        result: Err(err),
                    }
                }
            };
            rows.push(row);
        }
        let info = ReplayInfo {
            recorded,
            cap_avg_ms,
            downscale,
        };
        render_replay(args, screen, &info, &rows)
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

        if args.mode == Mode::Replay {
            let report = replay_main(&args, (sw, sh));
            print!("{report}");
            if let Err(err) = std::fs::write(&args.out, &report) {
                eprintln!("mf_bench: cannot write {}: {err}", args.out);
                std::process::exit(1);
            }
            return;
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
