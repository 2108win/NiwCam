//! Rust twin of xcheck/cpp_record.cpp. `record <output dir>`
use niwcam_media::recorder::Recorder;
use niwcam_media::snapshot::save_png;
use std::time::Instant;
use windows::Win32::Media::MediaFoundation::{MFStartup, MFSTARTUP_FULL, MF_VERSION};
use windows::Win32::System::Com::{CoInitializeEx, COINIT_MULTITHREADED};

/// xcheck/pattern.h `DrawPattern`.
fn draw_pattern(w: u32, h: u32, n: u32) -> Vec<u8> {
    let (wu, hu) = (w as usize, h as usize);
    let mut nv12 = vec![0u8; wu * hu * 3 / 2];
    let (y, uv) = nv12.split_at_mut(wu * hu);
    for r in 0..h {
        for c in 0..w {
            y[(r * w + c) as usize] = (40 + (c * 160 / w) + ((r * 7 + n * 3) % 23)) as u8;
        }
    }
    for r in 0..h / 2 {
        for c in 0..w / 2 {
            uv[(r * w + 2 * c) as usize] = (64 + c * 128 / (w / 2)) as u8;
            uv[(r * w + 2 * c + 1) as usize] = (64 + r * 128 / (h / 2)) as u8;
        }
    }
    let bx = ((n * 12) % (w - 160)) as usize;
    let mut r = h / 3;
    while r < h / 3 + 120 && r < h {
        let o = r as usize * wu + bx;
        y[o..o + 160].fill(235);
        r += 1;
    }
    nv12
}

fn main() {
    let a: Vec<String> = std::env::args().collect();
    let dir = format!("{}\\", a[1]);
    // SAFETY: COM / Media Foundation start like the C++ driver.
    unsafe {
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
        let _ = MFStartup(MF_VERSION, MFSTARTUP_FULL);
    }
    {
        let bad = Recorder::new();
        println!("stop-idle {:08x}", bad.stop().0 as u32);
        println!(
            "start-too-small {:08x}",
            bad.start(&format!("{dir}x.mp4"), 10, 10, 30, false).0 as u32
        );
        println!(
            "start-bad-path {:08x}",
            bad.start(&format!("{dir}no-such-dir\\x.mp4"), 640, 480, 30, false)
                .0 as u32
        );
        println!(
            "idle active {} audio {} elapsed {} problem [{}]",
            bad.active() as i32,
            bad.has_audio() as i32,
            bad.elapsed_ms(),
            bad.audio_problem()
        );
        bad.push(&draw_pattern(640, 480, 0), 640, 640, 480); // inactive: ignored
    }

    let r = Recorder::new();
    println!(
        "start {:08x}",
        r.start(&format!("{dir}rec.mp4"), 1280, 721, 30, false).0 as u32
    );
    println!(
        "active {} audio {} problem [{}]",
        r.active() as i32,
        r.has_audio() as i32,
        r.audio_problem()
    );
    let t0 = Instant::now();
    let mut pushed = 0u32;
    let mut n = 0u32;
    'outer: loop {
        let due = n as f64 * (1.0 / 30.0);
        loop {
            let t = t0.elapsed().as_secs_f64();
            if t >= 3.0 {
                break 'outer;
            }
            if t >= due {
                break;
            }
            std::thread::yield_now(); // SwitchToThread: the same pacing as the C++ driver
        }
        if (30..36).contains(&n) {
            r.push(&draw_pattern(640, 480, n), 640, 640, 480); // another shape: fitted with bars
        } else {
            r.push(&draw_pattern(1280, 720, n), 1280, 1280, 720);
        }
        pushed += 1;
        n += 1;
    }
    println!("elapsed>=2900ms {}", (r.elapsed_ms() >= 2900) as i32);
    println!("stop {:08x}", r.stop().0 as u32);
    println!("stop-again {:08x}", r.stop().0 as u32);
    println!(
        "after active {} elapsed {}",
        r.active() as i32,
        r.elapsed_ms()
    );
    eprintln!("pushed {pushed}");

    let snap = draw_pattern(1280, 720, 7);
    println!(
        "png {:08x}",
        save_png(&format!("{dir}snap.png"), &snap, 1280, 720).0 as u32
    );
    let small = draw_pattern(322, 182, 3);
    println!(
        "png-small {:08x}",
        save_png(&format!("{dir}snap-small.png"), &small, 322, 182).0 as u32
    );
    println!(
        "png-tiny {:08x}",
        save_png(&format!("{dir}snap-tiny.png"), &snap, 1, 1).0 as u32
    );
    println!(
        "png-null {:08x}",
        save_png(&format!("{dir}snap-null.png"), &[], 100, 100).0 as u32
    );
    println!(
        "png-bad-path {:08x}",
        save_png(&format!("{dir}no-such-dir\\snap.png"), &snap, 1280, 720).0 as u32
    );
}
