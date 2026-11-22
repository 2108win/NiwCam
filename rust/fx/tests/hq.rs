//! Tests of the high-quality background chain (`background_hq`): pure functions, no Windows ML.

use niwcam_fx::background_hq::{blend_background_hq, refine_alpha, smooth_probability, HqScratch};
use niwcam_fx::segmenter::{blend_background, BackgroundScratch, K_SIZE};

const K: usize = K_SIZE as usize;

#[test]
fn smoothing_damps_noise_but_follows_real_change() {
    let mut state = Vec::new();
    let still = vec![0.9f32; K * K];
    smooth_probability(&mut state, &still, true);
    assert_eq!(state, still);
    // small noise around the value: the result moves less than the noise does
    let mut worst = 0.0f32;
    for t in 0..30 {
        let noisy: Vec<f32> = (0..K * K)
            .map(|i| 0.9 + if (i + t) % 2 == 0 { 0.06 } else { -0.06 })
            .collect();
        smooth_probability(&mut state, &noisy, false);
        worst = worst.max(state.iter().map(|v| (v - 0.9).abs()).fold(0.0, f32::max));
    }
    assert!(worst < 0.045, "noise passes through: {worst}");
    // the person moves away: a sure, big change takes over in a few looks
    let gone = vec![0.02f32; K * K];
    for _ in 0..3 {
        smooth_probability(&mut state, &gone, false);
    }
    assert!(
        state.iter().all(|&v| v < 0.2),
        "a real change is followed: {}",
        state[0]
    );
}

/// A frame whose luma has a vertical edge at `edge_x`, and a probability mask that puts the edge a
/// little to the right of it.
fn edge_case(w: usize, h: usize, edge_x: usize, mask_edge_cell: usize) -> (Vec<u8>, Vec<u8>) {
    let y: Vec<u8> = (0..w * h)
        .map(|i| if i % w < edge_x { 60 } else { 190 })
        .collect();
    let mut prob = vec![0u8; K * K];
    for j in 0..K {
        for i in 0..K {
            // a ramp of two cells, like a soft network output
            prob[j * K + i] = if i < mask_edge_cell {
                0
            } else if i == mask_edge_cell {
                90
            } else {
                255
            };
        }
    }
    (y, prob)
}

/// Where (in pixels of the alpha plane) the alpha crosses one half along row `row`.
fn crossing(alpha: &[u8], w: usize, row: usize) -> f32 {
    let r = &alpha[row * w..(row + 1) * w];
    for x in 1..w {
        if r[x - 1] < 128 && r[x] >= 128 {
            return x as f32 - 1.0 + (128.0 - r[x - 1] as f32) / (r[x] as f32 - r[x - 1] as f32);
        }
    }
    -1.0
}

#[test]
fn refinement_moves_the_edge_to_the_pictures_edge() {
    let (w, h) = (960usize, 540usize);
    let edge_x = 480;
    // The mask edge is one cell (5 px) to the right of the luma edge.
    let (y, prob) = edge_case(w, h, edge_x, edge_x / (w / K) + 1);
    let mut s = HqScratch::default();
    refine_alpha(&y, w, w as u32, h as u32, &prob, K, K, &mut s);
    let (aw, ah) = (s.alpha_w() as usize, s.alpha_h() as usize);
    let at = crossing(s.alpha(), aw, ah / 2) * (w as f32 / aw as f32);
    // The raw mask crosses one half at 488.6 px (cell 97 is 90, cell 98 is 255, cells are 5 px
    // wide): 8.6 px from the picture's edge. The guided filter pulls it towards the edge.
    let raw = 488.6f32;
    assert!(
        raw - at > 2.5,
        "the edge moved only from {raw} to {at}, picture edge at {edge_x}"
    );
    assert!(at > edge_x as f32 - 2.0, "and did not overshoot: {at}");
}

#[test]
fn blurred_background_has_no_halo_of_the_persons_colour() {
    let (w, h) = (640usize, 360usize);
    // A person-shaped block (a red shirt) in the middle of a grey wall.
    let inside = |x: usize, y: usize| (240..400).contains(&x) && (60..320).contains(&y);
    let luma: Vec<u8> = (0..w * h)
        .map(|i| if inside(i % w, i / w) { 120 } else { 150 })
        .collect();
    let mut chroma = vec![128u8; w * h / 2];
    for j in 0..h / 2 {
        for i in 0..w / 2 {
            if inside(2 * i, 2 * j) {
                chroma[j * w + 2 * i] = 90; // U
                chroma[j * w + 2 * i + 1] = 220; // V: red
            }
        }
    }
    let mut mask = vec![0u8; K * K];
    for j in 0..K {
        for i in 0..K {
            let (x, y) = (i * w / K, j * h / K);
            mask[j * K + i] = if inside(x + 1, y + 1) { 255 } else { 0 };
        }
    }
    // Chroma of the wall just outside the block, after blurring (V of the wall is 128).
    let halo = |chroma: &[u8]| {
        let mut worst = 0i32;
        for j in 25..(h / 2 - 25) {
            for i in (120 - 14)..(120 - 4) {
                worst = worst.max((chroma[j * w + 2 * i + 1] as i32 - 128).abs());
            }
        }
        worst
    };
    let (mut cy, mut cuv) = (luma.clone(), chroma.clone());
    let mut bs = BackgroundScratch::default();
    blend_background(
        &mut cy, &mut cuv, w, w as u32, h as u32, &mask, true, 100, None, &mut bs, false,
    );
    let (mut hy, mut huv) = (luma.clone(), chroma.clone());
    let mut hs = HqScratch::default();
    blend_background_hq(
        &mut hy, &mut huv, w, w as u32, h as u32, &mask, K, K, 100, None, &mut hs,
    );
    let (classic, hq) = (halo(&cuv), halo(&huv));
    assert!(
        hq <= classic,
        "the halo should not be worse: classic {classic}, hq {hq}"
    );
    assert!(hq <= 6, "no red leaks into the blurred wall: {hq}");
}

#[test]
fn bad_buffers_do_nothing() {
    let mut y = vec![0u8; 10];
    let mut uv = vec![0u8; 5];
    let mut s = HqScratch::default();
    blend_background_hq(
        &mut y,
        &mut uv,
        640,
        640,
        360,
        &vec![0u8; K * K],
        K,
        K,
        50,
        None,
        &mut s,
    );
    assert!(y.iter().all(|&v| v == 0));
}
