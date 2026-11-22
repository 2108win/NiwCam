//! "Nền" in high quality (ADR-008): the same person mask, but treated so the edge follows the real
//! picture and does not flicker, and the blurred background does not carry the person's colours.
//!
//! What is new against `segmenter::blend_background` (all of it needs no new model weights):
//! - the model's person probability is kept soft (no early hardening) and smoothed over time by how
//!   sure the new look is ([`smooth_probability`]), instead of a fixed half-and-half average;
//! - the 192 x 192 probability is refined at a quarter of the picture size with a guided filter
//!   (He et al.) that uses the picture's own luma, so the alpha edge snaps to the real edge and keeps
//!   a soft ramp (hair, shoulders) ([`refine_alpha`]);
//! - the background blur is a normalised convolution: only background pixels contribute, so there is
//!   no halo of skin and shirt colour around the person (Meet's published recipe: blur at low
//!   resolution, weighted by the mask);
//! - replacement gets a light wrap: the edge of the person takes a little of the new background's
//!   colour, so the cut-out does not look pasted on.
//!
//! Pure functions on slices (no Windows), so `tests/` and `examples/matte_bench.rs` can measure them.

use crate::segmenter::K_SIZE;

const K: usize = K_SIZE as usize;

fn smoothstep(e0: f32, e1: f32, x: f32) -> f32 {
    let t = ((x - e0) / (e1 - e0)).clamp(0.0, 1.0);
    t * t * (3.0 - 2.0 * t)
}

/// The knobs of the high-quality chain (the defaults are the tuned ones; `matte_bench` sweeps them).
#[derive(Clone, Copy, Debug)]
pub struct HqParams {
    /// Smoothing: the weight of a new look that changed little and is unsure.
    pub base_weight: f32,
    /// Guided filter radius = alpha height / this.
    pub radius_div: f32,
    /// Guided filter edge strength (smaller follows the picture's edges more).
    pub eps: f32,
    /// The ramp that turns the filtered value into alpha: below `lo` background, above `hi` person.
    pub lo: f32,
    pub hi: f32,
}

impl Default for HqParams {
    fn default() -> Self {
        Self {
            base_weight: 0.5,
            radius_div: 60.0,
            eps: 6e-5,
            lo: 0.35,
            hi: 0.65,
        }
    }
}

/// Blends the model's new probability (`cur`, 0..1, K x K) into the running one (`prev`). A small,
/// uncertain change (noise, a value near 0.5) moves the result a little; a big change to a sure
/// value (the person moved) takes over at once: no flicker in the still parts, no trail on the
/// moving ones. `reset`: start from `cur`.
pub fn smooth_probability(prev: &mut Vec<f32>, cur: &[f32], reset: bool) {
    smooth_probability_with(prev, cur, reset, &HqParams::default());
}

/// [`smooth_probability`] with explicit knobs.
pub fn smooth_probability_with(prev: &mut Vec<f32>, cur: &[f32], reset: bool, k: &HqParams) {
    if reset || prev.len() != cur.len() {
        prev.clear();
        prev.extend_from_slice(cur);
        return;
    }
    for (p, &c) in prev.iter_mut().zip(cur) {
        let change = (c - *p).abs();
        let sure = (2.0 * c - 1.0).abs();
        let w = k.base_weight
            + (1.0 - k.base_weight) * smoothstep(0.12, 0.5, change) * (0.4 + 0.6 * sure);
        *p += (c - *p) * w;
    }
}

/// Box mean of radius `r` over a w x h plane, edges clamped (two running-sum passes).
pub fn box_mean(src: &[f32], dst: &mut [f32], tmp: &mut [f32], w: usize, h: usize, r: usize) {
    let n = (2 * r + 1) as f32;
    let (wi, hi, ri) = (w as isize, h as isize, r as isize);
    for y in 0..h {
        let s = &src[y * w..(y + 1) * w];
        let t = &mut tmp[y * w..(y + 1) * w];
        let mut sum = 0.0f32;
        for x in -ri..=ri {
            sum += s[x.clamp(0, wi - 1) as usize];
        }
        for (x, o) in t.iter_mut().enumerate() {
            *o = sum / n;
            let x = x as isize;
            sum += s[(x + ri + 1).min(wi - 1) as usize] - s[(x - ri).max(0) as usize];
        }
    }
    for x in 0..w {
        let mut sum = 0.0f32;
        for y in -ri..=ri {
            sum += tmp[y.clamp(0, hi - 1) as usize * w + x];
        }
        for y in 0..h {
            dst[y * w + x] = sum / n;
            let yi = y as isize;
            sum += tmp[(yi + ri + 1).min(hi - 1) as usize * w + x]
                - tmp[(yi - ri).max(0) as usize * w + x];
        }
    }
}

/// Scratch buffers of [`refine_alpha`] and [`blend_background_hq`], kept between frames.
#[derive(Default)]
pub struct HqScratch {
    guide: Vec<f32>,
    p: Vec<f32>,
    mean_i: Vec<f32>,
    mean_p: Vec<f32>,
    corr_i: Vec<f32>,
    corr_ip: Vec<f32>,
    a: Vec<f32>,
    b: Vec<f32>,
    tmp: Vec<f32>,
    pub(crate) alpha: Vec<u8>,
    pub(crate) alpha_w: u32,
    pub(crate) alpha_h: u32,
    alpha_half: Vec<u8>,
    planes: Vec<f32>,
    weights: Vec<f32>,
    blur_a: Vec<f32>,
    blur_b: Vec<f32>,
    blur_tmp: Vec<f32>,
    tiny: Vec<u8>,
}

impl HqScratch {
    /// The refined alpha of the last [`refine_alpha`] / [`blend_background_hq`] (0..255).
    pub fn alpha(&self) -> &[u8] {
        &self.alpha
    }
    pub fn alpha_w(&self) -> u32 {
        self.alpha_w
    }
    pub fn alpha_h(&self) -> u32 {
        self.alpha_h
    }
}

/// The size the alpha is refined at: a quarter of the picture, at least 16 each way.
pub fn alpha_size(w: u32, h: u32) -> (u32, u32) {
    ((w / 4).max(16), (h / 4).max(16))
}

/// The luma of a picture box-averaged to lw x lh, as 0..1.
fn luma_small(y: &[u8], pitch: usize, w: u32, h: u32, lw: u32, lh: u32, out: &mut Vec<f32>) {
    out.resize(lw as usize * lh as usize, 0.0);
    for j in 0..lh {
        let y0 = (j * h / lh) as usize;
        let y1 = (((j + 1) * h / lh) as usize).max(y0 + 1);
        for i in 0..lw {
            let x0 = (i * w / lw) as usize;
            let x1 = (((i + 1) * w / lw) as usize).max(x0 + 1);
            let (mut sum, mut count) = (0u32, 0u32);
            let mut yy = y0;
            while yy < y1 {
                let row = &y[yy * pitch..];
                let mut xx = x0;
                while xx < x1 {
                    sum += row[xx] as u32;
                    count += 1;
                    xx += 2;
                }
                yy += 2;
            }
            out[j as usize * lw as usize + i as usize] = sum as f32 / count.max(1) as f32 / 255.0;
        }
    }
}

/// Bilinear K x K -> ow x oh, 0..1.
fn upsample_probability(prob: &[u8], ow: u32, oh: u32, out: &mut Vec<f32>) {
    out.resize(ow as usize * oh as usize, 0.0);
    let tap = |x: u32, o: u32| {
        let pos = ((x as f32 + 0.5) * K as f32 / o as f32 - 0.5).max(0.0);
        let i0 = (pos as usize).min(K - 1);
        (i0, (i0 + 1).min(K - 1), pos - i0 as f32)
    };
    let xt: Vec<(usize, usize, f32)> = (0..ow).map(|i| tap(i, ow)).collect();
    for j in 0..oh {
        let (j0, j1, fy) = tap(j, oh);
        let r0 = &prob[j0 * K..];
        let r1 = &prob[j1 * K..];
        for (i, o) in out[j as usize * ow as usize..(j as usize + 1) * ow as usize]
            .iter_mut()
            .enumerate()
        {
            let (i0, i1, fx) = xt[i];
            let top = r0[i0] as f32 * (1.0 - fx) + r0[i1] as f32 * fx;
            let bot = r1[i0] as f32 * (1.0 - fx) + r1[i1] as f32 * fx;
            *o = (top * (1.0 - fy) + bot * fy) / 255.0;
        }
    }
}

/// Guided filter (He et al. 2010) of `p` with the gray guide `guide`, in place of `p`.
fn guided_filter(s: &mut HqScratch, w: usize, h: usize, r: usize, eps: f32) {
    let n = w * h;
    for v in [
        &mut s.mean_i,
        &mut s.mean_p,
        &mut s.corr_i,
        &mut s.corr_ip,
        &mut s.a,
        &mut s.b,
        &mut s.tmp,
    ] {
        v.resize(n, 0.0);
    }
    let HqScratch {
        guide,
        p,
        mean_i,
        mean_p,
        corr_i,
        corr_ip,
        a,
        b,
        tmp,
        ..
    } = s;
    box_mean(guide, mean_i, tmp, w, h, r);
    box_mean(p, mean_p, tmp, w, h, r);
    // corr_i = mean(I*I), corr_ip = mean(I*p): products into `a` / `b` first.
    for k in 0..n {
        a[k] = guide[k] * guide[k];
        b[k] = guide[k] * p[k];
    }
    box_mean(a, corr_i, tmp, w, h, r);
    box_mean(b, corr_ip, tmp, w, h, r);
    for k in 0..n {
        let var = (corr_i[k] - mean_i[k] * mean_i[k]).max(0.0);
        let cov = corr_ip[k] - mean_i[k] * mean_p[k];
        let ak = cov / (var + eps);
        a[k] = ak;
        b[k] = mean_p[k] - ak * mean_i[k];
    }
    box_mean(a, corr_i, tmp, w, h, r); // mean a
    box_mean(b, corr_ip, tmp, w, h, r); // mean b
    for k in 0..n {
        p[k] = (corr_i[k] * guide[k] + corr_ip[k]).clamp(0.0, 1.0);
    }
}

/// The soft person alpha at [`alpha_size`], refined against the picture's luma. `prob` is the
/// K x K probability (0..255). The result is in `s.alpha` (0..255) at `s.alpha_w` x `s.alpha_h`.
pub fn refine_alpha(y: &[u8], pitch: usize, w: u32, h: u32, prob: &[u8], s: &mut HqScratch) {
    refine_alpha_with(y, pitch, w, h, prob, s, &HqParams::default());
}

/// [`refine_alpha`] with explicit knobs.
pub fn refine_alpha_with(
    y: &[u8],
    pitch: usize,
    w: u32,
    h: u32,
    prob: &[u8],
    s: &mut HqScratch,
    k: &HqParams,
) {
    let (lw, lh) = alpha_size(w, h);
    luma_small(y, pitch, w, h, lw, lh, &mut s.guide);
    upsample_probability(prob, lw, lh, &mut s.p);
    let r = ((lh as f32 / k.radius_div).round() as usize).max(2);
    guided_filter(s, lw as usize, lh as usize, r, k.eps);
    s.alpha.resize(lw as usize * lh as usize, 0);
    for (o, &v) in s.alpha.iter_mut().zip(&s.p) {
        // A little contrast, keeping the soft ramp: below 0.08 is background, above 0.92 person.
        *o = (smoothstep(k.lo, k.hi, v) * 255.0 + 0.5) as u8;
    }
    s.alpha_w = lw;
    s.alpha_h = lh;
}

#[derive(Clone, Copy, Default)]
struct Tap {
    i0: u32,
    i1: u32,
    w1: u32, // 0..256
}

fn make_tap(x: u32, out: u32, inn: u32) -> Tap {
    let pos = ((x as f64 + 0.5) * inn as f64 / out as f64 - 0.5).max(0.0);
    let i0 = (pos as u32).min(inn - 1);
    Tap {
        i0,
        i1: (i0 + 1).min(inn - 1),
        w1: ((pos - i0 as f64) * 256.0) as u32,
    }
}

fn bilinear(r0: &[u8], r1: &[u8], x: &Tap, wy: u32) -> u32 {
    let top = r0[x.i0 as usize] as u32 * (256 - x.w1) + r0[x.i1 as usize] as u32 * x.w1;
    let bottom = r1[x.i0 as usize] as u32 * (256 - x.w1) + r1[x.i1 as usize] as u32 * x.w1;
    (top * (256 - wy) + bottom * wy + (1 << 15)) >> 16
}

/// The blurred background as an NV12-like tiny picture (tw x th luma, interleaved UV at half size):
/// a normalised convolution, `blur(P * (1 - alpha)) / blur(1 - alpha)`, so the person's colours do not
/// leak into it. `strength` 0..100 sets the radius.
#[expect(clippy::too_many_arguments)]
fn blurred_background(
    y: &[u8],
    uv: &[u8],
    pitch: usize,
    w: u32,
    h: u32,
    tw: u32,
    th: u32,
    strength: i32,
    s: &mut HqScratch,
) {
    let (twu, thu) = (tw as usize, th as usize);
    let (cw, ch) = (twu / 2, thu / 2);
    // The tiny picture: box average of the source (luma) and of the chroma.
    let n = twu * thu;
    s.planes.resize(n + 2 * cw * ch, 0.0);
    s.weights.resize(n, 0.0);
    for j in 0..thu {
        let y0 = (j as u32 * h / th) as usize;
        let y1 = (((j as u32 + 1) * h / th) as usize).max(y0 + 1);
        for i in 0..twu {
            let x0 = (i as u32 * w / tw) as usize;
            let x1 = (((i as u32 + 1) * w / tw) as usize).max(x0 + 1);
            let (mut sum, mut count) = (0u32, 0u32);
            let mut yy = y0;
            while yy < y1 {
                let mut xx = x0;
                while xx < x1 {
                    sum += y[yy * pitch + xx] as u32;
                    count += 1;
                    xx += 2;
                }
                yy += 2;
            }
            s.planes[j * twu + i] = sum as f32 / count.max(1) as f32;
        }
    }
    let (sw, sh) = ((w / 2) as usize, (h / 2) as usize);
    for j in 0..ch {
        let y0 = j * sh / ch;
        let y1 = ((j + 1) * sh / ch).max(y0 + 1);
        for i in 0..cw {
            let x0 = i * sw / cw;
            let x1 = ((i + 1) * sw / cw).max(x0 + 1);
            let (mut su, mut sv, mut count) = (0u32, 0u32, 0u32);
            let mut yy = y0;
            while yy < y1 {
                let mut xx = x0;
                while xx < x1 {
                    su += uv[yy * pitch + 2 * xx] as u32;
                    sv += uv[yy * pitch + 2 * xx + 1] as u32;
                    count += 1;
                    xx += 2;
                }
                yy += 2;
            }
            let c = count.max(1) as f32;
            s.planes[n + j * cw + i] = su as f32 / c;
            s.planes[n + cw * ch + j * cw + i] = sv as f32 / c;
        }
    }
    // Background weight at the tiny size: 1 - alpha (alpha taken from the refined plane).
    let (aw, ah) = (s.alpha_w, s.alpha_h);
    for j in 0..th {
        let t = make_tap(j, th, ah);
        let r0 = &s.alpha[t.i0 as usize * aw as usize..];
        let r1 = &s.alpha[t.i1 as usize * aw as usize..];
        for i in 0..tw {
            let x = make_tap(i, tw, aw);
            let a = bilinear(r0, r1, &x, t.w1) as f32 / 255.0;
            s.weights[j as usize * twu + i as usize] = 1.0 - a;
        }
    }
    // Radius in tiny pixels: 1..3 (the tiny picture is already 8..40 source pixels per cell).
    let radius = 1 + (strength.clamp(0, 100) as usize * 2 / 100);
    let mut wd = std::mem::take(&mut s.blur_a);
    let mut wt = std::mem::take(&mut s.blur_b);
    let mut tmp = std::mem::take(&mut s.blur_tmp);
    wd.resize(n, 0.0);
    wt.resize(n, 0.0);
    tmp.resize(n.max(cw * ch), 0.0);
    // Three box passes ~ a Gaussian; the weight is blurred the same way.
    let mut weight_blur = s.weights.clone();
    for _ in 0..3 {
        box_mean(&weight_blur, &mut wd, &mut tmp, twu, thu, radius);
        std::mem::swap(&mut weight_blur, &mut wd);
    }
    let blur_plane = |plane: &[f32],
                      weights_here: &[f32],
                      pw: usize,
                      ph: usize,
                      wb_here: &[f32],
                      out: &mut [f32],
                      wd: &mut Vec<f32>,
                      wt: &mut Vec<f32>,
                      tmp: &mut Vec<f32>| {
        let len = pw * ph;
        wd.resize(len, 0.0);
        wt.resize(len, 0.0);
        let mut num: Vec<f32> = plane.iter().zip(weights_here).map(|(p, w)| p * w).collect();
        let mut plain: Vec<f32> = plane.to_vec();
        for _ in 0..3 {
            box_mean(&num, wd, tmp, pw, ph, radius.max(1));
            std::mem::swap(&mut num, wd);
            box_mean(&plain, wt, tmp, pw, ph, radius.max(1));
            std::mem::swap(&mut plain, wt);
        }
        for k in 0..len {
            // Where almost no background is near, fall back to the plain blur.
            let den = wb_here[k];
            out[k] = if den > 0.03 { num[k] / den } else { plain[k] };
        }
    };
    // Luma.
    let mut result_y = vec![0f32; n];
    blur_plane(
        &s.planes[..n],
        &s.weights,
        twu,
        thu,
        &weight_blur,
        &mut result_y,
        &mut wd,
        &mut wt,
        &mut tmp,
    );
    // Chroma: the weights at half size.
    let mut cw_weights = vec![0f32; cw * ch];
    for j in 0..ch {
        for i in 0..cw {
            let k = (2 * j) * twu + 2 * i;
            cw_weights[j * cw + i] =
                (s.weights[k] + s.weights[k + 1] + s.weights[k + twu] + s.weights[k + twu + 1])
                    * 0.25;
        }
    }
    // Chroma needs the blurred weight of its own grid: blur it the same three passes.
    let mut cwb = cw_weights.clone();
    let mut cwd = vec![0f32; cw * ch];
    for _ in 0..3 {
        box_mean(&cwb, &mut cwd, &mut tmp, cw, ch, radius.max(1));
        std::mem::swap(&mut cwb, &mut cwd);
    }
    let mut result_u = vec![0f32; cw * ch];
    let mut result_v = vec![0f32; cw * ch];
    let (mut wd2, mut wt2) = (vec![0f32; cw * ch], vec![0f32; cw * ch]);
    blur_plane(
        &s.planes[n..n + cw * ch],
        &cw_weights,
        cw,
        ch,
        &cwb,
        &mut result_u,
        &mut wd2,
        &mut wt2,
        &mut tmp,
    );
    blur_plane(
        &s.planes[n + cw * ch..],
        &cw_weights,
        cw,
        ch,
        &cwb,
        &mut result_v,
        &mut wd2,
        &mut wt2,
        &mut tmp,
    );
    // Back to bytes: tiny NV12.
    s.tiny.resize(n + 2 * cw * ch, 0);
    for k in 0..n {
        s.tiny[k] = (result_y[k] + 0.5).clamp(0.0, 255.0) as u8;
    }
    for k in 0..cw * ch {
        s.tiny[n + 2 * k] = (result_u[k] + 0.5).clamp(0.0, 255.0) as u8;
        s.tiny[n + 2 * k + 1] = (result_v[k] + 0.5).clamp(0.0, 255.0) as u8;
    }
    s.blur_a = wd;
    s.blur_b = wt;
    s.blur_tmp = tmp;
}

/// Blends the background of an NV12 picture with its blurred, halo-free copy (`strength` 0..100) or
/// with `image` (NV12, same size, with a light wrap), using the soft probability `prob` (K x K,
/// 0..255). Same buffer contract as `segmenter::blend_background`.
#[expect(clippy::too_many_arguments)]
pub fn blend_background_hq(
    y: &mut [u8],
    uv: &mut [u8],
    pitch: usize,
    w: u32,
    h: u32,
    prob: &[u8],
    strength: i32,
    image: Option<&[u8]>,
    s: &mut HqScratch,
) {
    if w < 16 || h < 16 || prob.len() != K * K || pitch < w as usize {
        return;
    }
    let need_y = (h as usize - 1) * pitch + w as usize;
    let need_uv = (h as usize / 2 - 1) * pitch + (w as usize / 2) * 2;
    if y.len() < need_y
        || uv.len() < need_uv
        || image.is_some_and(|i| i.len() < w as usize * h as usize * 3 / 2)
    {
        return;
    }
    refine_alpha(y, pitch, w, h, prob, s);
    let (aw, ah) = (s.alpha_w, s.alpha_h);

    let (mut tw, mut th) = (0u32, 0u32);
    if image.is_none() {
        let block = (8.0 + strength.clamp(0, 100) as f64 * 0.32) * h as f64 / 1080.0;
        tw = 4.max((w as f64 / block.max(2.0)) as u32) & !1;
        th = 4.max((h as f64 / block.max(2.0)) as u32) & !1;
        blurred_background(y, uv, pitch, w, h, tw, th, strength, s);
    }
    let n = tw as usize * th as usize;
    let (ty, tuv): (&[u8], &[u8]) = if image.is_some() {
        (&[], &[])
    } else {
        s.tiny.split_at(n)
    };
    let xb: Vec<Tap> = if image.is_some() {
        Vec::new()
    } else {
        (0..w).map(|i| make_tap(i, w, tw)).collect()
    };
    let (hw, hh) = (w / 2, h / 2);
    let xac: Vec<Tap> = (0..hw).map(|i| make_tap(i, hw, aw)).collect();
    let xbc: Vec<Tap> = if image.is_some() {
        Vec::new()
    } else {
        (0..hw).map(|i| make_tap(i, hw, tw / 2)).collect()
    };

    // The alpha at half the picture's size (one value per chroma sample, per 2 x 2 luma): what every
    // pixel row reads, like the classic chain. Smooth already, so 2 x 2 steps do not show.
    s.alpha_half.resize(hw as usize * hh as usize, 0);
    for j in 0..hh {
        let ta = make_tap(j, hh, ah);
        let a0 = &s.alpha[ta.i0 as usize * aw as usize..];
        let a1 = &s.alpha[ta.i1 as usize * aw as usize..];
        let dst = &mut s.alpha_half[j as usize * hw as usize..(j as usize + 1) * hw as usize];
        for (i, d) in dst.iter_mut().enumerate() {
            *d = bilinear(a0, a1, &xac[i], ta.w1) as u8;
        }
    }
    let alpha_half = &s.alpha_half;
    // Luma.
    for yy in 0..h {
        let arow = &alpha_half[(yy / 2) as usize * hw as usize..];
        let row = &mut y[yy as usize * pitch..yy as usize * pitch + w as usize];
        if let Some(image) = image {
            let bg = &image[yy as usize * w as usize..];
            for x in 0..w as usize {
                let a = arow[x / 2] as u32;
                let a = a + (a >> 7);
                if a == 256 {
                    continue; // the person, nowhere near an edge: untouched
                }
                if a == 0 {
                    row[x] = bg[x];
                    continue;
                }
                // Light wrap: at the edge the person takes a little of the new background.
                let edge = (a * (256 - a)) >> 8; // 0..64
                let fg = wrap(row[x] as u32, bg[x] as u32, edge);
                row[x] = ((fg * a + bg[x] as u32 * (256 - a)) >> 8) as u8;
            }
        } else {
            let tb = make_tap(yy, h, th);
            let b0 = &ty[tb.i0 as usize * tw as usize..];
            let b1 = &ty[tb.i1 as usize * tw as usize..];
            for x in 0..w as usize {
                let a = arow[x / 2] as u32;
                let a = a + (a >> 7);
                if a == 256 {
                    continue;
                }
                let bg = bilinear(b0, b1, &xb[x], tb.w1);
                row[x] = ((row[x] as u32 * a + bg * (256 - a)) >> 8) as u8;
            }
        }
    }
    // Chroma.
    for cy in 0..hh {
        let arow = &alpha_half[cy as usize * hw as usize..];
        let crow = &mut uv[cy as usize * pitch..cy as usize * pitch + hw as usize * 2];
        if let Some(image) = image {
            let bg = &image[w as usize * h as usize + cy as usize * w as usize..];
            for x in 0..hw as usize {
                let a = arow[x] as u32;
                let a = a + (a >> 7);
                if a == 256 {
                    continue;
                }
                let edge = (a * (256 - a)) >> 8;
                for c in 0..2 {
                    let f = crow[2 * x + c] as u32;
                    let b = bg[2 * x + c] as u32;
                    let fg = wrap(f, b, edge);
                    crow[2 * x + c] = ((fg * a + b * (256 - a)) >> 8) as u8;
                }
            }
        } else {
            let tb = make_tap(cy, hh, th / 2);
            let b0 = &tuv[tb.i0 as usize * tw as usize..];
            let b1 = &tuv[tb.i1 as usize * tw as usize..];
            for x in 0..hw as usize {
                let a = arow[x] as u32;
                let a = a + (a >> 7);
                if a == 256 {
                    continue;
                }
                let xt = &xbc[x];
                for c in 0..2usize {
                    let top = b0[2 * xt.i0 as usize + c] as u32 * (256 - xt.w1)
                        + b0[2 * xt.i1 as usize + c] as u32 * xt.w1;
                    let bottom = b1[2 * xt.i0 as usize + c] as u32 * (256 - xt.w1)
                        + b1[2 * xt.i1 as usize + c] as u32 * xt.w1;
                    let bg = (top * (256 - tb.w1) + bottom * tb.w1 + (1 << 15)) >> 16;
                    crow[2 * x + c] = ((crow[2 * x + c] as u32 * a + bg * (256 - a)) >> 8) as u8;
                }
            }
        }
    }
}

/// The person's value with a light wrap: pulled towards the new background by `edge` (0..64 of 256),
/// at most a quarter of the way.
fn wrap(fg: u32, bg: u32, edge: u32) -> u32 {
    let amount = edge.min(64) as i32; // 0..64 of 256 = at most 25 %
    (fg as i32 + (((bg as i32 - fg as i32) * amount) >> 8)).clamp(0, 255) as u32
}
