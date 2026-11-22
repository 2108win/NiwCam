//! Quality bench for the background upgrade (ADR-008), with a known ground truth.
//!
//! A synthetic scene (textured background, a person-shaped figure with fine hair strands, a
//! low-contrast stretch) gives the true alpha. The model is simulated: the true alpha at 192 x 192,
//! softened, with spatially smooth per-frame noise and a half-cell misregistration, exactly the kind
//! of output a real segmentation net gives. Both post-processings get the same sequence:
//!   classic = what ships now (half-and-half average, smoothstep 0.3..0.7, bilinear up);
//!   hq      = `background_hq` (uncertainty-weighted smoothing, guided filter on the luma).
//! Metrics, against the true alpha at half the picture size:
//!   MAD       mean absolute error over the picture, x1000
//!   edge MAD  the same inside the band around the true edge, x1000
//!   flicker   mean |alpha(t) - alpha(t-1)| in the still scene, x1000 (the true value is 0)
//!   moving    edge MAD while the figure moves 1.5 px per frame (lag and trails)
//! Run: cargo run --release -p niwcam-fx --example matte_bench

use niwcam_fx::background_hq::{refine_alpha_with, smooth_probability_with, HqParams, HqScratch};
use niwcam_fx::segmenter::{blend_background, classic_mask, BackgroundScratch, K_SIZE};

const W: usize = 960;
const H: usize = 540;
const K: usize = K_SIZE as usize;

struct Rng(u32);
impl Rng {
    fn next(&mut self) -> f32 {
        self.0 = self.0.wrapping_mul(1664525).wrapping_add(1013904223);
        (self.0 >> 8) as f32 / 16777216.0
    }
    fn signed(&mut self) -> f32 {
        self.next() * 2.0 - 1.0
    }
}

fn smooth_edge(sdf: f32) -> f32 {
    (0.5 - sdf).clamp(0.0, 1.0)
}

/// True alpha of the figure centred `shift` pixels to the right.
fn true_alpha(shift: f32) -> Vec<f32> {
    let cx = 480.0 + shift;
    let mut a = vec![0f32; W * H];
    // Hair strands: thin segments out of the head's top.
    let mut rng = Rng(7);
    let strands: Vec<(f32, f32, f32, f32)> = (0..70)
        .map(|_| {
            let ang = -std::f32::consts::PI * (0.05 + 0.9 * rng.next());
            let (x0, y0) = (cx + 66.0 * ang.cos(), 190.0 + 66.0 * ang.sin());
            let len = 14.0 + 26.0 * rng.next();
            (
                x0,
                y0,
                x0 + len * ang.cos() + 6.0 * rng.signed(),
                y0 + len * ang.sin(),
            )
        })
        .collect();
    for y in 0..H {
        for x in 0..W {
            let (fx, fy) = (x as f32 + 0.5, y as f32 + 0.5);
            // head (circle r 68), shoulders (ellipse), neck (rectangle)
            let head = (((fx - cx).powi(2) + (fy - 190.0).powi(2)).sqrt() - 68.0).min(1e3);
            let shoulders = {
                let (dx, dy) = ((fx - cx) / 210.0, (fy - 500.0) / 190.0);
                ((dx * dx + dy * dy).sqrt() - 1.0) * 190.0
            };
            let neck = (fx - cx).abs() - 34.0;
            let neck = if fy > 240.0 && fy < 330.0 { neck } else { 1e3 };
            let mut sdf = head.min(shoulders).min(neck);
            for &(x0, y0, x1, y1) in &strands {
                let (vx, vy) = (x1 - x0, y1 - y0);
                let t = (((fx - x0) * vx + (fy - y0) * vy) / (vx * vx + vy * vy)).clamp(0.0, 1.0);
                let d = ((fx - x0 - t * vx).powi(2) + (fy - y0 - t * vy).powi(2)).sqrt() - 0.9;
                sdf = sdf.min(d);
            }
            a[y * W + x] = smooth_edge(sdf);
        }
    }
    a
}

/// The luma of the scene: a textured background with a stretch that matches the person, the person's
/// own shading, sensor noise `sigma`.
fn luma(alpha: &[f32], sigma: f32, seed: u32) -> Vec<u8> {
    let mut rng = Rng(seed);
    (0..W * H)
        .map(|i| {
            let (x, y) = ((i % W) as f32, (i / W) as f32);
            // The left side of the background is as bright as the person: low contrast there.
            let bg =
                105.0 + 45.0 * (x / 37.0).sin() * (y / 29.0).sin() + (1.0 - x / W as f32) * 40.0;
            let fg = 150.0 + 25.0 * (x / 60.0).sin() + 15.0 * (y / 50.0).cos();
            let v = fg * alpha[i] + bg * (1.0 - alpha[i]) + sigma * rng.signed() * 1.7;
            v.clamp(0.0, 255.0) as u8
        })
        .collect()
}

/// What a segmentation net would say: the truth at K x K, softened, noisy, a half-cell off.
fn simulate_model(alpha: &[f32], rng: &mut Rng) -> Vec<f32> {
    let mut small = vec![0f32; K * K];
    let (sx, sy) = (W as f32 / K as f32, H as f32 / K as f32);
    for j in 0..K {
        for i in 0..K {
            // area average with a half-cell offset
            let (mut sum, mut n) = (0.0, 0.0);
            let x0 = (((i as f32 + 0.5) * sx) as usize).min(W - 1);
            let y0 = (((j as f32 + 0.5) * sy) as usize).min(H - 1);
            for dy in 0..sy as usize {
                for dx in 0..sx as usize {
                    sum += alpha[(y0 + dy).min(H - 1) * W + (x0 + dx).min(W - 1)];
                    n += 1.0;
                }
            }
            small[j * K + i] = sum / n;
        }
    }
    // soften twice with a 3 x 3 box
    for _ in 0..2 {
        let src = small.clone();
        for j in 0..K {
            for i in 0..K {
                let (mut s, mut n) = (0.0, 0.0);
                for dj in -1i32..=1 {
                    for di in -1i32..=1 {
                        let (jj, ii) = (j as i32 + dj, i as i32 + di);
                        if jj >= 0 && ii >= 0 && (jj as usize) < K && (ii as usize) < K {
                            s += src[jj as usize * K + ii as usize];
                            n += 1.0;
                        }
                    }
                }
                small[j * K + i] = s / n;
            }
        }
    }
    // spatially smooth noise: 32 x 32 values, bilinear
    let coarse: Vec<f32> = (0..32 * 32).map(|_| rng.signed() * 0.12).collect();
    for j in 0..K {
        for i in 0..K {
            let (fx, fy) = (
                i as f32 * 31.0 / (K - 1) as f32,
                j as f32 * 31.0 / (K - 1) as f32,
            );
            let (x0, y0) = (fx as usize, fy as usize);
            let (x1, y1) = ((x0 + 1).min(31), (y0 + 1).min(31));
            let (tx, ty) = (fx - x0 as f32, fy - y0 as f32);
            let n = (coarse[y0 * 32 + x0] * (1.0 - tx) + coarse[y0 * 32 + x1] * tx) * (1.0 - ty)
                + (coarse[y1 * 32 + x0] * (1.0 - tx) + coarse[y1 * 32 + x1] * tx) * ty;
            small[j * K + i] = (small[j * K + i] + n).clamp(0.0, 1.0);
        }
    }
    small
}

fn half(alpha: &[f32]) -> Vec<f32> {
    let (hw, hh) = (W / 2, H / 2);
    (0..hw * hh)
        .map(|k| {
            let (i, j) = (k % hw, k / hw);
            (alpha[2 * j * W + 2 * i]
                + alpha[2 * j * W + 2 * i + 1]
                + alpha[(2 * j + 1) * W + 2 * i]
                + alpha[(2 * j + 1) * W + 2 * i + 1])
                / 4.0
        })
        .collect()
}

/// A u8 plane of any size resampled (bilinear) to the half-picture grid, as 0..1.
fn to_half_grid(src: &[u8], sw: usize, sh: usize) -> Vec<f32> {
    let (hw, hh) = (W / 2, H / 2);
    (0..hw * hh)
        .map(|k| {
            let (i, j) = (k % hw, k / hw);
            let fx = ((i as f32 + 0.5) * sw as f32 / hw as f32 - 0.5).max(0.0);
            let fy = ((j as f32 + 0.5) * sh as f32 / hh as f32 - 0.5).max(0.0);
            let (x0, y0) = ((fx as usize).min(sw - 1), (fy as usize).min(sh - 1));
            let (x1, y1) = ((x0 + 1).min(sw - 1), (y0 + 1).min(sh - 1));
            let (tx, ty) = (fx - x0 as f32, fy - y0 as f32);
            let v = |x: usize, y: usize| src[y * sw + x] as f32 / 255.0;
            (v(x0, y0) * (1.0 - tx) + v(x1, y0) * tx) * (1.0 - ty)
                + (v(x0, y1) * (1.0 - tx) + v(x1, y1) * tx) * ty
        })
        .collect()
}

fn score(est: &[f32], truth: &[f32], band: &[bool]) -> (f64, f64) {
    let (mut all, mut edge, mut ne) = (0.0, 0.0, 0usize);
    for k in 0..est.len() {
        let d = (est[k] - truth[k]).abs() as f64;
        all += d;
        if band[k] {
            edge += d;
            ne += 1;
        }
    }
    (
        all / est.len() as f64 * 1000.0,
        edge / ne.max(1) as f64 * 1000.0,
    )
}

fn edge_band(truth: &[f32]) -> Vec<bool> {
    let (hw, hh) = (W / 2, H / 2);
    let mut band = vec![false; hw * hh];
    for j in 0..hh {
        for i in 0..hw {
            let t = truth[j * hw + i];
            if t > 0.01 && t < 0.99 {
                for dj in -4i32..=4 {
                    for di in -4i32..=4 {
                        let (jj, ii) = (j as i32 + dj, i as i32 + di);
                        if jj >= 0 && ii >= 0 && (jj as usize) < hh && (ii as usize) < hw {
                            band[jj as usize * hw + ii as usize] = true;
                        }
                    }
                }
            }
        }
    }
    band
}

struct Frame {
    y: Vec<u8>,
    truth: Vec<f32>,
    band: Vec<bool>,
    model: Vec<f32>,
}

fn scenario(moving: bool, noise: f32) -> Vec<Frame> {
    let mut rng = Rng(99);
    (0..40)
        .map(|t| {
            let truth_full = true_alpha(if moving { 1.5 * t as f32 } else { 0.0 });
            let y = luma(&truth_full, noise, 1000 + t as u32);
            let truth = half(&truth_full);
            let band = edge_band(&truth);
            let model = simulate_model(&truth_full, &mut rng);
            Frame {
                y,
                truth,
                band,
                model,
            }
        })
        .collect()
}

type Acc = (f64, f64, f64, usize);
type Triple = (f64, f64, f64);

/// (MAD, edge MAD, flicker) of the classic chain, frames 10.. of the scenario.
fn run_classic(frames: &[Frame]) -> (f64, f64, f64) {
    let mut state: Vec<f32> = Vec::new();
    let mut bs = BackgroundScratch::default();
    let mut dummy_y = vec![0u8; W * H];
    let mut dummy_uv = vec![128u8; W * H / 2];
    let mut acc: Acc = (0.0, 0.0, 0.0, 0);
    let mut prev: Vec<f32> = Vec::new();
    for (t, f) in frames.iter().enumerate() {
        let mask = classic_mask(&f.model, t == 0, &mut state);
        dummy_y.copy_from_slice(&f.y);
        blend_background(
            &mut dummy_y,
            &mut dummy_uv,
            W,
            W as u32,
            H as u32,
            &mask,
            true,
            0,
            None,
            &mut bs,
            false,
        );
        let est = to_half_grid(&bs.alpha, bs.alpha_w as usize, bs.alpha_h as usize);
        accumulate(&mut acc, &est, f, &mut prev, t);
    }
    finish(acc)
}

fn run_hq(frames: &[Frame], k: &HqParams) -> (f64, f64, f64) {
    let mut state: Vec<f32> = Vec::new();
    let mut hs = HqScratch::default();
    let mut acc: Acc = (0.0, 0.0, 0.0, 0);
    let mut prev: Vec<f32> = Vec::new();
    for (t, f) in frames.iter().enumerate() {
        smooth_probability_with(&mut state, &f.model, t == 0, k);
        let prob: Vec<u8> = state.iter().map(|&p| (p * 255.0 + 0.5) as u8).collect();
        refine_alpha_with(&f.y, W, W as u32, H as u32, &prob, &mut hs, k);
        let est = to_half_grid(hs.alpha(), hs.alpha_w() as usize, hs.alpha_h() as usize);
        accumulate(&mut acc, &est, f, &mut prev, t);
    }
    finish(acc)
}

fn accumulate(acc: &mut Acc, est: &[f32], f: &Frame, prev: &mut Vec<f32>, t: usize) {
    if t >= 10 {
        let (m, e) = score(est, &f.truth, &f.band);
        acc.0 += m;
        acc.1 += e;
        acc.3 += 1;
        if t > 10 {
            acc.2 += est
                .iter()
                .zip(prev.iter())
                .map(|(a, b)| (a - b).abs() as f64)
                .sum::<f64>()
                / est.len() as f64
                * 1000.0;
        }
    }
    *prev = est.to_vec();
}

fn finish(acc: Acc) -> (f64, f64, f64) {
    let n = acc.3 as f64;
    (acc.0 / n, acc.1 / n, acc.2 / (n - 1.0))
}

fn main() {
    let still = scenario(false, 2.0);
    let moving = scenario(true, 2.0);
    let sweep = std::env::args().any(|a| a == "--sweep");
    let report = |name: &str, label: &str, r: (f64, f64, f64)| {
        println!(
            "  {name:8} {label:7}  MAD {:6.2}   edge MAD {:6.2}   flicker {:6.2}",
            r.0, r.1, r.2
        )
    };
    let (cs, cm) = (run_classic(&still), run_classic(&moving));
    report("still", "classic", cs);
    report("moving", "classic", cm);
    if !sweep {
        let k = HqParams::default();
        println!("hq {k:?}");
        report("still", "hq", run_hq(&still, &k));
        report("moving", "hq", run_hq(&moving, &k));
        return;
    }
    // A coarse sweep, ranked by edge error in both scenes plus flicker.
    let mut best: Vec<(f64, HqParams, Triple, Triple)> = Vec::new();
    for base_weight in [0.5f32, 0.65, 0.8] {
        for radius_div in [25.0f32, 40.0, 60.0] {
            for eps in [2e-5f32, 6e-5, 2e-4] {
                for (lo, hi) in [(0.35f32, 0.65f32), (0.4, 0.6), (0.45, 0.55)] {
                    let k = HqParams {
                        base_weight,
                        radius_div,
                        eps,
                        lo,
                        hi,
                    };
                    let (a, b) = (run_hq(&still, &k), run_hq(&moving, &k));
                    best.push((a.1 + b.1 + 20.0 * (a.2 + b.2), k, a, b));
                }
            }
        }
    }
    best.sort_by(|x, y| x.0.partial_cmp(&y.0).unwrap());
    println!("classic cost {:.1}", cs.1 + cm.1 + 20.0 * (cs.2 + cm.2));
    for (cost, k, a, b) in best.iter().take(8) {
        println!("cost {cost:7.1}  {k:?}");
        println!(
            "      still {:.1}/{:.1}/{:.2}  moving {:.1}/{:.1}/{:.2}",
            a.0, a.1, a.2, b.0, b.1, b.2
        );
    }
}
