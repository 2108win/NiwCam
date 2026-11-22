//! Port of `tools/frame_check.cpp`: the size rule, the fit layout, moving the view, black bars,
//! pitch-aware scale/adjust into a rectangle, BGRA <-> NV12 and skin smoothing.
//! (The SIMD-versus-plain check of the C++ has no counterpart: there is one implementation.)

use niwcam_core::auto_frame::FaceRect;
use niwcam_core::image_settings::*;
use niwcam_core::nv12::*;
use niwcam_core::skin_smooth::{smooth_face, SkinScratch};

fn nv12_bytes(w: u32, h: u32) -> usize {
    w as usize * h as usize * 3 / 2
}

#[test]
fn plan() {
    let p = |mode, pw, ph, cw, ch, holds| plan_output(mode, pw, ph, cw, ch, holds);
    let expect = |name: &str, got: OutputPlan, w, h, rebuild| {
        assert_eq!(
            got,
            OutputPlan {
                width: w,
                height: h,
                rebuild
            },
            "plan {name}"
        );
    };
    use OutputSize::*;
    expect(
        "auto first",
        p(Auto, 1920, 1080, 0, 0, false),
        1920,
        1080,
        true,
    );
    expect(
        "auto same",
        p(Auto, 1920, 1080, 1920, 1080, true),
        1920,
        1080,
        false,
    );
    expect(
        "auto new size, free",
        p(Auto, 1080, 1440, 1920, 1080, false),
        1080,
        1440,
        true,
    );
    expect(
        "auto new size, in use",
        p(Auto, 1080, 1440, 1920, 1080, true),
        1920,
        1080,
        false,
    );
    expect(
        "auto first, in use",
        p(Auto, 3840, 2160, 0, 0, true),
        3840,
        2160,
        true,
    );
    expect(
        "1080p portrait picture",
        p(R1080, 1080, 1920, 0, 0, false),
        1920,
        1080,
        true,
    );
    expect(
        "1080p kept",
        p(R1080, 3840, 2160, 1920, 1080, false),
        1920,
        1080,
        false,
    );
    expect(
        "4K from 1080p, free",
        p(R2160, 1920, 1080, 1920, 1080, false),
        3840,
        2160,
        true,
    );
    expect(
        "720p from 1080p, in use",
        p(R720, 1920, 1080, 1920, 1080, true),
        1920,
        1080,
        false,
    );
    expect(
        "back to auto, free",
        p(Auto, 1280, 720, 1920, 1080, false),
        1280,
        720,
        true,
    );
}

fn expect_fit(name: &str, l: &FrameLayout, fit_w: u32, fit_h: u32) {
    let n = format!("fit {name}: ");
    assert!(l.fitted, "{n}fitted");
    assert_eq!((l.out_width, l.out_height), (fit_w, fit_h), "{n}output");
    assert!(l.needs_resample, "{n}resamples");
    assert!(
        l.dest_x.is_multiple_of(2)
            && l.dest_y.is_multiple_of(2)
            && l.dest_width.is_multiple_of(2)
            && l.dest_height.is_multiple_of(2),
        "{n}even rect"
    );
    assert!(
        l.dest_x + l.dest_width <= fit_w && l.dest_y + l.dest_height <= fit_h,
        "{n}rect inside the output"
    );
    let dx = (fit_w - l.dest_width) as i32 - 2 * l.dest_x as i32;
    let dy = (fit_h - l.dest_height) as i32 - 2 * l.dest_y as i32;
    assert!((0..=2).contains(&dx) && (0..=2).contains(&dy), "{n}centred");
    assert!(
        l.dest_width == fit_w || l.dest_height == fit_h,
        "{n}touches two sides"
    );
    let want = l.picture_width as f64 / l.picture_height as f64;
    let got = l.dest_width as f64 / l.dest_height as f64;
    let tolerance = 2.0 / l.dest_width.min(l.dest_height) as f64 * want + 1e-9;
    assert!((got - want).abs() <= tolerance, "{n}aspect {got} vs {want}");
}

#[test]
fn fit_layout() {
    let plain = ImageSettings::default();
    let l = compute_layout(1920, 1080, &plain, 0, 0);
    assert!(!l.fitted && l.out_width == 1920 && l.out_height == 1080 && l.picture_width == 1920);
    assert!(l.dest_width == 1920 && l.dest_height == 1080 && l.dest_x == 0 && l.dest_y == 0);
    assert!(
        !l.needs_resample && !l.needs_orient,
        "no fit: straight copy"
    );
    let same = compute_layout(1920, 1080, &plain, 1920, 1080);
    assert!(
        !same.fitted && !same.needs_resample,
        "fit to the same size is no fit"
    );

    // Phone held upright into a call's 1080p camera: pillarbox.
    let mut turned = plain;
    turned.orientation.rotation = 90;
    let l = compute_layout(1920, 1080, &turned, 1920, 1080);
    expect_fit("portrait into 1080p", &l, 1920, 1080);
    assert!(l.picture_width == 1080 && l.picture_height == 1920);
    assert!(l.dest_height == 1080 && l.dest_width == 608 && l.dest_x == 656 && l.dest_y == 0);
    assert!(l.needs_orient);

    let l = compute_layout(1280, 720, &plain, 1920, 1080);
    expect_fit("720p into 1080p", &l, 1920, 1080);
    assert!(l.dest_x == 0 && l.dest_y == 0 && l.dest_width == 1920 && l.dest_height == 1080);

    let l = compute_layout(1440, 1080, &plain, 1920, 1080);
    expect_fit("4:3 into 16:9", &l, 1920, 1080);
    assert!(l.dest_x == 240 && l.dest_width == 1440);

    let l = compute_layout(1920, 1080, &plain, 1080, 1440);
    expect_fit("16:9 into 3:4", &l, 1080, 1440);
    assert!(l.dest_x == 0 && l.dest_width == 1080 && l.dest_height == 608 && l.dest_y == 416);

    let square = ImageSettings {
        aspect: CropAspect::R1x1,
        zoom: 2.0,
        center_x: 0.3,
        ..plain
    };
    let natural = compute_layout(1920, 1080, &square, 0, 0);
    let l = compute_layout(1920, 1080, &square, 1280, 720);
    expect_fit("1:1 zoomed into 720p", &l, 1280, 720);
    assert!(l.dest_width == 720 && l.dest_height == 720 && l.dest_x == 280);
    assert_eq!(l.source, natural.source, "fit leaves the zoom region alone");

    expect_fit(
        "odd output",
        &compute_layout(1920, 1080, &plain, 1001, 601),
        1000,
        600,
    );
    expect_fit(
        "sliver",
        &compute_layout(2, 200, &plain, 1920, 1080),
        1920,
        1080,
    );
}

#[test]
fn bars() {
    let (w, h, rx, ry, rw, rh) = (64u32, 36u32, 8u32, 4u32, 20u32, 10u32);
    let mut f = vec![77u8; nv12_bytes(w, h)];
    nv12_fill_bars(&mut f, w, h, rx, ry, rw, rh);
    let (wu, hu) = (w as usize, h as usize);
    for y in 0..hu {
        for x in 0..wu {
            let inside = (rx as usize..(rx + rw) as usize).contains(&x)
                && (ry as usize..(ry + rh) as usize).contains(&y);
            assert_eq!(
                f[y * wu + x],
                if inside { 77 } else { BLACK_Y },
                "luma {x},{y}"
            );
        }
    }
    let uv = &f[wu * hu..];
    for y in 0..hu / 2 {
        for x in 0..wu {
            let inside = (rx as usize..(rx + rw) as usize).contains(&x)
                && (ry as usize / 2..(ry + rh) as usize / 2).contains(&y);
            assert_eq!(
                uv[y * wu + x],
                if inside { 77 } else { BLACK_UV },
                "chroma {x},{y}"
            );
        }
    }
    let mut full = vec![77u8; nv12_bytes(w, h)];
    nv12_fill_bars(&mut full, w, h, 0, 0, w, h);
    assert!(
        full.iter().all(|&v| v == 77),
        "a full rectangle paints nothing"
    );
}

fn pattern(w: u32, h: u32, seed: usize) -> Vec<u8> {
    (0..nv12_bytes(w, h))
        .map(|i| ((i * 31 + seed * 17 + (i / w as usize) * 7) % 220 + 20) as u8)
        .collect()
}

#[test]
fn scale_into_rect() {
    let (sw, sh) = (96u32, 54u32);
    let src = pattern(sw, sh, 1);
    let r = Nv12Rect {
        x: 4.5,
        y: 2.0,
        w: 80.0,
        h: 45.0,
    };
    let (dw, dh) = (40u32, 22u32);
    let mut plain = vec![0u8; nv12_bytes(dw, dh)];
    nv12_crop_scale_bilinear(&src, sw as usize, sw, sh, &r, &mut plain, dw, dh);

    let (fw, fh, x0, y0) = (64usize, 36usize, 10usize, 6usize);
    let mut frame = vec![3u8; fw * fh * 3 / 2];
    let (fy, fuv) = frame.split_at_mut(fw * fh);
    nv12_crop_scale_bilinear_to(
        &src,
        sw as usize,
        sw,
        sh,
        &r,
        &mut fy[y0 * fw + x0..],
        &mut fuv[(y0 / 2) * fw + x0..],
        fw,
        dw,
        dh,
    );

    let (dwu, dhu) = (dw as usize, dh as usize);
    for y in 0..fh {
        for x in 0..fw {
            let inside = x >= x0 && x < x0 + dwu && y >= y0 && y < y0 + dhu;
            let v = frame[y * fw + x];
            if inside {
                assert_eq!(v, plain[(y - y0) * dwu + (x - x0)], "luma {x},{y}");
            } else {
                assert_eq!(v, 3, "outside luma {x},{y}");
            }
        }
    }
    for y in 0..fh / 2 {
        for x in 0..fw {
            let inside = x >= x0 && x < x0 + dwu && y >= y0 / 2 && y < (y0 + dhu) / 2;
            let v = frame[fw * fh + y * fw + x];
            if inside {
                assert_eq!(
                    v,
                    plain[dwu * dhu + (y - y0 / 2) * dwu + (x - x0)],
                    "chroma {x},{y}"
                );
            } else {
                assert_eq!(v, 3, "outside chroma {x},{y}");
            }
        }
    }
}

#[test]
fn adjust_rect() {
    let (fw, fh, x0, y0, rw, rh) = (64usize, 36usize, 12usize, 4usize, 32usize, 20usize);
    let mut frame = pattern(fw as u32, fh as u32, 2);
    nv12_fill_bars(
        &mut frame, fw as u32, fh as u32, x0 as u32, y0 as u32, rw as u32, rh as u32,
    );

    let mut alone = vec![0u8; rw * rh * 3 / 2];
    for y in 0..rh {
        alone[y * rw..(y + 1) * rw]
            .copy_from_slice(&frame[(y0 + y) * fw + x0..(y0 + y) * fw + x0 + rw]);
    }
    for y in 0..rh / 2 {
        let s = fw * fh + (y0 / 2 + y) * fw + x0;
        alone[rw * rh + y * rw..rw * rh + (y + 1) * rw].copy_from_slice(&frame[s..s + rw]);
    }
    let c = Nv12ColorAdjust {
        brightness: 40,
        contrast: -20,
        saturation: 30,
        temperature: 25,
    };
    let mut expected = vec![0u8; alone.len()];
    nv12_adjust(&alone, rw, &mut expected, rw as u32, rh as u32, &c);

    let (fy, fuv) = frame.split_at_mut(fw * fh);
    nv12_adjust_planes_in_place(
        &mut fy[y0 * fw + x0..],
        &mut fuv[(y0 / 2) * fw + x0..],
        fw,
        rw as u32,
        rh as u32,
        &c,
    );

    for y in 0..fh {
        for x in 0..fw {
            let inside = x >= x0 && x < x0 + rw && y >= y0 && y < y0 + rh;
            let v = frame[y * fw + x];
            if inside {
                assert_eq!(v, expected[(y - y0) * rw + (x - x0)], "luma {x},{y}");
            } else {
                assert_eq!(v, BLACK_Y, "bars luma {x},{y}");
            }
        }
    }
    for y in 0..fh / 2 {
        for x in 0..fw {
            let inside = x >= x0 && x < x0 + rw && y >= y0 / 2 && y < (y0 + rh) / 2;
            let v = frame[fw * fh + y * fw + x];
            if inside {
                assert_eq!(
                    v,
                    expected[rw * rh + (y - y0 / 2) * rw + (x - x0)],
                    "chroma {x},{y}"
                );
            } else {
                assert_eq!(v, BLACK_UV, "bars chroma {x},{y}");
            }
        }
    }
}

fn solid(w: usize, h: usize, pitch: usize, (r, g, b): (i32, i32, i32)) -> Vec<u8> {
    let mut img = vec![0xEE; pitch * h]; // pitch padding stays 0xEE
    for y in 0..h {
        for x in 0..w {
            let p = &mut img[y * pitch + x * 4..y * pitch + x * 4 + 4];
            p.copy_from_slice(&[b as u8, g as u8, r as u8, 255]);
        }
    }
    img
}

#[test]
fn bgra_to_nv12_and_back() {
    let (w, h) = (8usize, 4usize);
    let pitch = w * 4 + 16;
    let convert = |c| {
        let img = solid(w, h, pitch, c);
        let mut nv = vec![0u8; nv12_bytes(w as u32, h as u32)];
        let (y, uv) = nv.split_at_mut(w * h);
        bgra_to_nv12(&img, pitch, w as u32, h as u32, y, uv, w);
        (nv[w + 3], nv[w * h + w + 2], nv[w * h + w + 3])
    };
    assert_eq!(convert((255, 255, 255)), (235, 128, 128), "white");
    assert_eq!(convert((0, 0, 0)), (16, 128, 128), "black");
    assert_eq!(convert((255, 0, 0)), (82, 90, 240), "red");
    assert_eq!(convert((0, 0, 255)), (41, 240, 110), "blue");

    // A 2x2 block half white, half black: luma per pixel, chroma neutral.
    let mut img = solid(2, 2, 8, (0, 0, 0));
    img[..3].fill(255);
    img[8 + 4..8 + 7].fill(255);
    let mut nv = vec![0u8; nv12_bytes(2, 2)];
    let (y, uv) = nv.split_at_mut(4);
    bgra_to_nv12(&img, 8, 2, 2, y, uv, 2);
    assert_eq!(&nv[..6], &[235, 16, 16, 235, 128, 128]);

    // Round trip for flat colours stays within a few levels.
    let mut worst = 0;
    for r in (0..=255).step_by(51) {
        for g in (0..=255).step_by(51) {
            for b in (0..=255).step_by(51) {
                let src = solid(w, h, pitch, (r, g, b));
                let mut n = vec![0u8; nv12_bytes(w as u32, h as u32)];
                let (y, uv) = n.split_at_mut(w * h);
                bgra_to_nv12(&src, pitch, w as u32, h as u32, y, uv, w);
                let mut back = vec![0u32; w * h];
                nv12_to_bgra(
                    &n[..w * h],
                    &n[w * h..],
                    w,
                    w as u32,
                    h as u32,
                    &mut back,
                    w as u32,
                    h as u32,
                );
                let p = back[5];
                assert_eq!(p >> 24, 255, "opaque");
                for (got, want) in [((p >> 16) & 255, r), ((p >> 8) & 255, g), (p & 255, b)] {
                    worst = worst.max((got as i32 - want).abs());
                }
            }
        }
    }
    assert!(worst <= 4, "round trip within 4 levels, worst {worst}");
}

#[test]
fn nv12_to_bgra_scaled() {
    let (w, h) = (16usize, 8usize);
    let mut nv = vec![128u8; nv12_bytes(w as u32, h as u32)];
    for y in 0..h {
        for x in 0..w {
            nv[y * w + x] = if x < w / 2 { 16 } else { 235 };
        }
    }
    let mut out = vec![0u32; 4 * 2];
    nv12_to_bgra(
        &nv[..w * h],
        &nv[w * h..],
        w,
        w as u32,
        h as u32,
        &mut out,
        4,
        2,
    );
    assert_eq!(
        out,
        [
            0xFF000000, 0xFF000000, 0xFFFFFFFF, 0xFFFFFFFF, 0xFF000000, 0xFF000000, 0xFFFFFFFF,
            0xFFFFFFFF
        ]
    );
}

fn same_rect(a: &Nv12Rect, b: &Nv12Rect) -> bool {
    (a.x - b.x).abs() < 1e-9
        && (a.y - b.y).abs() < 1e-9
        && (a.w - b.w).abs() < 1e-9
        && (a.h - b.h).abs() < 1e-9
}

#[test]
fn view() {
    let aspects = [
        CropAspect::Native,
        CropAspect::R16x9,
        CropAspect::R4x3,
        CropAspect::R1x1,
        CropAspect::R9x16,
    ];
    for z in [1.0, 1.37, 2.0, 3.5] {
        for c in [(0.5, 0.5), (0.1, 0.9), (0.93, 0.07)] {
            for aspect in aspects {
                let mut s = ImageSettings {
                    aspect,
                    ..Default::default()
                };
                let mut moved = compute_layout(3840, 2160, &s, 0, 0);
                s.zoom = z;
                s.center_x = c.0;
                s.center_y = c.1;
                let want = compute_layout(3840, 2160, &s, 0, 0);
                set_layout_view(&mut moved, z, c.0, c.1);
                assert!(
                    same_rect(&moved.source, &want.source)
                        && moved.needs_resample == want.needs_resample,
                    "zoom {z} aspect {aspect:?}"
                );
            }
        }
    }

    let mut l = compute_layout(1920, 1080, &ImageSettings::default(), 0, 0);
    set_layout_view(&mut l, 2.0, 0.0, 1.0);
    assert!(
        l.source.x == 0.0
            && (l.source.y - 540.0).abs() < 1e-9
            && l.source.w == 960.0
            && l.source.h == 540.0
    );
    set_layout_view(&mut l, 2.0, 1.0, 0.0);
    assert!((l.source.x - 960.0).abs() < 1e-9 && l.source.y == 0.0);

    let square = ImageSettings {
        aspect: CropAspect::R1x1,
        ..Default::default()
    };
    let mut l = compute_layout(1920, 1080, &square, 0, 0);
    set_layout_view(&mut l, 1.0, 0.3011, 0.5);
    assert!(
        l.source.x % 2.0 == 0.0 && l.source.x == l.source.x.floor(),
        "zoom 1 crop on even pixels"
    );

    let mut l = compute_layout(1920, 1080, &ImageSettings::default(), 0, 0);
    assert!(!l.needs_resample);
    set_layout_view(&mut l, 1.6, 0.5, 0.5);
    assert!(l.needs_resample);
    set_layout_view(&mut l, 1.0, 0.5, 0.5);
    assert!(!l.needs_resample);

    let mut l = compute_layout(1080, 1920, &ImageSettings::default(), 1920, 1080);
    let before = l;
    set_layout_view(&mut l, 2.5, 0.4, 0.3);
    assert!(l.fitted && l.needs_resample);
    assert_eq!(
        (
            l.out_width,
            l.out_height,
            l.dest_x,
            l.dest_y,
            l.dest_width,
            l.dest_height
        ),
        (
            before.out_width,
            before.out_height,
            before.dest_x,
            before.dest_y,
            before.dest_width,
            before.dest_height
        )
    );
    assert!((l.source.w - 1080.0 / 2.5).abs() < 1e-9 && (l.source.h - 1920.0 / 2.5).abs() < 1e-9);
}

#[test]
fn skin_smooth_keeps_edges_and_evens_skin() {
    let (w, h) = (640usize, 480usize);
    let mut y = vec![0u8; w * h];
    let mut seed = 12345u32;
    let mut noise = || {
        seed = seed.wrapping_mul(1103515245).wrapping_add(12345);
        ((seed >> 16) % 13) as i32 - 6
    };
    for r in 0..h {
        for c in 0..w {
            y[r * w + c] = ((if c < 320 { 120 } else { 200 }) + noise()) as u8;
        }
    }
    let before = y.clone();
    let face = FaceRect {
        x: 0.35,
        y: 0.3,
        w: 0.3,
        h: 0.4,
    };
    let mut scratch = SkinScratch::default();
    smooth_face(&mut y, w, w as u32, h as u32, &face, 80, &mut scratch);

    let spread = |p: &[u8], x0: usize, x1: usize, y0: usize, y1: usize| {
        let (mut sum, mut sq, mut n) = (0.0f64, 0.0f64, 0);
        for r in y0..y1 {
            for c in x0..x1 {
                let v = p[r * w + c] as f64;
                sum += v;
                sq += v * v;
                n += 1;
            }
        }
        let mean = sum / n as f64;
        (sq / n as f64 - mean * mean).max(0.0).sqrt()
    };
    let (nb, na) = (
        spread(&before, 260, 300, 200, 260),
        spread(&y, 260, 300, 200, 260),
    );
    assert!(na < nb * 0.6, "skin: noise {nb} -> {na}");
    let mean = |x0: usize, x1: usize| {
        let mut s = 0.0;
        for r in 220..240 {
            for c in x0..x1 {
                s += y[r * w + c] as f64;
            }
        }
        s / (20.0 * (x1 - x0) as f64)
    };
    assert!(
        (mean(310, 316) - 120.0).abs() < 6.0 && (mean(324, 330) - 200.0).abs() < 6.0,
        "the edge stays"
    );
    assert!(
        y[..40 * w] == before[..40 * w],
        "outside the face unchanged"
    );
    let mut off = before.clone();
    smooth_face(&mut off, w, w as u32, h as u32, &face, 0, &mut scratch);
    assert!(off == before, "strength 0 changes nothing");
}
