//! Looks at the background effect on a real picture with the real model: the classic chain and the
//! high-quality one side by side, written as PNG files, plus the time each blend takes.
//!
//!   bg_compare <picture.png> <model.onnx> <out-dir> [width height]
//!
//! (The pictures Windows ships, e.g. portrait-lighting.png under SystemApps, do for a test.)

use std::time::{Duration, Instant};

use niwcam_fx::background_hq::{blend_background_hq, refine_alpha, HqScratch};
use niwcam_fx::matte_segmenter::MatteSegmenter;
use niwcam_fx::segmenter::{blend_background, BackgroundScratch, Segmenter, K_SIZE};
use niwcam_fx::still_picture::{load_image_fill, save_png};
use windows::Win32::System::Com::{CoInitializeEx, COINIT_MULTITHREADED};

fn mask_for(nv12: &[u8], w: u32, h: u32, model: &str, quality: bool) -> Vec<u8> {
    let seg = Segmenter::new();
    seg.set_model(model);
    seg.set_quality(quality);
    let (y, uv) = nv12.split_at(w as usize * h as usize);
    let mut mask = Vec::new();
    let mut now = 0.0;
    // A few looks of the same still picture: the temporal smoothing has settled by the last.
    for _ in 0..8 {
        let start = Instant::now();
        while !seg.due(now) && start.elapsed() < Duration::from_secs(5) {
            std::thread::sleep(Duration::from_millis(5));
            now += 5.0;
        }
        seg.offer(y, uv, w as usize, w, h, now);
        let t = Instant::now();
        while !seg.take_mask(&mut mask) && t.elapsed() < Duration::from_secs(8) {
            std::thread::sleep(Duration::from_millis(5));
        }
        now += 70.0;
    }
    println!(
        "{} mask: state {} infer {:.1} ms",
        if quality { "hq     " } else { "classic" },
        seg.state(),
        seg.infer_ms()
    );
    mask
}

/// The matting model's soft alpha and its size (a few looks of the still picture: smoothing settles).
fn matte_mask(nv12: &[u8], w: u32, h: u32, model: &str) -> (Vec<u8>, u32, u32) {
    let seg = MatteSegmenter::new();
    seg.set_model(model);
    let (y, uv) = nv12.split_at(w as usize * h as usize);
    let (mut mask, mut size) = (Vec::new(), (0, 0));
    let mut now = 0.0;
    for _ in 0..8 {
        let start = Instant::now();
        while !seg.due(now) && start.elapsed() < Duration::from_secs(5) {
            std::thread::sleep(Duration::from_millis(5));
            now += 5.0;
        }
        seg.offer(y, uv, w as usize, w, h, now);
        let t = Instant::now();
        loop {
            if let Some(s) = seg.take_mask(&mut mask) {
                size = s;
                break;
            }
            if t.elapsed() > Duration::from_secs(8) {
                break;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        now += 60.0;
    }
    println!(
        "matte   mask: state {} infer {:.1} ms, {}x{}",
        seg.state(),
        seg.infer_ms(),
        size.0,
        size.1
    );
    (mask, size.0, size.1)
}

/// A replacement background: warm diagonal gradient with a few soft rectangles.
fn backdrop(w: u32, h: u32) -> Vec<u8> {
    let (wu, hu) = (w as usize, h as usize);
    let mut out = vec![128u8; wu * hu * 3 / 2];
    for j in 0..hu {
        for i in 0..wu {
            let g = (i as f32 / wu as f32 * 0.5 + j as f32 / hu as f32 * 0.5) * 120.0 + 70.0;
            let rect = ((i / 160 + j / 160) % 2) as f32 * 14.0;
            out[j * wu + i] = (g + rect) as u8;
        }
    }
    for j in 0..hu / 2 {
        for i in 0..wu / 2 {
            out[wu * hu + j * wu + 2 * i] = (150.0 - i as f32 / wu as f32 * 60.0) as u8; // U
            out[wu * hu + j * wu + 2 * i + 1] = (110.0 + j as f32 / hu as f32 * 80.0) as u8;
            // V
        }
    }
    out
}

fn main() {
    let a: Vec<String> = std::env::args().collect();
    if a.len() < 4 {
        eprintln!("usage: bg_compare <picture.png> <model.onnx> <out-dir> [width height]");
        std::process::exit(2);
    }
    let (w, h) = (
        a.get(4).and_then(|v| v.parse().ok()).unwrap_or(1280u32),
        a.get(5).and_then(|v| v.parse().ok()).unwrap_or(720u32),
    );
    // SAFETY: plain COM start-up of this thread (WIC and Windows ML need it).
    unsafe {
        CoInitializeEx(None, COINIT_MULTITHREADED)
            .ok()
            .expect("COM")
    };
    let mut nv12 = Vec::new();
    assert!(
        load_image_fill(&a[1], w, h, &mut nv12),
        "cannot read {}",
        a[1]
    );
    let out = &a[3];
    std::fs::create_dir_all(out).ok();
    let _ = save_png(&format!("{out}/original.png"), &nv12, w, h);

    let classic_mask_u8 = mask_for(&nv12, w, h, &a[2], false);
    let hq_prob = mask_for(&nv12, w, h, &a[2], true);
    let matte = std::env::var("NIWCAM_MATTE_MODEL")
        .ok()
        .map(|m| matte_mask(&nv12, w, h, &m));
    let image = backdrop(w, h);
    let pitch = w as usize;
    let split = w as usize * h as usize;

    for (name, strength, image) in [
        ("blur60", 60, None),
        ("blur100", 100, None),
        ("image", 0, Some(&image)),
    ] {
        let mut c = nv12.clone();
        let mut hq = nv12.clone();
        let mut bs = BackgroundScratch::default();
        let mut hs = HqScratch::default();
        let (cy, cuv) = c.split_at_mut(split);
        blend_background(
            cy,
            cuv,
            pitch,
            w,
            h,
            &classic_mask_u8,
            true,
            strength,
            image.map(|v| v.as_slice()),
            &mut bs,
            false,
        );
        let (hy, huv) = hq.split_at_mut(split);
        blend_background_hq(
            hy,
            huv,
            pitch,
            w,
            h,
            &hq_prob,
            K_SIZE as usize,
            K_SIZE as usize,
            strength,
            image.map(|v| v.as_slice()),
            &mut hs,
        );
        if let Some((mm, mw, mh)) = &matte {
            let mut m = nv12.clone();
            let (my, muv) = m.split_at_mut(split);
            blend_background_hq(
                my,
                muv,
                pitch,
                w,
                h,
                mm,
                *mw as usize,
                *mh as usize,
                strength,
                image.map(|v| v.as_slice()),
                &mut hs,
            );
            let _ = save_png(&format!("{out}/matte-{name}.png"), &m, w, h);
        }
        let _ = save_png(&format!("{out}/classic-{name}.png"), &c, w, h);
        let _ = save_png(&format!("{out}/hq-{name}.png"), &hq, w, h);
        // timing (the second image of each pair is made from the same input)
        let n = 40;
        let t = Instant::now();
        for _ in 0..n {
            let mut c2 = nv12.clone();
            let (cy, cuv) = c2.split_at_mut(split);
            blend_background(
                cy,
                cuv,
                pitch,
                w,
                h,
                &classic_mask_u8,
                true,
                strength,
                image.map(|v| v.as_slice()),
                &mut bs,
                false,
            );
        }
        let classic_ms = t.elapsed().as_secs_f64() * 1000.0 / n as f64;
        let t = Instant::now();
        for _ in 0..n {
            let mut h2 = nv12.clone();
            let (hy, huv) = h2.split_at_mut(split);
            blend_background_hq(
                hy,
                huv,
                pitch,
                w,
                h,
                &hq_prob,
                K_SIZE as usize,
                K_SIZE as usize,
                strength,
                image.map(|v| v.as_slice()),
                &mut hs,
            );
        }
        let hq_ms = t.elapsed().as_secs_f64() * 1000.0 / n as f64;
        let t = Instant::now();
        for _ in 0..n {
            refine_alpha(
                &nv12[..split],
                pitch,
                w,
                h,
                &hq_prob,
                K_SIZE as usize,
                K_SIZE as usize,
                &mut hs,
            );
        }
        println!(
            "{name:8} refine_alpha alone {:.2} ms",
            t.elapsed().as_secs_f64() * 1000.0 / n as f64
        );
        let t = Instant::now();
        for _ in 0..n {
            let _ = nv12.clone();
        }
        let copy_ms = t.elapsed().as_secs_f64() * 1000.0 / n as f64;
        println!("{name:8} {w}x{h}: classic {:.2} ms   hq {:.2} ms   (frame copy {:.2} ms is inside both)", classic_ms, hq_ms, copy_ms);
    }
}
