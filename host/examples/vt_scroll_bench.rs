//! Measurement tool: how does the VideoToolbox encoder behave on scrolling
//! text after an idle period? Builds a procedural text page, feeds it to a
//! fresh VT session per variant in real time (idle: one frame / 500 ms,
//! scroll: 30 fps), parses slice QP from the Annex-B output and prints one
//! Markdown table. Not used by the production host.
//!
//! `cargo run --release -p rcdesk-host --example vt_scroll_bench -- --help`

#[path = "common/h264_qp.rs"]
mod qp;

#[cfg(target_os = "macos")]
mod mac {
    use std::time::{Duration, Instant};

    use clap::Parser;
    use rcdesk_host::capture::RawFrame;
    use rcdesk_host::encode::videotoolbox::{VideoToolboxEncoder, VtProfile, VtTuning};
    use rcdesk_host::encode::{Encoder, EncoderConfig};

    use super::qp;

    // ------------------------------------------------------------------
    // Procedural text page
    // ------------------------------------------------------------------
    const W: usize = 1920;
    const H: usize = 1080;

    struct Rng(u64);
    impl Rng {
        fn next(&mut self) -> u64 {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            self.0
        }
        fn below(&mut self, n: u64) -> u64 {
            self.next() % n
        }
    }

    pub struct Page {
        height: usize,
        y: Vec<u8>,
        uv: Vec<u8>,
    }

    impl Page {
        fn rect(
            &mut self,
            x: usize,
            y: usize,
            w: usize,
            h: usize,
            luma: u8,
            cbcr: Option<(u8, u8)>,
        ) {
            for yy in y..(y + h).min(self.height) {
                for xx in x..(x + w).min(W) {
                    self.y[yy * W + xx] = luma;
                    if let Some((cb, cr)) = cbcr {
                        let i = (yy / 2) * W + (xx / 2) * 2;
                        self.uv[i] = cb;
                        self.uv[i + 1] = cr;
                    }
                }
            }
        }

        /// White page with glyph-like strokes: 18 px line pitch, ~11 px
        /// glyphs on a 7 px pitch, random words, rare coloured headings.
        pub fn generate(height: usize) -> Self {
            let mut p = Page {
                height,
                y: vec![235; W * height],
                uv: vec![128; W * (height / 2)],
            };
            let mut rng = Rng(0x9E37_79B9_7F4A_7C15);
            let mut line = 0usize;
            let mut y0 = 12usize;
            while y0 + 24 < height {
                let heading = line.is_multiple_of(29);
                let (gh, pitch, luma, color) = if heading {
                    (18, 11, 40u8, Some((200u8, 90u8)))
                } else if rng.below(12) == 0 {
                    (11, 7, 60, Some((190, 110))) // link-ish
                } else {
                    (11, 7, 28, None)
                };
                let mut x = 80usize;
                let right = W - 80;
                let last_word = if rng.below(5) == 0 { right / 2 } else { right };
                while x + 9 * pitch < last_word {
                    let letters = 2 + rng.below(8) as usize;
                    for _ in 0..letters {
                        let strokes = 2 + rng.below(3);
                        for _ in 0..strokes {
                            match rng.below(3) {
                                0 => {
                                    let sx = x + rng.below(4) as usize;
                                    let sh = gh / 2 + rng.below(gh as u64 / 2) as usize;
                                    p.rect(sx, y0 + gh - sh, 2, sh, luma, color);
                                }
                                1 => {
                                    let sy = y0 + rng.below(gh as u64 - 1) as usize;
                                    p.rect(x, sy, 5, 2, luma, color);
                                }
                                _ => {
                                    let sy = y0 + gh / 3 + rng.below(gh as u64 / 3) as usize;
                                    p.rect(x + 1, sy, 3, 3, luma, color);
                                }
                            }
                        }
                        x += pitch;
                    }
                    x += 10;
                }
                y0 += if heading { 30 } else { 18 };
                line += 1;
            }
            p
        }

        /// NV12 frame of the 1920x1080 window starting at (even) row `off`.
        pub fn window(&self, off: usize) -> RawFrame {
            let off = off & !1;
            assert!(off + H <= self.height);
            RawFrame::Nv12 {
                width: W as u32,
                height: H as u32,
                y: self.y[off * W..(off + H) * W].to_vec(),
                y_stride: W,
                uv: self.uv[(off / 2) * W..(off / 2 + H / 2) * W].to_vec(),
                uv_stride: W,
                ts: Instant::now(),
            }
        }
    }

    // ------------------------------------------------------------------
    // Variants
    // ------------------------------------------------------------------
    struct Variant {
        name: &'static str,
        tuning: VtTuning,
        max_qp: Option<u8>,
    }

    fn variants() -> Vec<Variant> {
        // Production tuning (DataRateLimits 1.0x / 1 s, average = target): the
        // other variants are expressed relative to it.
        let d = VtTuning::default();
        let v = |name, tuning, max_qp| Variant {
            name,
            tuning,
            max_qp,
        };
        vec![
            v("base", d, None),
            v(
                "no-drl",
                VtTuning {
                    data_rate_limit: None,
                    ..d
                },
                None,
            ),
            v(
                "drl-1.5",
                VtTuning {
                    data_rate_limit: Some(1.5),
                    ..d
                },
                None,
            ),
            v(
                "nodrl-avg0.6",
                VtTuning {
                    data_rate_limit: None,
                    avg_factor: 0.6,
                    ..d
                },
                None,
            ),
            v(
                "nodrl-avg0.75",
                VtTuning {
                    data_rate_limit: None,
                    avg_factor: 0.75,
                    ..d
                },
                None,
            ),
            v(
                "drl1.5-w3",
                VtTuning {
                    data_rate_limit: Some(1.5),
                    drl_window_s: 3.0,
                    ..d
                },
                None,
            ),
            v(
                "drl2-w3",
                VtTuning {
                    data_rate_limit: Some(2.0),
                    drl_window_s: 3.0,
                    ..d
                },
                None,
            ),
            v(
                "drl1.2-w2",
                VtTuning {
                    data_rate_limit: Some(1.2),
                    drl_window_s: 2.0,
                    ..d
                },
                None,
            ),
            v(
                "drl1.5-w3-avg0.75",
                VtTuning {
                    data_rate_limit: Some(1.5),
                    drl_window_s: 3.0,
                    avg_factor: 0.75,
                    ..d
                },
                None,
            ),
            v("qp36", d, Some(36)),
            v(
                "main",
                VtTuning {
                    profile: VtProfile::Main,
                    ..d
                },
                None,
            ),
            v(
                "high",
                VtTuning {
                    profile: VtProfile::High,
                    ..d
                },
                None,
            ),
            v(
                "chigh",
                VtTuning {
                    profile: VtProfile::ConstrainedHigh,
                    ..d
                },
                None,
            ),
            v(
                "llrc",
                VtTuning {
                    low_latency_rc: true,
                    ..d
                },
                None,
            ),
            v(
                "llrc-high",
                VtTuning {
                    low_latency_rc: true,
                    profile: VtProfile::High,
                    ..d
                },
                None,
            ),
            v(
                "llrc-qp36",
                VtTuning {
                    low_latency_rc: true,
                    ..d
                },
                Some(36),
            ),
        ]
    }

    // ------------------------------------------------------------------
    // Run + metrics
    // ------------------------------------------------------------------
    #[derive(Parser)]
    #[command(about = "VideoToolbox scroll-after-idle encoder bench")]
    struct Args {
        /// Target bitrate, kbit/s.
        #[arg(long, default_value_t = 6000)]
        bitrate: u32,
        #[arg(long, default_value_t = 30)]
        fps: u32,
        /// Scroll speed, px per frame (rounded down to even).
        #[arg(long, default_value_t = 40)]
        speed: usize,
        /// Variant name; repeatable; none = all.
        #[arg(long = "variant")]
        variants: Vec<String>,
        #[arg(long, default_value_t = 4)]
        cycles: usize,
    }

    struct Sample {
        t: f64, // seconds since phase start
        bytes: usize,
        qp: Option<i32>,
        key: bool,
        enc_ms: f64,
    }

    #[derive(Default)]
    struct Cycle {
        idle: Vec<Sample>,
        scroll: Vec<Sample>,
        scroll_fed: usize,
    }

    fn sleep_until(t: Instant) {
        let now = Instant::now();
        if t > now {
            std::thread::sleep(t - now);
        }
    }

    fn run_variant(var: &Variant, args: &Args, page: &Page) -> Result<Vec<Cycle>, String> {
        let cfg = EncoderConfig {
            width: W as u32,
            height: H as u32,
            fps: args.fps,
            bitrate_kbps: args.bitrate,
            keyframe_interval_frames: args.fps.max(1) * 2,
            max_qp: var.max_qp,
        };
        eprintln!("# variant {}", var.name);
        let mut enc = VideoToolboxEncoder::new_tuned(cfg, var.tuning).map_err(|e| e.to_string())?;
        let mut parser = qp::QpParser::default();
        let speed = args.speed & !1;
        let frame_dt = Duration::from_secs_f64(1.0 / f64::from(args.fps));
        let scroll_frames = (2.0 * f64::from(args.fps)) as usize;
        let mut off = 0usize;
        let mut first = true;
        let mut cycles = Vec::new();

        for _ in 0..args.cycles {
            let mut cy = Cycle::default();
            let mut feed = |frame: &mut RawFrame,
                            t0: Instant,
                            sink: &mut Vec<Sample>,
                            enc: &mut VideoToolboxEncoder,
                            first: &mut bool|
             -> Result<(), String> {
                let now = Instant::now();
                frame.set_ts(now);
                let t = now.duration_since(t0).as_secs_f64();
                let started = Instant::now();
                let out = enc
                    .encode(frame, std::mem::take(first))
                    .map_err(|e| e.to_string())?;
                let enc_ms = started.elapsed().as_secs_f64() * 1e3;
                if let Some(out) = out {
                    let qp = parser.frame_qp(&out.data);
                    sink.push(Sample {
                        t,
                        bytes: out.data.len(),
                        qp,
                        key: out.keyframe,
                        enc_ms,
                    });
                }
                Ok(())
            };

            // Idle: same frame every 500 ms for 3 s.
            let t0 = Instant::now();
            for i in 0..6u32 {
                let mut f = page.window(off);
                sleep_until(t0 + Duration::from_millis(500) * i);
                feed(&mut f, t0, &mut cy.idle, &mut enc, &mut first)?;
            }
            sleep_until(t0 + Duration::from_secs(3));

            // Scroll: `fps` frames/s for 2 s.
            let t0 = Instant::now();
            for i in 0..scroll_frames {
                off += speed;
                let mut f = page.window(off);
                sleep_until(t0 + frame_dt * i as u32);
                feed(&mut f, t0, &mut cy.scroll, &mut enc, &mut first)?;
                cy.scroll_fed += 1;
            }
            cycles.push(cy);
        }
        Ok(cycles)
    }

    fn kbps(samples: &[Sample], window: f64, dur: f64) -> f64 {
        let bytes: usize = samples
            .iter()
            .filter(|s| s.t < window)
            .map(|s| s.bytes)
            .sum();
        bytes as f64 * 8.0 / dur / 1000.0
    }

    fn mean(v: &[f64]) -> f64 {
        if v.is_empty() {
            f64::NAN
        } else {
            v.iter().sum::<f64>() / v.len() as f64
        }
    }

    fn row(name: &str, cycles: &[Cycle]) -> String {
        let per = |window: f64, c: &Cycle| kbps(&c.scroll, window, window);
        let c1 = &cycles[0];
        let avg = |w: f64| mean(&cycles.iter().map(|c| per(w, c)).collect::<Vec<_>>());
        let idle = mean(
            &cycles
                .iter()
                .map(|c| kbps(&c.idle, 3.0, 3.0))
                .collect::<Vec<_>>(),
        );
        let all: Vec<&Sample> = cycles.iter().flat_map(|c| c.scroll.iter()).collect();
        let fed: usize = cycles.iter().map(|c| c.scroll_fed).sum();
        let bpf = all.iter().map(|s| s.bytes).sum::<usize>() as f64 / all.len().max(1) as f64;
        let qps: Vec<f64> = all.iter().filter_map(|s| s.qp.map(f64::from)).collect();
        let qps_early: Vec<f64> = all
            .iter()
            .filter(|s| s.t < 0.5)
            .filter_map(|s| s.qp.map(f64::from))
            .collect();
        let qp_known = qps.len() == all.len() && !qps.is_empty();
        let qp_avg = if qp_known {
            format!("{:.1}", mean(&qps))
        } else {
            "?".into()
        };
        let qp_early = if qps_early.is_empty() {
            "?".to_string()
        } else {
            format!("{:.1}", mean(&qps_early))
        };
        let qp_max = if qp_known {
            format!("{:.0}", qps.iter().cloned().fold(f64::MIN, f64::max))
        } else {
            "?".into()
        };
        let keys = all.iter().filter(|s| s.key).count();
        let enc_ms = mean(&all.iter().map(|s| s.enc_ms).collect::<Vec<_>>());
        format!(
            "| {name} | {idle:.0} | {:.0} | {:.0} | {:.0} | {:.0} | {:.0} | {:.0} | {bpf:.0} | {qp_avg} | {qp_early} | {qp_max} | {keys} | {} / {fed} | {enc_ms:.1} |",
            per(0.5, c1),
            per(1.0, c1),
            per(2.0, c1),
            avg(0.5),
            avg(1.0),
            avg(2.0),
            all.len(),
        )
    }

    pub fn main() {
        tracing_subscriber::fmt()
            .with_env_filter(
                tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| {
                    tracing_subscriber::EnvFilter::new(
                        "warn,rcdesk_host::encode::videotoolbox=debug",
                    )
                }),
            )
            .with_writer(std::io::stderr)
            .init();
        let args = Args::parse();
        let all = variants();
        let wanted: Vec<&Variant> = if args.variants.is_empty() {
            all.iter().collect()
        } else {
            args.variants
                .iter()
                .map(|n| {
                    all.iter().find(|v| v.name == n).unwrap_or_else(|| {
                        let names: Vec<_> = all.iter().map(|v| v.name).collect();
                        panic!("unknown variant {n}; known: {names:?}")
                    })
                })
                .collect()
        };

        let page_h = H + args.speed * (2 * args.fps as usize) * args.cycles + 8;
        eprintln!("# generating {W}x{page_h} page");
        let page = Page::generate(page_h);

        println!(
            "bitrate {} kbit/s, {} fps, speed {} px/frame, {} cycles (idle 3 s @ 2 fps, scroll 2 s)\n",
            args.bitrate, args.fps, args.speed, args.cycles
        );
        println!(
            "| variant | idle kbps | c1 0.5s | c1 1s | c1 2s | avg 0.5s | avg 1s | avg 2s | B/frame | QP avg | QP avg 0.5s | QP max | KF | frames out/fed | enc ms |"
        );
        println!("|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|");
        for v in wanted {
            match run_variant(v, &args, &page) {
                Ok(c) => println!("{}", row(v.name, &c)),
                Err(e) => println!("| {} | ERR: {e} |", v.name),
            }
        }
    }
}

#[cfg(target_os = "macos")]
fn main() {
    mac::main();
}

#[cfg(not(target_os = "macos"))]
fn main() {
    println!("macOS only");
}
