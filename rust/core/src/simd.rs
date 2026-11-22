//! The two per-frame inner loops that were SSE2 in the C++ (`Nv12.h`): colour adjust and
//! NV12 -> BGRA. SSE2 is part of x86-64, so no run-time detection is needed. Other targets use the
//! plain loops. `tests` in `nv12.rs` and `xcheck` compare both with the reference.
//!
//! This is the only module of `niwcam-core` with `unsafe` (ADR-003, ADR-007): raw loads and stores of
//! 16 bytes, each inside the bounds the loop checks.
#![allow(unsafe_code)]

/// out = (in - 128) * mul / 128 + add, per 16-bit lane, saturated to 0..255. `add` alternates per
/// byte (U and V offsets alternate in the chroma plane).
pub fn adjust_row(src: &[u8], dst: &mut [u8], mul: i16, add: [i16; 2]) {
    let n = src.len().min(dst.len());
    let done = adjust_wide(src.as_ptr(), dst.as_mut_ptr(), n, mul, add);
    for (x, (&i, o)) in src[..n]
        .iter()
        .zip(dst[..n].iter_mut())
        .enumerate()
        .skip(done)
    {
        *o = scalar_adjust(i, mul, add[x & 1]);
    }
}

/// The same, on the row where it is.
pub fn adjust_row_in_place(row: &mut [u8], mul: i16, add: [i16; 2]) {
    let n = row.len();
    // Each 16-byte group is loaded before it is stored back to the same place.
    let done = adjust_wide(row.as_ptr(), row.as_mut_ptr(), n, mul, add);
    for (x, o) in row.iter_mut().enumerate().skip(done) {
        *o = scalar_adjust(*o, mul, add[x & 1]);
    }
}

#[inline(always)]
fn scalar_adjust(v: u8, mul: i16, add: i16) -> u8 {
    (((v as i16 - 128).wrapping_mul(mul)) >> 7)
        .wrapping_add(add)
        .clamp(0, 255) as u8
}

/// The whole 16-byte groups of `len` bytes; returns how many bytes were done. `src` and `dst` are
/// valid for `len` bytes and may be the same place.
#[cfg(target_arch = "x86_64")]
fn adjust_wide(src: *const u8, dst: *mut u8, len: usize, mul: i16, add: [i16; 2]) -> usize {
    use std::arch::x86_64::*;
    let wide = len / 16 * 16;
    // SAFETY: SSE2 is baseline on x86-64; every load/store is 16 bytes inside the first `wide`
    // bytes of `src` / `dst`, which the callers took from slices of at least `len` bytes.
    unsafe {
        let zero = _mm_setzero_si128();
        let bias = _mm_set1_epi16(128);
        let m = _mm_set1_epi16(mul);
        let a = _mm_setr_epi16(
            add[0], add[1], add[0], add[1], add[0], add[1], add[0], add[1],
        );
        let mut x = 0;
        while x < wide {
            let v = _mm_loadu_si128(src.add(x).cast());
            let mut lo = _mm_sub_epi16(_mm_unpacklo_epi8(v, zero), bias);
            let mut hi = _mm_sub_epi16(_mm_unpackhi_epi8(v, zero), bias);
            lo = _mm_add_epi16(_mm_srai_epi16(_mm_mullo_epi16(lo, m), 7), a);
            hi = _mm_add_epi16(_mm_srai_epi16(_mm_mullo_epi16(hi, m), 7), a);
            _mm_storeu_si128(dst.add(x).cast(), _mm_packus_epi16(lo, hi));
            x += 16;
        }
    }
    wide
}

#[cfg(not(target_arch = "x86_64"))]
fn adjust_wide(_src: *const u8, _dst: *mut u8, _len: usize, _mul: i16, _add: [i16; 2]) -> usize {
    0
}

/// One BGRA pixel from Y, U, V (BT.601 limited range).
#[inline(always)]
pub fn bgra_pixel(y: u8, u: u8, v: u8) -> u32 {
    let c = y as i32 - 16;
    let d = u as i32 - 128;
    let e = v as i32 - 128;
    let r = ((298 * c + 409 * e + 128) >> 8).clamp(0, 255) as u32;
    let g = ((298 * c - 100 * d - 208 * e + 128) >> 8).clamp(0, 255) as u32;
    let b = ((298 * c + 516 * d + 128) >> 8).clamp(0, 255) as u32;
    0xFF00_0000 | (r << 16) | (g << 8) | b
}

/// One output row from its gathered Y, U and V samples (all `out.len()` long): 8 pixels at a time
/// with 32-bit sums, the same output as the per-pixel version bit for bit.
pub fn row_to_bgra(ly: &[u8], lu: &[u8], lv: &[u8], out: &mut [u32]) {
    let n = out.len().min(ly.len()).min(lu.len()).min(lv.len());
    let done = row_wide(&ly[..n], &lu[..n], &lv[..n], &mut out[..n]);
    for x in done..n {
        out[x] = bgra_pixel(ly[x], lu[x], lv[x]);
    }
}

#[cfg(target_arch = "x86_64")]
fn row_wide(ly: &[u8], lu: &[u8], lv: &[u8], out: &mut [u32]) -> usize {
    use std::arch::x86_64::*;
    let n = out.len();
    let wide = n / 8 * 8;
    // SAFETY: SSE2 is baseline on x86-64; each iteration reads 8 bytes from each plane and writes 8
    // pixels, all below `wide <= n`.
    unsafe {
        let zero = _mm_setzero_si128();
        let k16 = _mm_set1_epi16(16);
        let k128 = _mm_set1_epi16(128);
        let one = _mm_set1_epi16(1);
        let round = _mm_set1_epi32(128);
        let k_r = _mm_setr_epi16(298, 409, 298, 409, 298, 409, 298, 409); // (c, e)
        let k_b = _mm_setr_epi16(298, 516, 298, 516, 298, 516, 298, 516); // (c, d)
        let k_g1 = _mm_setr_epi16(298, -100, 298, -100, 298, -100, 298, -100); // (c, d)
        let k_g2 = _mm_setr_epi16(-208, 128, -208, 128, -208, 128, -208, 128); // (e, 1): + rounding
        let alpha = _mm_set1_epi8(-1);
        // Four 32-bit sums >> 8 for each half, packed to 8 bytes clamped to 0..255.
        let channel = |lo: __m128i, hi: __m128i| {
            let v16 = _mm_packs_epi32(_mm_srai_epi32(lo, 8), _mm_srai_epi32(hi, 8));
            _mm_packus_epi16(v16, v16)
        };
        let load =
            |p: &[u8], x: usize| _mm_unpacklo_epi8(_mm_loadl_epi64(p.as_ptr().add(x).cast()), zero);
        let mut x = 0;
        while x < wide {
            let c = _mm_sub_epi16(load(ly, x), k16);
            let d = _mm_sub_epi16(load(lu, x), k128);
            let e = _mm_sub_epi16(load(lv, x), k128);
            let ce_lo = _mm_unpacklo_epi16(c, e);
            let ce_hi = _mm_unpackhi_epi16(c, e);
            let cd_lo = _mm_unpacklo_epi16(c, d);
            let cd_hi = _mm_unpackhi_epi16(c, d);
            let e1_lo = _mm_unpacklo_epi16(e, one);
            let e1_hi = _mm_unpackhi_epi16(e, one);
            let r = channel(
                _mm_add_epi32(_mm_madd_epi16(ce_lo, k_r), round),
                _mm_add_epi32(_mm_madd_epi16(ce_hi, k_r), round),
            );
            let b = channel(
                _mm_add_epi32(_mm_madd_epi16(cd_lo, k_b), round),
                _mm_add_epi32(_mm_madd_epi16(cd_hi, k_b), round),
            );
            let g = channel(
                _mm_add_epi32(_mm_madd_epi16(cd_lo, k_g1), _mm_madd_epi16(e1_lo, k_g2)),
                _mm_add_epi32(_mm_madd_epi16(cd_hi, k_g1), _mm_madd_epi16(e1_hi, k_g2)),
            );
            let bg = _mm_unpacklo_epi8(b, g); // B G pairs
            let ra = _mm_unpacklo_epi8(r, alpha);
            let o = out.as_mut_ptr().add(x);
            _mm_storeu_si128(o.cast(), _mm_unpacklo_epi16(bg, ra));
            _mm_storeu_si128(o.add(4).cast(), _mm_unpackhi_epi16(bg, ra));
            x += 8;
        }
    }
    wide
}

#[cfg(not(target_arch = "x86_64"))]
fn row_wide(_ly: &[u8], _lu: &[u8], _lv: &[u8], _out: &mut [u32]) -> usize {
    0
}

#[cfg(test)]
mod tests {
    use super::*;

    fn noise(n: usize, mut s: u32) -> Vec<u8> {
        (0..n)
            .map(|_| {
                s = s.wrapping_mul(1664525).wrapping_add(1013904223);
                (s >> 24) as u8
            })
            .collect()
    }

    #[test]
    fn adjust_matches_the_scalar_formula() {
        for len in [0usize, 1, 7, 16, 17, 33, 1280] {
            let src = noise(len, 3);
            for (mul, add) in [
                (128i16, [128i16, 128]),
                (230, [160, 96]),
                (0, [128, 128]),
                (256, [100, 156]),
            ] {
                let mut got = vec![0u8; len];
                adjust_row(&src, &mut got, mul, add);
                let want: Vec<u8> = src
                    .iter()
                    .enumerate()
                    .map(|(x, &v)| scalar_adjust(v, mul, add[x & 1]))
                    .collect();
                assert_eq!(got, want, "len {len} mul {mul}");
                let mut inplace = src.clone();
                adjust_row_in_place(&mut inplace, mul, add);
                assert_eq!(inplace, want);
            }
        }
    }

    #[test]
    fn bgra_row_matches_the_pixel_formula() {
        for len in [0usize, 1, 7, 8, 9, 100, 1281] {
            let (y, u, v) = (noise(len, 1), noise(len, 2), noise(len, 3));
            let mut got = vec![0u32; len];
            row_to_bgra(&y, &u, &v, &mut got);
            let want: Vec<u32> = (0..len).map(|x| bgra_pixel(y[x], u[x], v[x])).collect();
            assert_eq!(got, want, "len {len}");
        }
        // Every luma with the most extreme chroma: the clamps at 0 and 255.
        let y: Vec<u8> = (0..=255).collect();
        for (u, v) in [(0u8, 255u8), (255, 0), (0, 0), (255, 255)] {
            let (lu, lv) = (vec![u; 256], vec![v; 256]);
            let mut got = vec![0u32; 256];
            row_to_bgra(&y, &lu, &lv, &mut got);
            let want: Vec<u32> = (0..256).map(|x| bgra_pixel(y[x], u, v)).collect();
            assert_eq!(got, want);
        }
    }
}
