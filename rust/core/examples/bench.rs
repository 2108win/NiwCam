//! Times the per-frame picture functions at 720p and 1080p (the C++ twin is xcheck/cpp_bench.cpp).
use niwcam_core::nv12::*;
use std::time::Instant;

fn time(name: &str, n: u32, mut f: impl FnMut()) {
    for _ in 0..3 {
        f();
    }
    let t = Instant::now();
    for _ in 0..n {
        f();
    }
    println!(
        "{name} {:.3} ms",
        t.elapsed().as_secs_f64() * 1000.0 / n as f64
    );
}

fn main() {
    for (w, h) in [(1280u32, 720u32), (1920, 1080)] {
        let src: Vec<u8> = (0..w as usize * h as usize * 3 / 2)
            .map(|i| (i * 7 + i / 13) as u8)
            .collect();
        let n = if w > 1300 { 100 } else { 200 };
        let mut bgra = vec![0u32; w as usize * h as usize];
        let (y, uv) = src.split_at(w as usize * h as usize);
        time(&format!("{w}x{h} nv12_to_bgra 1:1"), n, || {
            nv12_to_bgra(y, uv, w as usize, w, h, &mut bgra, w, h)
        });
        let (pw, ph) = (w / 2, h / 2);
        let mut small = vec![0u32; pw as usize * ph as usize];
        time(&format!("{w}x{h} nv12_to_bgra half"), n, || {
            nv12_to_bgra(y, uv, w as usize, w, h, &mut small, pw, ph)
        });
        let mut dst = vec![0u8; src.len()];
        time(&format!("{w}x{h} nv12_scale same size"), n, || {
            nv12_scale(&src, w as usize, w, h, &mut dst, w as usize, w, h)
        });
        time(&format!("{w}x{h} nv12_adjust"), n, || {
            nv12_adjust(
                &src,
                w as usize,
                &mut dst,
                w,
                h,
                &Nv12ColorAdjust {
                    brightness: 10,
                    contrast: 5,
                    saturation: 20,
                    temperature: 8,
                },
            )
        });
        let r = Nv12Rect {
            x: 100.0,
            y: 60.0,
            w: w as f64 - 200.0,
            h: h as f64 - 120.0,
        };
        time(&format!("{w}x{h} bilinear zoom"), n, || {
            nv12_crop_scale_bilinear(&src, w as usize, w, h, &r, &mut dst, w, h)
        });
        let o = Nv12Orientation {
            rotation: 90,
            mirror: false,
            flip: false,
        };
        time(&format!("{w}x{h} orient 90"), n, || {
            nv12_orient(&src, w as usize, w, h, &mut dst, &o)
        });
        let mut bg = vec![0u8; w as usize * 4 * h as usize];
        time(&format!("{w}x{h} bgra_to_nv12"), n, || {
            let (a, b) = dst.split_at_mut(w as usize * h as usize);
            bgra_to_nv12(&bg, w as usize * 4, w, h, a, b, w as usize)
        });
        bg[0] = 1;
    }
}
