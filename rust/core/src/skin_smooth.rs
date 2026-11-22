//! "Làm mịn da": an edge-preserving smoothing (a self-guided filter, He et al. 2010) of the luma
//! inside each face, faded out over an ellipse around it. Eyes, brows and the face's outline
//! keep their edges (high local variance); even skin is evened out. Chroma is left alone.
//! Port of `src/engine/SkinSmooth.h`.

use crate::auto_frame::FaceRect;
use crate::util::clamp;

/// Buffers kept between calls so a frame allocates nothing.
#[derive(Clone, Debug, Default)]
pub struct SkinScratch {
    i: Vec<f32>,
    ii: Vec<f32>,
    a: Vec<f32>,
    b: Vec<f32>,
    tmp: Vec<f32>,
}

/// Box mean of radius `r` over a w x h float plane, edges clamped (two 1-D passes, running sums).
fn box_mean(src: &[f32], dst: &mut [f32], tmp: &mut [f32], w: usize, h: usize, r: usize) {
    let (wi, hi, ri) = (w as isize, h as isize, r as isize);
    let at = |x: isize| clamp(x, 0, wi - 1) as usize;
    let n = (2 * r + 1) as f64;
    for y in 0..h {
        let s = &src[y * w..(y + 1) * w];
        let t = &mut tmp[y * w..(y + 1) * w];
        let mut sum = 0.0f64;
        for x in -ri..=ri {
            sum += s[at(x)] as f64;
        }
        for (x, o) in t.iter_mut().enumerate() {
            *o = (sum / n) as f32;
            let x = x as isize;
            sum += (s[(x + ri + 1).min(wi - 1) as usize] - s[(x - ri).max(0) as usize]) as f64;
        }
    }
    let aty = |y: isize| clamp(y, 0, hi - 1) as usize;
    for x in 0..w {
        let mut sum = 0.0f64;
        for y in -ri..=ri {
            sum += tmp[aty(y) * w + x] as f64;
        }
        for y in 0..h {
            dst[y * w + x] = (sum / n) as f32;
            let yi = y as isize;
            sum += (tmp[(yi + ri + 1).min(hi - 1) as usize * w + x]
                - tmp[(yi - ri).max(0) as usize * w + x]) as f64;
        }
    }
}

/// `y`: the luma plane of a w x h picture (`pitch` bytes a row). `face`: normalised to that
/// picture. `strength` 1..100.
pub fn smooth_face(
    y: &mut [u8],
    pitch: usize,
    w: u32,
    h: u32,
    face: &FaceRect,
    strength: i32,
    s: &mut SkinScratch,
) {
    if strength <= 0 || w < 16 || h < 16 {
        return;
    }
    let (wf, hf) = (w as f64, h as f64);
    // The face box, wider and taller (forehead, cheeks, chin), as the ellipse's bounds.
    let cx = (face.x + face.w / 2.0) * wf;
    let cy = (face.y + face.h * 0.55) * hf;
    let rx = face.w * wf * 0.75;
    let ry = face.h * hf * 0.85;
    let x0 = ((cx - rx) as i32).max(0);
    let x1 = ((cx + rx) as i32).min(w as i32);
    let y0 = ((cy - ry) as i32).max(0);
    let y1 = ((cy + ry) as i32).min(h as i32);
    let (rw, rh) = (x1 - x0, y1 - y0);
    if rw < 8 || rh < 8 {
        return;
    }
    let (x0, y0, rw, rh) = (x0 as usize, y0 as usize, rw as usize, rh as usize);

    let n = rw * rh;
    for v in [&mut s.i, &mut s.ii, &mut s.a, &mut s.b, &mut s.tmp] {
        v.resize(n, 0.0);
    }
    for yy in 0..rh {
        let row = &y[(y0 + yy) * pitch + x0..];
        for xx in 0..rw {
            let v = row[xx] as f32;
            s.i[yy * rw + xx] = v;
            s.ii[yy * rw + xx] = v * v;
        }
    }
    // The radius follows the face's size; eps (what counts as "even") follows the strength.
    let r = 2.max((face.h * hf / 45.0) as i32) as usize;
    let sigma = 3.0f32 + 0.30f32 * clamp(strength, 0, 100) as f32; // in luma steps
    let eps = sigma * sigma;
    box_mean(&s.i, &mut s.a, &mut s.tmp, rw, rh, r); // a = mean I
    box_mean(&s.ii, &mut s.b, &mut s.tmp, rw, rh, r); // b = mean I²
    for k in 0..n {
        let mean = s.a[k];
        let var = (s.b[k] - mean * mean).max(0.0);
        let ak = var / (var + eps);
        s.a[k] = ak;
        s.b[k] = mean - ak * mean;
    }
    box_mean(&s.a, &mut s.ii, &mut s.tmp, rw, rh, r); // mean a (reuses ii)
    box_mean(&s.b, &mut s.a, &mut s.tmp, rw, rh, r); // mean b
    let amount = (0.35f32 + 0.65f32 * strength as f32 / 100.0f32).min(1.0);
    for yy in 0..rh {
        let row = &mut y[(y0 + yy) * pitch + x0..(y0 + yy) * pitch + x0 + rw];
        let dy = ((y0 + yy) as f64 + 0.5 - cy) / ry;
        for (xx, px) in row.iter_mut().enumerate() {
            let dx = ((x0 + xx) as f64 + 0.5 - cx) / rx;
            let d = (dx * dx + dy * dy).sqrt();
            if d >= 1.0 {
                continue;
            }
            // Full inside 70 % of the ellipse, fading to nothing at its edge.
            let t = clamp((1.0 - d) / 0.3, 0.0, 1.0);
            let weight = amount * (t * t * (3.0 - 2.0 * t)) as f32;
            let k = yy * rw + xx;
            let q = s.ii[k] * s.i[k] + s.a[k];
            let v = s.i[k] + weight * (q - s.i[k]);
            *px = (v.round() as i64).clamp(0, 255) as u8;
        }
    }
}
