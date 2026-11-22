//! Rust side of the rust/fx cross-check: the same cases as `xcheck/fx_cpp.cpp`, with the output
//! in the same format, so `xcheck/compare.py` can diff the two directories.
//!   fx_rust still  <indir> <outdir>
//!   fx_rust blend  <indir> <outdir>
//!   fx_rust seg    <indir> <outdir> <model.onnx>
//!   fx_rust face   <indir> <outdir>
//!   fx_rust closewatch <indir> <outdir> <hwnd>   (see fx_cpp windowhost)
//!   fx_rust window <indir> <outdir> [x y]
#![allow(clippy::too_many_arguments)]

mod window;

use std::fmt::Write as _;
use std::fs;
use std::io::Write as _;
use std::time::{Duration, Instant};

use niwcam_fx::face_detect::FaceDetector;
use niwcam_fx::segmenter::{blend_background, BackgroundScratch, Segmenter};
use niwcam_fx::still_picture;
use windows::Win32::System::Com::{CoInitializeEx, CoUninitialize, COINIT_MULTITHREADED};

pub struct Ctx {
    pub indir: String,
    pub outdir: String,
    pub results: fs::File,
}

fn read(path: &str) -> Vec<u8> {
    fs::read(path).unwrap_or_default()
}

fn fnv(d: &[u8]) -> u64 {
    let mut h: u64 = 1469598103934665603;
    for &b in d {
        h = (h ^ b as u64).wrapping_mul(1099511628211);
    }
    h
}

impl Ctx {
    fn input(&self, name: &str) -> Vec<u8> {
        read(&format!("{}\\{}", self.indir, name))
    }
    fn text(&self, name: &str) -> String {
        String::from_utf8_lossy(&self.input(name)).into_owned()
    }
    pub fn line(&mut self, s: &str) {
        let _ = writeln!(self.results, "{s}");
    }
    fn result(&mut self, name: &str, what: &str, data: Option<&[u8]>) {
        let mut s = format!("{name} {what}");
        if let Some(d) = data {
            let _ = fs::write(format!("{}\\{}.nv12", self.outdir, name), d);
            let _ = write!(s, " bytes={} fnv={:016x}", d.len(), fnv(d));
        }
        self.line(&s);
    }
}

fn ms(t0: Instant) -> f64 {
    t0.elapsed().as_secs_f64() * 1000.0
}

fn cmd_still(c: &mut Ctx) -> i32 {
    let sizes: [(u32, u32); 4] = [(1920, 1080), (1280, 720), (720, 1280), (64, 36)];
    let frame_a = c.input("frame_a_1920x1080.nv12");
    let frame_c = c.input("frame_c_640x480.nv12");
    let frame_d = c.input("frame_d_1366x768.nv12");
    let short_text = c.text("text_short.txt");
    let long_text = c.text("text_long.txt");
    let mut timing = fs::File::create(format!("{}\\timing.txt", c.outdir)).unwrap();
    for (w, h) in sizes {
        let tag = format!("_{w}x{h}");
        let mut run = |c: &mut Ctx, name: &str, text: &str, bg: Option<&[u8]>, bw: u32, bh: u32| {
            let mut out = Vec::new();
            let t0 = Instant::now();
            let ok = still_picture::render_card(text, w, h, bg, bw, bh, &mut out);
            let _ = writeln!(timing, "{}{} {:.1} ms", name, tag, ms(t0));
            c.result(
                &format!("{name}{tag}"),
                if ok { "ok" } else { "false" },
                ok.then_some(&out[..]),
            );
        };
        run(c, "card_short", &short_text, None, 0, 0);
        run(c, "card_long", &long_text, None, 0, 0);
        run(c, "card_empty", "", None, 0, 0);
        run(c, "blur_a", &short_text, Some(&frame_a), 1920, 1080);
        run(c, "blur_c_long", &long_text, Some(&frame_c), 640, 480);
        run(c, "blur_d", &short_text, Some(&frame_d), 1366, 768);
        run(c, "blur_smallbw", &short_text, Some(&frame_a), 1, 1);
        let indir = c.indir.clone();
        let mut img = |c: &mut Ctx, name: &str, file: &str| {
            let mut out = Vec::new();
            let t0 = Instant::now();
            let ok = still_picture::load_image_fill(file, w, h, &mut out);
            let _ = writeln!(timing, "{}{} {:.1} ms", name, tag, ms(t0));
            c.result(
                &format!("{name}{tag}"),
                if ok { "ok" } else { "false" },
                ok.then_some(&out[..]),
            );
        };
        img(
            c,
            "img_png",
            &format!("{indir}\\img_portrait_1000x1500.png"),
        );
        img(c, "img_jpg", &format!("{indir}\\img_wide_1600x900.jpg"));
        img(c, "img_bmp", &format!("{indir}\\img_small_332x500.bmp"));
        img(c, "img_missing", &format!("{indir}\\no_such_file.png"));
        img(c, "img_notimage", &format!("{indir}\\not_an_image.png"));
        img(c, "img_emptypath", "");
    }
    {
        let mut out = Vec::new();
        let ok = still_picture::render_card(&short_text, 1, 1080, None, 0, 0, &mut out);
        c.result("card_w1", if ok { "ok" } else { "false" }, None);
    }
    let png = format!("{}\\save_a.png", c.outdir);
    let hr = still_picture::save_png(&png, &frame_a, 1920, 1080);
    c.line(&format!("save_png hr={:08x}", hr.0 as u32));
    let hr = still_picture::save_png(
        &format!("{}\\no_such_dir\\x.png", c.outdir),
        &frame_a,
        1920,
        1080,
    );
    c.line(&format!("save_png_badpath hr={:08x}", hr.0 as u32));
    let hr = still_picture::save_png(&png, &[], 1920, 1080);
    c.line(&format!("save_png_null hr={:08x}", hr.0 as u32));
    let hr = still_picture::save_png(&png, &frame_a, 1, 1);
    c.line(&format!("save_png_tiny hr={:08x}", hr.0 as u32));
    0
}

fn cmd_blend(c: &mut Ctx) -> i32 {
    struct Frame(&'static str, u32, u32);
    let (fa, fb, fd) = (
        Frame("frame_a_1920x1080", 1920, 1080),
        Frame("frame_b_1280x720", 1280, 720),
        Frame("frame_d_1366x768", 1366, 768),
    );
    let mask_a = c.input("mask_a.u8");
    let mask_b = c.input("mask_b.u8");
    let run = |c: &mut Ctx,
               name: &str,
               f: &Frame,
               padding: u32,
               mask: &[u8],
               changed: bool,
               strength: i32,
               bg_image: Option<&str>,
               scratch: &mut BackgroundScratch,
               parallel: bool| {
        let src = c.input(&format!("{}.nv12", f.0));
        let (w, h) = (f.1 as usize, f.2 as usize);
        let pitch = w + padding as usize;
        // Planes with `padding` junk bytes at the end of each row (0xAB: must stay untouched).
        let mut buf = vec![0xABu8; pitch * h * 3 / 2];
        for y in 0..h {
            buf[y * pitch..y * pitch + w].copy_from_slice(&src[y * w..(y + 1) * w]);
        }
        for y in 0..h / 2 {
            buf[pitch * h + y * pitch..pitch * h + y * pitch + w]
                .copy_from_slice(&src[w * h + y * w..w * h + (y + 1) * w]);
        }
        let image = bg_image.map(|n| c.input(&format!("{n}.nv12")));
        let (y, uv) = buf.split_at_mut(pitch * h);
        let t0 = Instant::now();
        blend_background(
            y,
            uv,
            pitch,
            f.1,
            f.2,
            mask,
            changed,
            strength,
            image.as_deref(),
            scratch,
            parallel,
        );
        eprintln!("  blend {:<28} {:.2} ms", name, ms(t0));
        c.result(name, "ok", Some(&buf));
    };
    {
        let mut s = BackgroundScratch::default();
        run(
            c,
            "seq1_a_720p_changed",
            &fb,
            0,
            &mask_a,
            true,
            50,
            None,
            &mut s,
            false,
        );
        run(
            c,
            "seq1_b_1080p_size_changed",
            &fa,
            0,
            &mask_a,
            false,
            50,
            None,
            &mut s,
            false,
        );
        run(
            c,
            "seq1_c_1080p_stale_mask",
            &fa,
            0,
            &mask_b,
            false,
            50,
            None,
            &mut s,
            false,
        );
        run(
            c,
            "seq1_d_1080p_new_mask",
            &fa,
            0,
            &mask_b,
            true,
            50,
            None,
            &mut s,
            false,
        );
    }
    for strength in [0, 25, 100] {
        let mut s = BackgroundScratch::default();
        run(
            c,
            &format!("blur_1080p_s{strength}"),
            &fa,
            0,
            &mask_a,
            true,
            strength,
            None,
            &mut s,
            false,
        );
    }
    run(
        c,
        "blur_1366_pitch64_s30",
        &fd,
        64,
        &mask_a,
        true,
        30,
        None,
        &mut BackgroundScratch::default(),
        false,
    );
    run(
        c,
        "blur_1080p_s50_par",
        &fa,
        0,
        &mask_a,
        true,
        50,
        None,
        &mut BackgroundScratch::default(),
        true,
    );
    run(
        c,
        "blur_1366_pitch64_s30_par",
        &fd,
        64,
        &mask_a,
        true,
        30,
        None,
        &mut BackgroundScratch::default(),
        true,
    );
    run(
        c,
        "image_1080p",
        &fa,
        0,
        &mask_a,
        true,
        50,
        Some("bgimg_1920x1080"),
        &mut BackgroundScratch::default(),
        false,
    );
    run(
        c,
        "image_720p_pitch32",
        &fb,
        32,
        &mask_b,
        true,
        50,
        Some("bgimg_1280x720"),
        &mut BackgroundScratch::default(),
        false,
    );
    run(
        c,
        "image_1366_par",
        &fd,
        0,
        &mask_a,
        true,
        50,
        Some("bgimg_1366x768"),
        &mut BackgroundScratch::default(),
        true,
    );
    run(
        c,
        "bad_mask_size",
        &fb,
        0,
        &[255u8; 100],
        true,
        50,
        None,
        &mut BackgroundScratch::default(),
        false,
    );
    0
}

/// Process CPU time (kernel + user) in ms.
fn process_cpu_ms() -> f64 {
    use windows::Win32::Foundation::FILETIME;
    use windows::Win32::System::Threading::{GetCurrentProcess, GetProcessTimes};
    let (mut create, mut exit, mut kernel, mut user) = (
        FILETIME::default(),
        FILETIME::default(),
        FILETIME::default(),
        FILETIME::default(),
    );
    // SAFETY: GetProcessTimes writes the four FILETIMEs passed.
    unsafe {
        let _ = GetProcessTimes(
            GetCurrentProcess(),
            &mut create,
            &mut exit,
            &mut kernel,
            &mut user,
        );
    }
    let ms =
        |f: FILETIME| (((f.dwHighDateTime as u64) << 32) | f.dwLowDateTime as u64) as f64 / 10000.0;
    ms(kernel) + ms(user)
}

/// CPU cost of blend_background, serial and with every core (not compared: informational).
fn cmd_cpu(c: &mut Ctx) -> i32 {
    let mask = c.input("mask_a.u8");
    let mut f = fs::File::create(format!("{}\\cpu.txt", c.outdir)).unwrap();
    for (w, h) in [(1280u32, 720u32), (1920, 1080), (3840, 2160)] {
        for parallel in [false, true] {
            let mut buf = vec![0u8; w as usize * h as usize * 3 / 2];
            for (i, v) in buf.iter_mut().enumerate() {
                *v = (16 + (i * 7) % 200) as u8;
            }
            let mut scratch = BackgroundScratch::default();
            let mut run = |changed: bool| {
                let (y, uv) = buf.split_at_mut(w as usize * h as usize);
                blend_background(
                    y,
                    uv,
                    w as usize,
                    w,
                    h,
                    &mask,
                    changed,
                    50,
                    None,
                    &mut scratch,
                    parallel,
                );
            };
            run(true);
            for _ in 0..5 {
                run(false);
            }
            let n = 300;
            let cpu0 = process_cpu_ms();
            let t0 = Instant::now();
            for _ in 0..n {
                run(false);
            }
            let wall = ms(t0);
            let cpu = process_cpu_ms() - cpu0;
            let _ = writeln!(
                f,
                "{w}x{h} parallel={}: wall {:.2} ms/frame, cpu {:.2} ms/frame",
                parallel as i32,
                wall / n as f64,
                cpu / n as f64
            );
        }
    }
    0
}

fn cmd_seg(c: &mut Ctx, model: &str) -> i32 {
    let frames = [
        ("face_one_1280x720", 1280u32, 720u32),
        ("face_one_720x1280", 720, 1280),
        ("face_two_1280x720", 1280, 720),
        ("face_none_1280x720", 1280, 720),
        ("frame_a_1920x1080", 1920, 1080),
    ];
    let mut info = fs::File::create(format!("{}\\seg_info.txt", c.outdir)).unwrap();
    {
        // No model file: the state must become "unsupported" (0).
        let s = Segmenter::new();
        s.set_model("C:\\no\\such\\model.onnx");
        let dummy = vec![128u8; 1280 * 720 * 3 / 2];
        s.offer(
            &dummy[..1280 * 720],
            &dummy[1280 * 720..],
            1280,
            1280,
            720,
            0.0,
        );
        for _ in 0..200 {
            if s.state() != 1 {
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        c.line(&format!("seg_missing_model state={}", s.state()));
    }
    for (file, w, h) in frames {
        let src = c.input(&format!("{file}.nv12"));
        if src.is_empty() {
            continue;
        }
        let s = Segmenter::new();
        s.set_model(model);
        let mut mask = Vec::new();
        for round in 0..3 {
            let now = 1000.0 * (round + 1) as f64;
            if !s.due(now) {
                c.line(&format!("seg_{file}_round{round} not-due"));
                continue;
            }
            let (yp, uvp) = src.split_at(w as usize * h as usize);
            s.offer(yp, uvp, w as usize, w, h, now);
            let mut got = false;
            for _ in 0..1000 {
                if got {
                    break;
                }
                got = s.take_mask(&mut mask);
                if !got {
                    if s.state() != 1 {
                        break;
                    }
                    std::thread::sleep(Duration::from_millis(5));
                }
            }
            let name = format!("mask_{file}_r{round}");
            if got {
                c.result(&name, &format!("ok state={}", s.state()), Some(&mask));
            } else {
                c.line(&format!("{name} none state={}", s.state()));
            }
            let _ = writeln!(
                info,
                "{} inferMs={:.2} state={}",
                name,
                s.infer_ms(),
                s.state()
            );
            if round == 1 {
                s.reset();
            }
        }
    }
    {
        // One segmenter over different pictures: smoothing with the previous mask, then reset().
        let s = Segmenter::new();
        s.set_model(model);
        let seq = [
            "face_one_1280x720",
            "face_none_1280x720",
            "face_two_1280x720",
            "face_two_1280x720",
        ];
        let mut mask = Vec::new();
        c.line(&format!("seg_due_initial={}", s.due(1000.0) as i32));
        for (i, file) in seq.iter().enumerate() {
            let src = c.input(&format!("{file}.nv12"));
            if src.is_empty() {
                break;
            }
            let now = 2000.0 + 100.0 * i as f64;
            if i == 3 {
                s.reset();
            }
            s.offer(&src[..1280 * 720], &src[1280 * 720..], 1280, 1280, 720, now);
            if i == 0 {
                c.line(&format!(
                    "seg_due_10ms_after_offer={}",
                    s.due(now + 10.0) as i32
                ));
            }
            let mut got = false;
            for _ in 0..1000 {
                if got {
                    break;
                }
                got = s.take_mask(&mut mask);
                if !got {
                    std::thread::sleep(Duration::from_millis(5));
                }
            }
            if i == 0 {
                c.line(&format!(
                    "seg_due_70ms_after_done={}",
                    s.due(now + 70.0) as i32
                ));
            }
            if got {
                c.result(&format!("mask_mix_r{i}"), "ok", Some(&mask));
            } else {
                c.line(&format!("mask_mix_r{i} none"));
            }
        }
        c.line(&format!("seg_take_again={}", s.take_mask(&mut mask) as i32));
    }
    0
}

fn cmd_face(c: &mut Ctx) -> i32 {
    let frames = [
        ("face_one_1280x720", 1280u32, 720u32),
        ("face_one_1920x1080", 1920, 1080),
        ("face_one_640x360", 640, 360),
        ("face_two_1280x720", 1280, 720),
        ("face_two_1920x1080", 1920, 1080),
        ("face_none_1280x720", 1280, 720),
        ("face_none_1920x1080", 1920, 1080),
        ("face_one_720x1280", 720, 1280),
    ];
    let mut info = fs::File::create(format!("{}\\face_info.txt", c.outdir)).unwrap();
    for (file, w, h) in frames {
        let src = c.input(&format!("{file}.nv12"));
        if src.is_empty() {
            continue;
        }
        // A fresh detector per file; the same frame is offered 4 times (a tracker follows faces over time).
        let d = FaceDetector::new();
        let mut now = 1000.0;
        for round in 0..4 {
            now += 250.0;
            let mut tries = 0;
            while !d.due(now) && tries < 1000 {
                tries += 1;
                std::thread::sleep(Duration::from_millis(5));
            }
            d.offer(&src, w as usize, w, h, 7, now);
            let mut faces = Vec::new();
            let mut got = false;
            for _ in 0..2000 {
                if got {
                    break;
                }
                got = d.take_result(7, &mut faces);
                if !got {
                    if d.state() != 1 {
                        break;
                    }
                    std::thread::sleep(Duration::from_millis(5));
                }
            }
            let mut line = format!(
                "face_{file}_r{round} {} state={} n={}",
                if got { "result" } else { "none" },
                d.state(),
                faces.len()
            );
            for r in &faces {
                let _ = write!(line, " [{:.9} {:.9} {:.9} {:.9}]", r.x, r.y, r.w, r.h);
            }
            c.line(&line);
            if !got && d.state() != 1 {
                break;
            }
        }
        let _ = writeln!(info, "{file} state={}", d.state());
    }
    {
        // Gating: the interval, the generation tag, results handed out once.
        let src = c.input("face_one_1280x720.nv12");
        let d = FaceDetector::new();
        c.line(&format!("face_due_initial={}", d.due(1000.0) as i32));
        d.offer(&src, 1280, 1280, 720, 7, 1000.0);
        c.line(&format!(
            "face_due_100ms_after_offer={}",
            d.due(1100.0) as i32
        ));
        let mut tries = 0;
        while !d.due(1300.0) && tries < 2000 {
            tries += 1;
            std::thread::sleep(Duration::from_millis(5));
        }
        c.line(&format!(
            "face_due_300ms_after_offer_when_done={}",
            d.due(1300.0) as i32
        ));
        let mut faces = Vec::new();
        c.line(&format!(
            "face_take_other_generation={}",
            d.take_result(8, &mut faces) as i32
        ));
        let taken = d.take_result(7, &mut faces);
        c.line(&format!(
            "face_take_after_drop={} n={}",
            taken as i32,
            faces.len()
        ));
    }
    0
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 4 {
        eprintln!("usage: fx_rust still|blend|seg|face|window <indir> <outdir> [model|x y]");
        std::process::exit(2);
    }
    let outdir = args[3].clone();
    let _ = fs::create_dir_all(&outdir);
    // The engine's capture thread is in the multithreaded apartment: so is this one.
    // SAFETY: plain COM initialisation of the main thread, balanced below.
    unsafe {
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
    }
    let results = fs::File::create(format!("{outdir}\\results.txt")).unwrap();
    let mut c = Ctx {
        indir: args[2].clone(),
        outdir,
        results,
    };
    let rc = match args[1].as_str() {
        "still" => cmd_still(&mut c),
        "blend" => cmd_blend(&mut c),
        "seg" if args.len() >= 5 => cmd_seg(&mut c, &args[4]),
        "face" => cmd_face(&mut c),
        "cpu" => cmd_cpu(&mut c),
        "closewatch" if args.len() >= 5 => {
            window::cmd_closewatch(&mut c, args[4].parse().unwrap_or(0))
        }
        "window" => {
            let x = args.get(4).and_then(|s| s.parse().ok()).unwrap_or(i32::MIN);
            let y = args.get(5).and_then(|s| s.parse().ok()).unwrap_or(i32::MIN);
            window::cmd_window(&mut c, x, y)
        }
        _ => 2,
    };
    drop(c);
    // SAFETY: balances the CoInitializeEx above.
    unsafe { CoUninitialize() };
    std::process::exit(rc);
}
