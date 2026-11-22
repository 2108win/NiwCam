//! NV12 copy/scale helpers. Port of `src/common/Nv12.h`.
//!
//! The C++ passes raw pointers; here planes are slices. A plane slice starts at its first
//! pixel, so a rectangle inside a bigger frame is the frame split at the UV plane
//! (`split_at_mut`) and each half offset to the rectangle. `pitch` is bytes per row.
//! The SSE2 loops of the C++ are plain loops here: they computed the same values.

/// Black in limited-range video (what the virtual camera and the preview assume).
pub const BLACK_Y: u8 = 16;
pub const BLACK_UV: u8 = 128;

/// Brightness / contrast / saturation / colour temperature, each -100..100.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Nv12ColorAdjust {
    pub brightness: i32,
    pub contrast: i32,
    pub saturation: i32,
    /// `+` warmer (more red), `-` cooler (more blue).
    pub temperature: i32,
}

impl Nv12ColorAdjust {
    pub fn is_identity(&self) -> bool {
        self.brightness == 0 && self.contrast == 0 && self.saturation == 0 && self.temperature == 0
    }
}

/// out = (in - 128) * mul / 128 + add, saturated to 0..255.
fn adjust_value(v: u8, mul: i32, add: i32) -> u8 {
    ((((v as i32 - 128) * mul) >> 7) + add).clamp(0, 255) as u8
}

#[expect(clippy::too_many_arguments)]
fn adjust_rows(
    src: &[u8],
    src_pitch: usize,
    dst: &mut [u8],
    dst_pitch: usize,
    bytes: usize,
    rows: usize,
    mul: i32,
    add: [i32; 2],
) {
    for y in 0..rows {
        let s = &src[y * src_pitch..y * src_pitch + bytes];
        let d = &mut dst[y * dst_pitch..y * dst_pitch + bytes];
        for (x, (o, &i)) in d.iter_mut().zip(s).enumerate() {
            *o = adjust_value(i, mul, add[x & 1]);
        }
    }
}

fn adjust_rows_in_place(
    buf: &mut [u8],
    pitch: usize,
    bytes: usize,
    rows: usize,
    mul: i32,
    add: [i32; 2],
) {
    for y in 0..rows {
        for (x, v) in buf[y * pitch..y * pitch + bytes].iter_mut().enumerate() {
            *v = adjust_value(*v, mul, add[x & 1]);
        }
    }
}

/// The four fixed-point factors, as 16-bit values like the C++ (`int16_t`).
struct Factors {
    contrast: i32,
    saturation: i32,
    luma_add: i32,
    warm: i32,
}

fn factors(c: &Nv12ColorAdjust) -> Factors {
    // Multipliers are 1.7 fixed point (128 = 1.0) so (x-128)*mul fits int16.
    Factors {
        contrast: (128 + c.contrast * 102 / 100) as i16 as i32, // 0.2x .. 1.8x
        saturation: (128 + c.saturation * 128 / 100) as i16 as i32, // 0x .. 2x
        luma_add: 128 + (c.brightness * 64 / 100) as i16 as i32, // -64 .. +64
        warm: (c.temperature * 30 / 100) as i16 as i32,         // chroma shift
    }
}

/// Applies colour adjustments to a w x h picture whose planes sit anywhere (e.g. a rectangle
/// inside a bigger frame).
#[expect(clippy::too_many_arguments)]
pub fn nv12_adjust_planes(
    src_y: &[u8],
    src_uv: &[u8],
    src_pitch: usize,
    dst_y: &mut [u8],
    dst_uv: &mut [u8],
    dst_pitch: usize,
    w: u32,
    h: u32,
    c: &Nv12ColorAdjust,
) {
    let f = factors(c);
    let (w, h) = (w as usize, h as usize);
    adjust_rows(
        src_y,
        src_pitch,
        dst_y,
        dst_pitch,
        w,
        h,
        f.contrast,
        [f.luma_add; 2],
    );
    // Warmer = less blue (U down), more red (V up).
    adjust_rows(
        src_uv,
        src_pitch,
        dst_uv,
        dst_pitch,
        w,
        h / 2,
        f.saturation,
        [128 - f.warm, 128 + f.warm],
    );
}

/// The same, adjusting the planes where they are (`src == dst` in the C++).
pub fn nv12_adjust_planes_in_place(
    y: &mut [u8],
    uv: &mut [u8],
    pitch: usize,
    w: u32,
    h: u32,
    c: &Nv12ColorAdjust,
) {
    let f = factors(c);
    let (w, h) = (w as usize, h as usize);
    adjust_rows_in_place(y, pitch, w, h, f.contrast, [f.luma_add; 2]);
    adjust_rows_in_place(
        uv,
        pitch,
        w,
        h / 2,
        f.saturation,
        [128 - f.warm, 128 + f.warm],
    );
}

/// From `src` (pitch `src_pitch`, UV plane right after `h` rows) into a contiguous `dst`.
pub fn nv12_adjust(
    src: &[u8],
    src_pitch: usize,
    dst: &mut [u8],
    w: u32,
    h: u32,
    c: &Nv12ColorAdjust,
) {
    let (sy, suv) = src.split_at(src_pitch * h as usize);
    let (dy, duv) = dst.split_at_mut(w as usize * h as usize);
    nv12_adjust_planes(sy, suv, src_pitch, dy, duv, w as usize, w, h, c);
}

/// Adjusts a contiguous frame (pitch = w) in place.
pub fn nv12_adjust_in_place(frame: &mut [u8], w: u32, h: u32, c: &Nv12ColorAdjust) {
    let (y, uv) = frame.split_at_mut(w as usize * h as usize);
    nv12_adjust_planes_in_place(y, uv, w as usize, w, h, c);
}

/// Orientation applied by the host before frames reach the virtual camera. Rotation is
/// clockwise and happens first; mirror (left-right) and flip (top-bottom) then apply to the
/// rotated image, i.e. what the user sees.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Nv12Orientation {
    /// 0, 90, 180 or 270.
    pub rotation: i32,
    pub mirror: bool,
    pub flip: bool,
}

impl Nv12Orientation {
    pub fn is_identity(&self) -> bool {
        self.rotation == 0 && !self.mirror && !self.flip
    }

    pub fn swaps_size(&self) -> bool {
        self.rotation == 90 || self.rotation == 270
    }
}

/// Writes `src` (w x h, `src_pitch`) into a contiguous `dst` of the oriented size:
/// (h x w) for 90/270, otherwise (w x h). w and h must be even.
pub fn nv12_orient(
    src: &[u8],
    src_pitch: usize,
    w: u32,
    h: u32,
    dst: &mut [u8],
    o: &Nv12Orientation,
) {
    let ow = if o.swaps_size() { h } else { w } as usize;
    let oh = if o.swaps_size() { w } else { h } as usize;

    // Maps an output position (after undoing mirror/flip) to a source one, for a plane of
    // size sw x sh whose rotated size is dw x dh.
    let source = |mut x: usize,
                  mut y: usize,
                  sw: usize,
                  sh: usize,
                  dw: usize,
                  dh: usize|
     -> (usize, usize) {
        if o.mirror {
            x = dw - 1 - x;
        }
        if o.flip {
            y = dh - 1 - y;
        }
        match o.rotation {
            90 => (y, sh - 1 - x),
            180 => (sw - 1 - x, sh - 1 - y),
            270 => (sw - 1 - y, x),
            _ => (x, y),
        }
    };

    let (wu, hu) = (w as usize, h as usize);
    for y in 0..oh {
        for x in 0..ow {
            let (sx, sy) = source(x, y, wu, hu, ow, oh);
            dst[y * ow + x] = src[sy * src_pitch + sx];
        }
    }

    let src_uv = &src[src_pitch * hu..];
    let dst_uv = &mut dst[ow * oh..];
    let (cw, ch, ocw, och) = (wu / 2, hu / 2, ow / 2, oh / 2);
    for y in 0..och {
        for x in 0..ocw {
            let (sx, sy) = source(x, y, cw, ch, ocw, och);
            let s = sy * src_pitch + sx * 2;
            dst_uv[y * ow + x * 2] = src_uv[s];
            dst_uv[y * ow + x * 2 + 1] = src_uv[s + 1];
        }
    }
}

/// A source rectangle in pixels; fractional so zoom/pan move smoothly.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Nv12Rect {
    pub x: f64,
    pub y: f64,
    pub w: f64,
    pub h: f64,
}

#[derive(Clone, Copy)]
struct Tap {
    i0: usize,
    i1: usize,
    w1: u32, // 0..256
}

/// Per-thread buffers reused between calls (the C++ keeps them `thread_local` too), so scaling a
/// frame allocates nothing once the sizes have been seen.
#[derive(Default)]
struct Pool {
    taps_x: Vec<Tap>,
    taps_y: Vec<Tap>,
    columns: Vec<usize>,
    xmap: Vec<usize>,
    cxmap: Vec<usize>,
}

thread_local! {
    static POOL: std::cell::RefCell<Pool> = std::cell::RefCell::new(Pool::default());
}

fn fill_taps(out: &mut Vec<Tap>, n: u32, start: f64, len: f64, limit: u32) {
    let step = len / n as f64;
    let max = limit.saturating_sub(1) as f64;
    out.clear();
    out.extend((0..n).map(|i| {
        let mut f = start + (i as f64 + 0.5) * step - 0.5;
        if f < 0.0 {
            f = 0.0;
        }
        if f > max {
            f = max;
        }
        let i0 = f as u32;
        Tap {
            i0: i0 as usize,
            i1: if i0 + 1 < limit {
                i0 as usize + 1
            } else {
                i0 as usize
            },
            w1: ((f - i0 as f64) * 256.0 + 0.5) as u32,
        }
    }));
}

fn lerp2(a: u32, b: u32, wx: u32) -> u32 {
    a * (256 - wx) + b * wx
}

/// Bilinear-scales the `r` region of `src` (sw x sh, contiguous planes) into a dw x dh
/// picture whose planes start at `dst_y` / `dst_uv` with `dst_pitch` bytes per row: a whole
/// frame, or a rectangle inside a bigger one (fit with bars). Used for digital zoom and
/// fitting; 8-bit fixed-point weights.
#[expect(clippy::too_many_arguments)]
pub fn nv12_crop_scale_bilinear_to(
    src: &[u8],
    src_pitch: usize,
    sw: u32,
    sh: u32,
    r: &Nv12Rect,
    dst_y: &mut [u8],
    dst_uv: &mut [u8],
    dst_pitch: usize,
    dw: u32,
    dh: u32,
) {
    // Luma.
    let (mut xs, mut ys) =
        POOL.with_borrow_mut(|p| (std::mem::take(&mut p.taps_x), std::mem::take(&mut p.taps_y)));
    fill_taps(&mut xs, dw, r.x, r.w, sw);
    fill_taps(&mut ys, dh, r.y, r.h, sh);
    for (y, ty) in ys.iter().enumerate() {
        let a = &src[ty.i0 * src_pitch..];
        let b = &src[ty.i1 * src_pitch..];
        let wy = ty.w1;
        let d = &mut dst_y[y * dst_pitch..y * dst_pitch + dw as usize];
        for (o, t) in d.iter_mut().zip(&xs) {
            let top = lerp2(a[t.i0] as u32, a[t.i1] as u32, t.w1);
            let bot = lerp2(b[t.i0] as u32, b[t.i1] as u32, t.w1);
            *o = ((top * (256 - wy) + bot * wy + 32768) >> 16) as u8;
        }
    }

    // Chroma (interleaved UV at half resolution).
    let src_uv = &src[src_pitch * sh as usize..];
    fill_taps(&mut xs, dw / 2, r.x / 2.0, r.w / 2.0, sw / 2);
    fill_taps(&mut ys, dh / 2, r.y / 2.0, r.h / 2.0, sh / 2);
    for (y, ty) in ys.iter().enumerate() {
        let a = &src_uv[ty.i0 * src_pitch..];
        let b = &src_uv[ty.i1 * src_pitch..];
        let wy = ty.w1;
        let d = &mut dst_uv[y * dst_pitch..y * dst_pitch + dw as usize / 2 * 2];
        for (x, t) in xs.iter().enumerate() {
            for c in 0..2 {
                let top = lerp2(a[t.i0 * 2 + c] as u32, a[t.i1 * 2 + c] as u32, t.w1);
                let bot = lerp2(b[t.i0 * 2 + c] as u32, b[t.i1 * 2 + c] as u32, t.w1);
                d[x * 2 + c] = ((top * (256 - wy) + bot * wy + 32768) >> 16) as u8;
            }
        }
    }
    POOL.with_borrow_mut(|p| {
        p.taps_x = xs;
        p.taps_y = ys;
    });
}

/// Same into a contiguous dw x dh frame.
#[expect(clippy::too_many_arguments)]
pub fn nv12_crop_scale_bilinear(
    src: &[u8],
    src_pitch: usize,
    sw: u32,
    sh: u32,
    r: &Nv12Rect,
    dst: &mut [u8],
    dw: u32,
    dh: u32,
) {
    let (y, uv) = dst.split_at_mut(dw as usize * dh as usize);
    nv12_crop_scale_bilinear_to(src, src_pitch, sw, sh, r, y, uv, dw as usize, dw, dh);
}

/// Paints everything of a contiguous w x h frame outside the rectangle (rx, ry, rw, rh) black:
/// the bars around a picture fitted inside the frame. The rectangle's position and size are
/// even. The picture inside is left alone.
pub fn nv12_fill_bars(frame: &mut [u8], w: u32, h: u32, rx: u32, ry: u32, rw: u32, rh: u32) {
    #[expect(clippy::too_many_arguments)]
    fn plane(
        p: &mut [u8],
        row_bytes: usize,
        rows: usize,
        x0: usize,
        y0: usize,
        x1: usize,
        y1: usize,
        v: u8,
    ) {
        for y in 0..rows {
            let row = &mut p[y * row_bytes..(y + 1) * row_bytes];
            if y < y0 || y >= y1 {
                row.fill(v);
                continue;
            }
            row[..x0].fill(v);
            row[x1..].fill(v);
        }
    }
    let (wu, hu) = (w as usize, h as usize);
    let (rx, ry, rw, rh) = (rx as usize, ry as usize, rw as usize, rh as usize);
    let (y_plane, uv_plane) = frame.split_at_mut(wu * hu);
    plane(y_plane, wu, hu, rx, ry, rx + rw, ry + rh, BLACK_Y);
    // UV rows are w bytes too (interleaved pairs at half the height).
    plane(
        uv_plane,
        wu,
        hu / 2,
        rx,
        ry / 2,
        rx + rw,
        (ry + rh) / 2,
        BLACK_UV,
    );
}

/// BGRA (8 bits per channel, top-down, `pitch` bytes per row) -> NV12, BT.601 limited range
/// like the rest of the pipeline. w and h are even; chroma is the average of each 2x2 block.
pub fn bgra_to_nv12(
    bgra: &[u8],
    pitch: usize,
    w: u32,
    h: u32,
    dst_y: &mut [u8],
    dst_uv: &mut [u8],
    dst_pitch: usize,
) {
    let (w, h) = (w as usize, h as usize);
    for y in (0..h).step_by(2) {
        let r0 = &bgra[y * pitch..];
        let r1 = &bgra[(y + 1) * pitch..];
        for x in (0..w).step_by(2) {
            let p = [
                &r0[x * 4..],
                &r0[x * 4 + 4..],
                &r1[x * 4..],
                &r1[x * 4 + 4..],
            ];
            let (mut sb, mut sg, mut sr) = (0i32, 0i32, 0i32);
            for (i, px) in p.iter().enumerate() {
                let (b, g, r) = (px[0] as i32, px[1] as i32, px[2] as i32);
                sb += b;
                sg += g;
                sr += r;
                let luma = (((66 * r + 129 * g + 25 * b + 128) >> 8) + 16) as u8;
                let row = y + (i >> 1);
                dst_y[row * dst_pitch + x + (i & 1)] = luma;
            }
            // Averages with rounding; the sums of four fit easily in i32.
            let (b, g, r) = ((sb + 2) >> 2, (sg + 2) >> 2, (sr + 2) >> 2);
            let uv = (y / 2) * dst_pitch + x;
            dst_uv[uv] = (((-38 * r - 74 * g + 112 * b + 128) >> 8) + 128) as u8;
            dst_uv[uv + 1] = (((112 * r - 94 * g - 18 * b + 128) >> 8) + 128) as u8;
        }
    }
}

/// One BGRA pixel from Y, U, V (BT.601 limited range).
fn bgra_pixel(y: u8, u: u8, v: u8) -> u32 {
    let c = y as i32 - 16;
    let d = u as i32 - 128;
    let e = v as i32 - 128;
    let r = ((298 * c + 409 * e + 128) >> 8).clamp(0, 255) as u32;
    let g = ((298 * c - 100 * d - 208 * e + 128) >> 8).clamp(0, 255) as u32;
    let b = ((298 * c + 516 * d + 128) >> 8).clamp(0, 255) as u32;
    0xFF00_0000 | (r << 16) | (g << 8) | b
}

/// NV12 (w x h, planes `y_plane` / `uv_plane`, `pitch` bytes per row) -> BGRA of pw x ph,
/// nearest-neighbour (pw == w and ph == h converts 1:1). BT.601 limited range. Used for
/// on-screen pictures (the preview, a phone screen's window). `out` is pw x ph pixels.
#[expect(clippy::too_many_arguments)]
pub fn nv12_to_bgra(
    y_plane: &[u8],
    uv_plane: &[u8],
    pitch: usize,
    w: u32,
    h: u32,
    out: &mut [u32],
    pw: u32,
    ph: u32,
) {
    let (pwu, phu) = (pw as usize, ph as usize);
    let mut columns = POOL.with_borrow_mut(|p| std::mem::take(&mut p.columns));
    columns.clear();
    columns.extend((0..pw as u64).map(|x| (x * w as u64 / pw as u64) as usize));
    let mut last_sy = usize::MAX;
    for y in 0..phu {
        let sy = (y as u64 * h as u64 / ph as u64) as usize;
        if sy == last_sy {
            // Scaled up: the same source row again.
            out.copy_within((y - 1) * pwu..y * pwu, y * pwu);
            continue;
        }
        last_sy = sy;
        let row = &y_plane[sy * pitch..];
        let uv = &uv_plane[(sy / 2) * pitch..];
        for (o, &sx) in out[y * pwu..(y + 1) * pwu].iter_mut().zip(&columns) {
            let cx = sx & !1;
            *o = bgra_pixel(row[sx], uv[cx], uv[cx + 1]);
        }
    }
    POOL.with_borrow_mut(|p| p.columns = columns);
}

/// Copies or nearest-neighbour scales an NV12 image into separate Y and UV destinations (a
/// rectangle inside a larger frame, say). The source planes are contiguous: the UV plane
/// starts right after `sh` luma rows of `src_pitch` bytes.
#[expect(clippy::too_many_arguments)]
pub fn nv12_scale_to(
    src: &[u8],
    src_pitch: usize,
    sw: u32,
    sh: u32,
    dst: &mut [u8],
    dst_uv: &mut [u8],
    dst_pitch: usize,
    dw: u32,
    dh: u32,
) {
    let src_uv = &src[src_pitch * sh as usize..];
    let (dwu, dhu) = (dw as usize, dh as usize);

    if sw == dw && sh == dh {
        for y in 0..dhu {
            dst[y * dst_pitch..y * dst_pitch + dwu]
                .copy_from_slice(&src[y * src_pitch..y * src_pitch + dwu]);
        }
        for y in 0..dhu / 2 {
            dst_uv[y * dst_pitch..y * dst_pitch + dwu]
                .copy_from_slice(&src_uv[y * src_pitch..y * src_pitch + dwu]);
        }
        return;
    }

    let (mut xmap, mut cxmap) =
        POOL.with_borrow_mut(|p| (std::mem::take(&mut p.xmap), std::mem::take(&mut p.cxmap)));
    xmap.clear();
    xmap.extend((0..dw as u64).map(|x| (x * sw as u64 / dw as u64) as usize));
    cxmap.clear();
    cxmap
        .extend((0..(dw / 2) as u64).map(|x| (x * (sw / 2) as u64 / (dw / 2) as u64) as usize * 2));

    for y in 0..dhu {
        let s = &src[(y as u64 * sh as u64 / dh as u64) as usize * src_pitch..];
        for (o, &sx) in dst[y * dst_pitch..y * dst_pitch + dwu]
            .iter_mut()
            .zip(&xmap)
        {
            *o = s[sx];
        }
    }
    for y in 0..dhu / 2 {
        let s = &src_uv[(y as u64 * (sh / 2) as u64 / (dh / 2) as u64) as usize * src_pitch..];
        let d = &mut dst_uv[y * dst_pitch..];
        for (x, &cx) in cxmap.iter().enumerate() {
            d[2 * x] = s[cx];
            d[2 * x + 1] = s[cx + 1];
        }
    }
    POOL.with_borrow_mut(|p| {
        p.xmap = xmap;
        p.cxmap = cxmap;
    });
}

/// Same, with contiguous destination planes (the UV plane right after `dh` rows of `dst_pitch`).
#[expect(clippy::too_many_arguments)]
pub fn nv12_scale(
    src: &[u8],
    src_pitch: usize,
    sw: u32,
    sh: u32,
    dst: &mut [u8],
    dst_pitch: usize,
    dw: u32,
    dh: u32,
) {
    let (y, uv) = dst.split_at_mut(dst_pitch * dh as usize);
    nv12_scale_to(src, src_pitch, sw, sh, y, uv, dst_pitch, dw, dh);
}

/// Scales an NV12 picture into a frame of another size keeping its shape: centred, black bars
/// around (even position and size). Same size: a plain copy.
#[expect(clippy::too_many_arguments)]
pub fn nv12_fit_into(
    src: &[u8],
    src_pitch: usize,
    sw: u32,
    sh: u32,
    dst: &mut [u8],
    dst_pitch: usize,
    dw: u32,
    dh: u32,
) {
    if sw == 0 || sh == 0 || dw == 0 || dh == 0 {
        return;
    }
    if sw == dw && sh == dh {
        nv12_scale(src, src_pitch, sw, sh, dst, dst_pitch, dw, dh);
        return;
    }
    let (mut rw, mut rh) = (dw, dh);
    if sw as u64 * dh as u64 > dw as u64 * sh as u64 {
        rh = (dw as u64 * sh as u64 / sw as u64) as u32;
    } else {
        rw = (dh as u64 * sw as u64 / sh as u64) as u32;
    }
    rw = 2.max(rw & !1);
    rh = 2.max(rh & !1);
    let rx = (((dw - rw) / 2) & !1) as usize;
    let ry = (((dh - rh) / 2) & !1) as usize;
    let (dwu, dhu) = (dw as usize, dh as usize);
    let (rwu, rhu) = (rw as usize, rh as usize);
    let (y_plane, uv_plane) = dst.split_at_mut(dst_pitch * dhu);
    if rw != dw || rh != dh {
        // Bars: every row outside the rectangle, and the margins of the rows inside it.
        for y in 0..dhu {
            let row = &mut y_plane[y * dst_pitch..y * dst_pitch + dwu];
            if y < ry || y >= ry + rhu {
                row.fill(BLACK_Y);
                continue;
            }
            row[..rx].fill(BLACK_Y);
            row[rx + rwu..].fill(BLACK_Y);
        }
        for y in 0..dhu / 2 {
            let row = &mut uv_plane[y * dst_pitch..y * dst_pitch + dwu];
            if y < ry / 2 || y >= (ry + rhu) / 2 {
                row.fill(BLACK_UV);
                continue;
            }
            row[..rx].fill(BLACK_UV);
            row[rx + rwu..].fill(BLACK_UV);
        }
    }
    nv12_scale_to(
        src,
        src_pitch,
        sw,
        sh,
        &mut y_plane[ry * dst_pitch + rx..],
        &mut uv_plane[(ry / 2) * dst_pitch + rx..],
        dst_pitch,
        rw,
        rh,
    );
}
