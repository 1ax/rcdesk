// Временный эксперимент задачи 017 (не компилируется в составе крейта).
// Вставить в конец host/src/encode/videotoolbox.rs и запустить:
//   cargo test -p rcdesk-host --lib exp017 -- --ignored --nocapture

#[cfg(test)]
mod exp017 {
    use super::*;
    use openh264::decoder::Decoder;
    use openh264::formats::YUVSource;

    const W: u32 = 1920;
    const H: u32 = 1080;

    fn text_frame(seed: u64, blank: bool) -> RawFrame {
        let w = W as usize;
        let h = H as usize;
        let mut y = vec![235u8; w * h];
        if !blank {
            let mut s = seed;
            let mut rnd = || {
                s ^= s << 13;
                s ^= s >> 7;
                s ^= s << 17;
                s
            };
            // rows of "glyphs": 11px tall lines, 1-2px strokes
            let mut row = 40;
            while row + 14 < h - 40 {
                let mut x = 200;
                while x < w - 200 {
                    let gw = 5 + (rnd() % 4) as usize;
                    for _ in 0..4 {
                        let vx = x + (rnd() as usize % gw);
                        let vy0 = row + (rnd() as usize % 5);
                        let len = 3 + rnd() as usize % 8;
                        let horiz = rnd() % 2 == 0;
                        for k in 0..len {
                            let (px, py) = if horiz { (vx + k.min(gw), vy0 + 3) } else { (vx, vy0 + k) };
                            if px < w && py < h {
                                y[py * w + px] = 30;
                            }
                        }
                    }
                    x += gw + 2 + if rnd() % 6 == 0 { 6 } else { 0 };
                }
                row += 22;
            }
        }
        RawFrame::Nv12 {
            width: W,
            height: H,
            y,
            y_stride: w,
            uv: vec![128u8; w * h / 2],
            uv_stride: w,
            ts: Instant::now(),
        }
    }

    fn psnr(src: &RawFrame, dec: &impl YUVSource) -> f64 {
        let RawFrame::Nv12 { y, .. } = src else { unreachable!() };
        let (ys, _, _) = dec.strides();
        let dy = dec.y();
        let w = W as usize;
        let mut se = 0f64;
        for r in 0..H as usize {
            for c in 0..w {
                let d = y[r * w + c] as f64 - dy[r * ys + c] as f64;
                se += d * d;
            }
        }
        let mse = se / (W as f64 * H as f64);
        10.0 * (255.0f64 * 255.0 / mse).log10()
    }

    fn step(enc: &mut VideoToolboxEncoder, dec: &mut Decoder, f: &RawFrame, key: bool, label: &str) {
        let mut f2 = f.clone();
        f2.set_ts(Instant::now());
        let out = enc.encode(&f2, key).unwrap();
        match out {
            Some(e) => {
                let size = e.data.len();
                let kf = e.keyframe;
                let p = dec.decode(&e.data).unwrap().map(|y| psnr(f, &y));
                println!("{label:<28} size={size:>7} key={kf} psnr={p:?}");
            }
            None => println!("{label:<28} no output"),
        }
        std::thread::sleep(std::time::Duration::from_millis(66));
    }

    #[test]
    #[ignore]
    fn exp_static_refinement() {
        let text = text_frame(0x1234_5678, false);
        let blank = text_frame(0, true);
        for (b0, f0) in [(1000u32, 8u32), (6000, 15)] {
            println!("=== start rate {b0}/{f0} ===");
            let mut enc = VideoToolboxEncoder::new(EncoderConfig {
                width: W,
                height: H,
                fps: f0,
                bitrate_kbps: b0,
                keyframe_interval_frames: 30,
                max_qp: None,
            })
            .unwrap();
            let mut dec = Decoder::new().unwrap();
            step(&mut enc, &mut dec, &blank, true, "blank key");
            for i in 0..3 {
                step(&mut enc, &mut dec, &blank, false, &format!("blank rep {i}"));
            }
            step(&mut enc, &mut dec, &text, false, "TEXT appears (P)");
            for i in 0..4 {
                step(&mut enc, &mut dec, &text, false, &format!("text rep {i} same rate"));
            }
            enc.set_rate(RateTarget { bitrate_kbps: 6000, fps: 15 }).unwrap();
            for i in 0..8 {
                step(&mut enc, &mut dec, &text, false, &format!("text rep {i} @6000"));
            }
            step(&mut enc, &mut dec, &text, true, "text FORCED KEY @6000");
            for i in 0..3 {
                step(&mut enc, &mut dec, &text, false, &format!("after key rep {i}"));
            }
        }
        for qp in [Some(28u8), Some(24)] {
            println!("=== 1000/8 with max_qp {qp:?} ===");
            let mut enc = VideoToolboxEncoder::new(EncoderConfig {
                width: W, height: H, fps: 8, bitrate_kbps: 1000, keyframe_interval_frames: 30, max_qp: qp,
            }).unwrap();
            let mut dec = Decoder::new().unwrap();
            step(&mut enc, &mut dec, &blank, true, "blank key");
            step(&mut enc, &mut dec, &text, false, "TEXT appears (P)");
            for i in 0..4 {
                step(&mut enc, &mut dec, &text, false, &format!("text rep {i}"));
            }
            step(&mut enc, &mut dec, &text, true, "text FORCED KEY");
        }
    }

    fn shifted(f: &RawFrame, dy: usize) -> RawFrame {
        let RawFrame::Nv12 { y, uv, .. } = f else { unreachable!() };
        let w = W as usize;
        let mut ny = vec![235u8; y.len()];
        ny[..y.len() - dy * w].copy_from_slice(&y[dy * w..]);
        RawFrame::Nv12 { width: W, height: H, y: ny, y_stride: w, uv: uv.clone(), uv_stride: w, ts: Instant::now() }
    }

    fn set_qp(enc: &VideoToolboxEncoder, qp: i32) {
        unsafe {
            soft_set_property(
                &enc.session,
                "MaxAllowedFrameQP",
                kVTCompressionPropertyKey_MaxAllowedFrameQP,
                CFNumber::new_i32(qp).as_ref(),
            );
        }
    }

    #[test]
    #[ignore]
    fn exp_scroll_and_live_qp() {
        let text = text_frame(0x1234_5678, false);
        let blank = text_frame(0, true);
        for qp in [None, Some(28u8)] {
            for (b, f) in [(6000u32, 15u32), (1000, 8)] {
                println!("=== scroll {b}/{f} qp {qp:?} ===");
                let mut enc = VideoToolboxEncoder::new(EncoderConfig { width: W, height: H, fps: f, bitrate_kbps: b, keyframe_interval_frames: 300, max_qp: qp }).unwrap();
                let mut dec = Decoder::new().unwrap();
                step(&mut enc, &mut dec, &text, true, "key");
                let mut total = 0usize;
                for i in 1..12 {
                    let fr = shifted(&text, i * 20);
                    let mut f2 = fr.clone(); f2.set_ts(Instant::now());
                    if let Some(e) = enc.encode(&f2, false).unwrap() {
                        total += e.data.len();
                        let p = dec.decode(&e.data).unwrap().map(|y| psnr(&fr, &y));
                        println!("scroll {i:>2} size={:>7} psnr={p:?}", e.data.len());
                    } else { println!("scroll {i} none"); }
                    std::thread::sleep(std::time::Duration::from_millis(1000 / f as u64));
                }
                println!("scroll total {total}");
            }
        }
        println!("=== live qp: 1000/8 no cap, then cap 26 at same rate ===");
        let mut enc = VideoToolboxEncoder::new(EncoderConfig { width: W, height: H, fps: 8, bitrate_kbps: 1000, keyframe_interval_frames: 300, max_qp: None }).unwrap();
        let mut dec = Decoder::new().unwrap();
        step(&mut enc, &mut dec, &blank, true, "blank key");
        step(&mut enc, &mut dec, &text, false, "TEXT appears (P)");
        step(&mut enc, &mut dec, &text, false, "rep");
        step(&mut enc, &mut dec, &text, false, "rep");
        set_qp(&enc, 26);
        for i in 0..6 { step(&mut enc, &mut dec, &text, false, &format!("rep after cap26 {i}")); }
        set_qp(&enc, 51);
        step(&mut enc, &mut dec, &text, false, "rep after uncap");
        println!("=== live qp: 6000/15 page, rates up ===");
        let mut enc = VideoToolboxEncoder::new(EncoderConfig { width: W, height: H, fps: 15, bitrate_kbps: 6000, keyframe_interval_frames: 300, max_qp: None }).unwrap();
        let mut dec = Decoder::new().unwrap();
        step(&mut enc, &mut dec, &blank, true, "blank key");
        step(&mut enc, &mut dec, &text, false, "TEXT appears (P)");
        for i in 0..4 { step(&mut enc, &mut dec, &text, false, &format!("rep {i}")); }
        set_qp(&enc, 22);
        for i in 0..5 { step(&mut enc, &mut dec, &text, false, &format!("rep after cap22 {i}")); }
    }
}

