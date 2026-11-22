//! Prints the same lines as `xcheck/cpp_hashes.cpp`, from the Rust port. `xcheck/run.ps1` diffs them.

use niwcam_core::auto_frame::*;
use niwcam_core::image_settings::*;
use niwcam_core::json::Value;
use niwcam_core::nv12::*;
use niwcam_core::skin_smooth::{smooth_face, SkinScratch};

fn fnv(b: &[u8]) -> u64 {
    let mut h = 1469598103934665603u64;
    for &x in b {
        h ^= x as u64;
        h = h.wrapping_mul(1099511628211);
    }
    h
}

fn out(name: &str, b: &[u8]) {
    println!("{name} {:016x}", fnv(b));
}

fn noise(n: usize, seed: u32) -> Vec<u8> {
    let mut s = seed;
    (0..n)
        .map(|_| {
            s = s.wrapping_mul(1664525).wrapping_add(1013904223);
            (s >> 24) as u8
        })
        .collect()
}

fn bytes(w: u32, h: u32) -> usize {
    w as usize * h as usize * 3 / 2
}

/// C's `%.9g` through our own writer: a JSON number prints exactly that.
fn g(v: f64) -> String {
    Value::from(v).dump()
}

fn layout(l: &FrameLayout) -> String {
    format!(
        "{}x{} out {}x{} src {},{},{},{} o{} r{} pic {}x{} fit{} dest {},{},{},{}",
        l.oriented_width,
        l.oriented_height,
        l.out_width,
        l.out_height,
        g(l.source.x),
        g(l.source.y),
        g(l.source.w),
        g(l.source.h),
        l.needs_orient as u8,
        l.needs_resample as u8,
        l.picture_width,
        l.picture_height,
        l.fitted as u8,
        l.dest_x,
        l.dest_y,
        l.dest_width,
        l.dest_height
    )
}

fn main() {
    // adjust
    {
        let (w, h) = (66u32, 36u32);
        let src = noise(bytes(w, h), 7);
        let vals = [-100, -37, 0, 41, 100];
        for b in vals {
            for c in vals {
                for s in vals {
                    for t in [-100, 0, 63] {
                        let a = Nv12ColorAdjust {
                            brightness: b,
                            contrast: c,
                            saturation: s,
                            temperature: t,
                        };
                        let mut dst = vec![0u8; src.len()];
                        nv12_adjust(&src, w as usize, &mut dst, w, h, &a);
                        out(&format!("adjust {b} {c} {s} {t}"), &dst);
                    }
                }
            }
        }
    }
    // orient
    for w in [64u32, 66] {
        for rot in [0, 90, 180, 270] {
            for m in 0..2 {
                for f in 0..2 {
                    let h = 36u32;
                    let pitch = (w + 6) as usize;
                    let src = noise(pitch * h as usize * 3 / 2, 11);
                    let o = Nv12Orientation {
                        rotation: rot,
                        mirror: m == 1,
                        flip: f == 1,
                    };
                    let mut dst = vec![0u8; bytes(w, h)];
                    nv12_orient(&src, pitch, w, h, &mut dst, &o);
                    out(&format!("orient {w} {rot} {m}{f}"), &dst);
                }
            }
        }
    }
    // bilinear crop-scale
    {
        let (sw, sh) = (96u32, 54u32);
        let pitch = 100usize;
        let src = noise(pitch * sh as usize * 3 / 2, 21);
        let r = |x, y, w, h| Nv12Rect { x, y, w, h };
        let rects = [
            r(0.0, 0.0, 96.0, 54.0),
            r(4.5, 2.0, 80.0, 45.0),
            r(10.25, 6.75, 48.0, 27.0),
            r(0.0, 0.0, 32.0, 18.0),
            r(50.0, 20.0, 46.0, 34.0),
        ];
        let sizes = [(96u32, 54u32), (40, 22), (160, 90), (2, 2), (64, 36)];
        let mut i = 0;
        for rc in &rects {
            for s in &sizes {
                let mut dst = vec![0u8; bytes(s.0, s.1)];
                nv12_crop_scale_bilinear(&src, pitch, sw, sh, rc, &mut dst, s.0, s.1);
                out(&format!("bilinear {i}"), &dst);
                i += 1;
            }
        }
    }
    // bars, scale, fit
    {
        let mut f = vec![77u8; bytes(64, 36)];
        nv12_fill_bars(&mut f, 64, 36, 8, 4, 20, 10);
        out("bars", &f);
        let pairs = [
            (96u32, 54u32, 40u32, 22u32),
            (40, 22, 96, 54),
            (64, 36, 64, 36),
            (108, 192, 192, 108),
            (192, 108, 108, 192),
            (66, 34, 100, 50),
            (2, 200, 192, 108),
        ];
        for (i, p) in pairs.iter().enumerate() {
            let sp = p.0 as usize + 4;
            let src = noise(sp * p.1 as usize * 3 / 2, 31 + i as u32);
            let mut a = vec![0u8; bytes(p.2, p.3)];
            let mut b = vec![0xABu8; bytes(p.2, p.3)];
            nv12_scale(&src, sp, p.0, p.1, &mut a, p.2 as usize, p.2, p.3);
            nv12_fit_into(&src, sp, p.0, p.1, &mut b, p.2 as usize, p.2, p.3);
            out(&format!("scale {i}"), &a);
            out(&format!("fit {i}"), &b);
        }
    }
    // bgra <-> nv12
    {
        let (w, h) = (34u32, 18u32);
        let pitch = w as usize * 4 + 8;
        let bgra = noise(pitch * h as usize, 41);
        let mut nv = vec![0u8; bytes(w, h)];
        let (y, uv) = nv.split_at_mut((w * h) as usize);
        bgra_to_nv12(&bgra, pitch, w, h, y, uv, w as usize);
        out("bgra2nv12", &nv);
        for sz in [(34u32, 18u32), (17, 9), (70, 37)] {
            let sp = (w + 6) as usize;
            let src = noise(sp * h as usize * 3 / 2, 51);
            let mut px = vec![0u32; (sz.0 * sz.1) as usize];
            nv12_to_bgra(
                &src[..sp * h as usize],
                &src[sp * h as usize..],
                sp,
                w,
                h,
                &mut px,
                sz.0,
                sz.1,
            );
            let b: Vec<u8> = px.iter().flat_map(|v| v.to_le_bytes()).collect();
            out(&format!("nv12tobgra {}", sz.0), &b);
        }
    }
    // layouts and plans
    {
        let cams = [
            (1920u32, 1080u32),
            (1080, 1920),
            (3840, 2160),
            (1440, 1080),
            (2, 200),
            (641, 481),
        ];
        let fits = [
            (0u32, 0u32),
            (1920, 1080),
            (1080, 1440),
            (1001, 601),
            (1280, 720),
        ];
        let aspects = [
            CropAspect::Native,
            CropAspect::R16x9,
            CropAspect::R4x3,
            CropAspect::R1x1,
            CropAspect::R9x16,
        ];
        let mut n = 0;
        for c in cams {
            for rot in [0, 90, 270] {
                for a in aspects {
                    for z in [1.0, 1.7, 4.5] {
                        for f in fits {
                            let s = ImageSettings {
                                orientation: Nv12Orientation {
                                    rotation: rot,
                                    mirror: n % 3 == 0,
                                    flip: false,
                                },
                                aspect: a,
                                zoom: z,
                                center_x: 0.2 + 0.1 * (n % 7) as f64,
                                center_y: 0.9 - 0.1 * (n % 5) as f64,
                                color: Nv12ColorAdjust::default(),
                            };
                            println!(
                                "layout {n} {}",
                                layout(&compute_layout(c.0, c.1, &s, f.0, f.1))
                            );
                            n += 1;
                        }
                    }
                }
            }
        }
        let modes = [
            OutputSize::Auto,
            OutputSize::R2160,
            OutputSize::R1080,
            OutputSize::R720,
        ];
        let mut n = 0;
        for m in modes {
            for cw in [0u32, 1920, 1280] {
                for holds in [false, true] {
                    let p = plan_output(
                        m,
                        1920,
                        1080,
                        cw,
                        if cw != 0 { cw * 9 / 16 } else { 0 },
                        holds,
                    );
                    println!("plan {n} {} {} {}", p.width, p.height, p.rebuild as i32);
                    n += 1;
                }
            }
        }
    }
    // skin smoothing
    for strength in [1, 40, 100] {
        let (w, h) = (200u32, 160u32);
        let mut y = noise((w * h) as usize, 61);
        let mut sc = SkinScratch::default();
        smooth_face(
            &mut y,
            w as usize,
            w,
            h,
            &FaceRect {
                x: 0.3,
                y: 0.2,
                w: 0.3,
                h: 0.45,
            },
            strength,
            &mut sc,
        );
        out(&format!("skin {strength}"), &y);
    }
    // auto framing
    {
        let geo = FramingGeometry {
            frame_width: 3840.0,
            frame_height: 2160.0,
            picture_width: 3840.0,
            picture_height: 2160.0,
        };
        let mut f = AutoFramer::new();
        let mut next = 0.0;
        for i in 1..=600 {
            let t = i as f64 / 30.0;
            if t >= next {
                let mut faces = Vec::new();
                if t < 6.0 {
                    faces.push(FaceRect {
                        x: 0.45,
                        y: 0.3,
                        w: 0.1,
                        h: 0.2,
                    });
                } else if t < 12.0 {
                    faces.push(FaceRect {
                        x: 0.2,
                        y: 0.3,
                        w: 0.1,
                        h: 0.2,
                    });
                    faces.push(FaceRect {
                        x: 0.6,
                        y: 0.3,
                        w: 0.09,
                        h: 0.18,
                    });
                }
                f.on_faces(&faces, t);
                next += 0.2;
            }
            let v = f.step(&geo, t);
            if i % 30 == 0 {
                println!(
                    "frame {i} {} {} {}",
                    g(v.zoom),
                    g(v.center_x),
                    g(v.center_y)
                );
            }
        }
        let (dw, dh) = detection_size(880, 1920, 640);
        println!("detect {dw} {dh}");
        let src = noise(108 * 60, 71);
        let mut dst = vec![0u8; 37 * 23];
        downscale_luma(&src, 108, 100, 60, &mut dst, 37, 23);
        out("downscale", &dst);
    }
    // json
    {
        let docs = [
            r#"{"a":[1,2.5,-3e2,{"b":null}],"s":"xé😀\n","t":true}"#,
            "[]",
            "{}",
            "[0.1,1e21,123456789.5,0.000012345]",
        ];
        for d in docs {
            println!(
                "json {}",
                Value::parse(d).map(|v| v.dump()).unwrap_or_else(|e| e.0)
            );
        }
        for v in [
            0.5,
            1.0 / 3.0,
            1e20,
            1.5e-7,
            123456789.5,
            -0.25,
            1e15,
            999999999999999.0,
            0.1 + 0.2,
        ] {
            println!("num {}", Value::from(v).dump());
        }
    }
}
