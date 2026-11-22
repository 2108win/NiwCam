//! "Nền" (background effects): finds the person in the picture with a small segmentation model
//! (PP-HumanSeg, OpenCV Zoo, Apache 2.0) through Windows ML, on its own thread, ~15 times a
//! second; the capture thread blends every frame with the latest mask ([`blend_background`]).
//! Nothing runs (no thread, no model) until the first frame is offered: off costs nothing.
//! Port of `src/engine/Segmenter.h/.cpp`.
//!
//! `blend_background` is told by its caller whether to use every core (`parallel`): the
//! capture engine passes `false` for pictures up to 1080p at up to 35 frames a second (all cores
//! cost 1.7x the CPU in all there) and `true` for bigger or faster ones. That rule is the
//! caller's (`CaptureEngine.cpp`, `ApplyBackground`), not this module's.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::JoinHandle;

use windows::core::{Interface, HSTRING};
use windows::Win32::System::WinRT::{RoInitialize, RoUninitialize, RO_INIT_MULTITHREADED};
use windows::AI::MachineLearning::{
    LearningModel, LearningModelBinding, LearningModelDevice, LearningModelDeviceKind,
    LearningModelSession, TensorFloat,
};

/// The model's input, square.
pub const K_SIZE: u32 = 192;
/// ~15 a second.
pub const K_INTERVAL_MS: f64 = 66.0;
/// ~30 a second in the high-quality mode (the model costs 1-2 ms on the GPU).
pub const K_INTERVAL_HQ_MS: f64 = 33.0;

/// What the worker thread and the capture thread share (the C++ members under `m_mutex`).
struct State {
    quit: bool,
    busy: bool,
    model: String,
    last_offer_ms: f64,
    job: Vec<f32>, // 1 x 3 x kSize x kSize, normalised RGB
    result: Vec<u8>,
    has_result: bool,
    reset: bool,
}

struct Shared {
    state: Mutex<State>,
    wake: Condvar,
    unsupported: AtomicBool,
    gave_up: AtomicBool,
    infer_ms: AtomicU64, // f64 bits
    /// High quality (ADR-008): the mask is the soft probability, smoothed by `background_hq`.
    quality: AtomicBool,
}

pub struct Segmenter {
    shared: Arc<Shared>,
    thread: Mutex<Option<JoinHandle<()>>>,
}

impl Default for Segmenter {
    fn default() -> Self {
        Self::new()
    }
}

impl Drop for Segmenter {
    fn drop(&mut self) {
        {
            let mut st = lock(&self.shared.state);
            st.quit = true;
        }
        self.shared.wake.notify_all();
        let handle = self
            .thread
            .get_mut()
            .map(|t| t.take())
            .unwrap_or_else(|e| e.into_inner().take());
        if let Some(h) = handle {
            let _ = h.join();
        }
    }
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

impl Segmenter {
    pub fn new() -> Segmenter {
        Segmenter {
            shared: Arc::new(Shared {
                state: Mutex::new(State {
                    quit: false,
                    busy: false,
                    model: String::new(),
                    last_offer_ms: -1e9,
                    job: Vec::new(),
                    result: Vec::new(),
                    has_result: false,
                    reset: false,
                }),
                wake: Condvar::new(),
                unsupported: AtomicBool::new(false),
                gave_up: AtomicBool::new(false),
                infer_ms: AtomicU64::new(0f64.to_bits()),
                quality: AtomicBool::new(false),
            }),
            thread: Mutex::new(None),
        }
    }

    /// High quality: the mask handed out is the soft person probability (0..255) smoothed over time
    /// by how sure each new look is, to be refined by `background_hq::blend_background_hq`; the
    /// model runs 30 times a second. Off: the classic firm mask, 15 times a second.
    pub fn set_quality(&self, on: bool) {
        self.shared.quality.store(on, Ordering::Relaxed);
    }

    pub fn set_model(&self, path: &str) {
        lock(&self.shared.state).model = path.to_owned();
    }

    /// Capture thread: the last job is done and `K_INTERVAL_MS` passed.
    pub fn due(&self, now_ms: f64) -> bool {
        if self.shared.unsupported.load(Ordering::Relaxed)
            || self.shared.gave_up.load(Ordering::Relaxed)
        {
            return false;
        }
        let st = lock(&self.shared.state);
        let interval = if self.shared.quality.load(Ordering::Relaxed) {
            K_INTERVAL_HQ_MS
        } else {
            K_INTERVAL_MS
        };
        !st.busy && now_ms - st.last_offer_ms >= interval
    }

    /// Starts over (another source: the old mask is someone else).
    pub fn reset(&self) {
        let mut st = lock(&self.shared.state);
        st.reset = true;
        st.has_result = false;
    }

    /// A `K_SIZE` x `K_SIZE` RGB sample of this NV12 picture (planes y / uv, `pitch` bytes a row).
    pub fn offer(&self, y: &[u8], uv: &[u8], pitch: usize, w: u32, h: u32, now_ms: f64) {
        if w < 2 || h < 2 {
            return;
        }
        // A picture that is not all there would be read out of bounds in the C++: not offered.
        if y.len() < (h as usize - 1) * pitch + w as usize
            || uv.len() < (h as usize / 2 - 1) * pitch + w as usize
        {
            return;
        }
        let job = sample_input(y, uv, pitch, w, h);
        {
            let mut st = lock(&self.shared.state);
            if st.busy {
                return;
            }
            st.job = job;
            st.busy = true;
            st.last_offer_ms = now_ms;
            let mut thread = lock(&self.thread);
            if thread.is_none() {
                let shared = Arc::clone(&self.shared);
                // The C++ std::thread throws if it cannot start; here the offer is simply lost
                // and the next due frame tries again.
                match std::thread::Builder::new()
                    .name("niwcam-segmenter".into())
                    .spawn(move || worker(&shared))
                {
                    Ok(h) => *thread = Some(h),
                    Err(_) => st.busy = false,
                }
            }
        }
        self.shared.wake.notify_one();
    }

    /// The newest mask (`K_SIZE` x `K_SIZE`, 0 background .. 255 person, smoothed over time),
    /// when it changed since the last call.
    pub fn take_mask(&self, mask: &mut Vec<u8>) -> bool {
        let mut st = lock(&self.shared.state);
        if !st.has_result {
            return false;
        }
        st.has_result = false;
        mask.clone_from(&st.result);
        true
    }

    /// 1 works or not tried yet; 0 Windows ML or the model is missing; -1 it failed and gave up.
    pub fn state(&self) -> i32 {
        if self.shared.unsupported.load(Ordering::Relaxed) {
            0
        } else if self.shared.gave_up.load(Ordering::Relaxed) {
            -1
        } else {
            1
        }
    }

    pub fn infer_ms(&self) -> f64 {
        f64::from_bits(self.shared.infer_ms.load(Ordering::Relaxed))
    }
}

/// The model's input: RGB, (value / 255 - 0.5) / 0.5, planar (NCHW). One sample per cell,
/// BT.601 limited range like the rest of the pipeline. (`Segmenter::Offer`'s first half; public
/// so the cross-check can compare the tensor itself.)
#[doc(hidden)]
pub fn sample_input(y: &[u8], uv: &[u8], pitch: usize, w: u32, h: u32) -> Vec<f32> {
    let k = K_SIZE as usize;
    let mut job = vec![0f32; 3 * k * k];
    let (r, rest) = job.split_at_mut(k * k);
    let (g, b) = rest.split_at_mut(k * k);
    for j in 0..K_SIZE {
        let sy = ((j * h + h / 2) / K_SIZE) as usize;
        let yrow = &y[sy * pitch..];
        let uvrow = &uv[(sy / 2) * pitch..];
        for i in 0..K_SIZE {
            let sx = ((i * w + w / 2) / K_SIZE) as usize;
            let yy: f32 = 1.164f32 * (yrow[sx] as i32 - 16) as f32;
            let u: f32 = uvrow[sx & !1] as f32 - 128.0;
            let v: f32 = uvrow[(sx & !1) + 1] as f32 - 128.0;
            let at = j as usize * k + i as usize;
            r[at] = (yy + 1.596f32 * v).clamp(0.0, 255.0) / 127.5f32 - 1.0;
            g[at] = (yy - 0.392f32 * u - 0.813f32 * v).clamp(0.0, 255.0) / 127.5f32 - 1.0;
            b[at] = (yy + 2.017f32 * u).clamp(0.0, 255.0) / 127.5f32 - 1.0;
        }
    }
    job
}

/// The Windows ML objects of the worker thread.
struct Model {
    session: LearningModelSession,
    input: HSTRING,
    output: HSTRING,
}

fn load_model(path: &str) -> windows::core::Result<Model> {
    let model = LearningModel::LoadFromFilePath(&HSTRING::from(path))?;
    let input = model.InputFeatures()?.GetAt(0)?.Name()?;
    let output = model.OutputFeatures()?.GetAt(0)?.Name()?;
    // The GPU when it can run the model, else the CPU (still a few ms at 192 x 192).
    let gpu = LearningModelDevice::Create(LearningModelDeviceKind::DirectXHighPerformance)
        .and_then(|d| LearningModelSession::CreateFromModelOnDevice(&model, &d));
    let session = match gpu {
        Ok(s) => s,
        Err(_) => {
            let cpu = LearningModelDevice::Create(LearningModelDeviceKind::Cpu)?;
            LearningModelSession::CreateFromModelOnDevice(&model, &cpu)?
        }
    };
    Ok(Model {
        session,
        input,
        output,
    })
}

/// One inference: the person probability over the picture, smoothed and sharpened into a mask.
fn infer(
    model: &Model,
    job: &[f32],
    reset: bool,
    quality: bool,
    smooth: &mut Vec<f32>,
    infer_ms: &AtomicU64,
) -> windows::core::Result<Vec<u8>> {
    let k = K_SIZE as usize;
    let t0 = std::time::Instant::now();
    let binding = LearningModelBinding::CreateFromSession(&model.session)?;
    let shape: windows_collections::IIterable<i64> =
        vec![1i64, 3, K_SIZE as i64, K_SIZE as i64].into();
    let tensor = TensorFloat::CreateFromArray(&shape, job)?;
    binding.Bind(&model.input, &tensor)?;
    let result = model.session.Evaluate(&binding, &HSTRING::new())?;
    let outputs = result.Outputs()?;
    let tensor: TensorFloat = outputs.Lookup(&model.output)?.cast()?;
    let view = tensor.GetAsVectorView()?;
    let mut out = vec![0f32; view.Size()? as usize];
    view.GetMany(0, &mut out)?;
    infer_ms.store(
        (t0.elapsed().as_secs_f64() * 1000.0).to_bits(),
        Ordering::Relaxed,
    );

    // Two channels (background, person): probabilities when they add up to 1, else scores
    // (logits).
    let n = k * k;
    let mut person = vec![0f32; n];
    if out.len() >= 2 * n {
        let c0 = &out[..n];
        let c1 = &out[n..];
        let probabilities = (c0[n / 2] + c1[n / 2] - 1.0).abs() < 0.02
            && c1[n / 2] >= 0.0
            && c1[n / 2] <= 1.0
            && (c0[0] + c1[0] - 1.0).abs() < 0.02;
        for i in 0..n {
            person[i] = if probabilities {
                c1[i]
            } else {
                1.0 / (1.0 + (c0[i] - c1[i]).exp())
            };
        }
    } else if out.len() >= n {
        for i in 0..n {
            person[i] = if out[i] >= 0.0 && out[i] <= 1.0 {
                out[i]
            } else {
                1.0 / (1.0 + (-out[i]).exp())
            };
        }
    } else {
        return Err(windows::Win32::Foundation::E_UNEXPECTED.into());
    }
    if quality {
        crate::background_hq::smooth_probability(smooth, &person, reset);
        return Ok(smooth
            .iter()
            .map(|&p| (p.clamp(0.0, 1.0) * 255.0 + 0.5) as u8)
            .collect());
    }
    Ok(classic_mask(&person, reset, smooth))
}

/// The classic post-processing of the person probability: half the new look and half the old over
/// time, then a firmer edge (0.3 and below is background, 0.7 and above the person).
#[doc(hidden)]
pub fn classic_mask(person: &[f32], reset: bool, smooth: &mut Vec<f32>) -> Vec<u8> {
    let n = person.len();
    // Over time: half the new look, half the old (no flicker at the edges).
    if reset || smooth.len() != n {
        smooth.clear();
        smooth.extend_from_slice(person);
    } else {
        for i in 0..n {
            smooth[i] = 0.5 * (smooth[i] + person[i]);
        }
    }
    // A firmer edge: 0.3 and below is background, 0.7 and above the person.
    let mut mask = vec![0u8; n];
    for i in 0..n {
        let t = ((smooth[i] - 0.3) / 0.4).clamp(0.0, 1.0);
        mask[i] = (255.0f32 * t * t * (3.0 - 2.0 * t)).round() as i64 as u8;
    }
    mask
}

fn worker(shared: &Shared) {
    let mut apartment = false;
    let mut model: Option<Model> = None;
    // SAFETY: plain apartment initialisation of this thread, undone at its end when it worked.
    let init = unsafe { RoInitialize(RO_INIT_MULTITHREADED) };
    let loaded = init.map(|()| {
        apartment = true;
        let path = lock(&shared.state).model.clone();
        load_model(&path)
    });
    match loaded {
        Ok(Ok(m)) => model = Some(m),
        // No Windows ML (Windows N), the model missing, or an operator it lacks.
        _ => shared.unsupported.store(true, Ordering::Relaxed),
    }

    let mut errors = 0;
    let mut job: Vec<f32> = Vec::new();
    let mut smooth: Vec<f32> = Vec::new();
    loop {
        let reset;
        {
            let mut st = lock(&shared.state);
            while !(st.quit || st.busy) {
                st = shared.wake.wait(st).unwrap_or_else(|e| e.into_inner());
            }
            if st.quit {
                break;
            }
            std::mem::swap(&mut job, &mut st.job);
            reset = st.reset;
            st.reset = false;
        }
        let mut ok = false;
        let mut mask: Vec<u8> = Vec::new();
        if !shared.unsupported.load(Ordering::Relaxed) && !shared.gave_up.load(Ordering::Relaxed) {
            let outcome = model.as_ref().map(|m| {
                std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    infer(
                        m,
                        &job,
                        reset,
                        shared.quality.load(Ordering::Relaxed),
                        &mut smooth,
                        &shared.infer_ms,
                    )
                }))
            });
            match outcome {
                Some(Ok(Ok(m))) => {
                    mask = m;
                    ok = true;
                    errors = 0;
                }
                _ => {
                    errors += 1;
                    if errors >= 10 {
                        shared.gave_up.store(true, Ordering::Relaxed);
                    }
                }
            }
        }
        let mut st = lock(&shared.state);
        st.busy = false;
        if ok {
            std::mem::swap(&mut st.result, &mut mask);
            st.has_result = true;
        }
    }
    drop(model);
    if apartment {
        // SAFETY: balances the successful RoInitialize above, on the same thread.
        unsafe { RoUninitialize() };
    }
}

// ---- blending ----------------------------------------------------------------------------

/// Box-averages an NV12 region into tw x th (both even), then softens it twice with a 3 x 3 box.
#[expect(clippy::too_many_arguments)]
fn shrink(
    y: &[u8],
    uv: &[u8],
    pitch: usize,
    w: u32,
    h: u32,
    tw: u32,
    th: u32,
    tiny: &mut Vec<u8>,
    tmp: &mut Vec<u8>,
) {
    tiny.resize(tw as usize * th as usize * 3 / 2, 0);
    let (ty, tuv) = tiny.split_at_mut(tw as usize * th as usize);
    for j in 0..th {
        for i in 0..tw {
            let x0 = i * w / tw;
            let x1 = (x0 + 1).max((i + 1) * w / tw);
            let y0 = j * h / th;
            let y1 = (y0 + 1).max((j + 1) * h / th);
            let (mut sum, mut count) = (0u32, 0u32);
            // every other row and column: plenty for a blur
            let mut yy = y0;
            while yy < y1 {
                let mut xx = x0;
                while xx < x1 {
                    sum += y[yy as usize * pitch + xx as usize] as u32;
                    count += 1;
                    xx += 2;
                }
                yy += 2;
            }
            ty[j as usize * tw as usize + i as usize] = (sum / count.max(1)) as u8;
        }
    }
    let (cw, ch, sw, sh) = (tw / 2, th / 2, w / 2, h / 2);
    for j in 0..ch {
        for i in 0..cw {
            let x0 = i * sw / cw;
            let x1 = (x0 + 1).max((i + 1) * sw / cw);
            let y0 = j * sh / ch;
            let y1 = (y0 + 1).max((j + 1) * sh / ch);
            let (mut su, mut sv, mut count) = (0u32, 0u32, 0u32);
            let mut yy = y0;
            while yy < y1 {
                let mut xx = x0;
                while xx < x1 {
                    su += uv[yy as usize * pitch + 2 * xx as usize] as u32;
                    sv += uv[yy as usize * pitch + 2 * xx as usize + 1] as u32;
                    count += 1;
                    xx += 2;
                }
                yy += 2;
            }
            let count = count.max(1);
            tuv[j as usize * tw as usize + 2 * i as usize] = (su / count) as u8;
            tuv[j as usize * tw as usize + 2 * i as usize + 1] = (sv / count) as u8;
        }
    }
    // Softer: a 3 x 3 box, twice, on each plane (chroma per component).
    let mut soften = |plane: &mut [u8], pw: u32, ph: u32, step: u32| {
        let len = pw as usize * step as usize * ph as usize;
        tmp.clear();
        tmp.extend_from_slice(&plane[..len]);
        for _pass in 0..2 {
            for j in 0..ph {
                for i in 0..pw {
                    for c in 0..step {
                        let (mut sum, mut count) = (0u32, 0u32);
                        for dj in -1i32..=1 {
                            for di in -1i32..=1 {
                                let jj = j as i32 + dj;
                                let ii = i as i32 + di;
                                if jj < 0 || ii < 0 || jj >= ph as i32 || ii >= pw as i32 {
                                    continue;
                                }
                                sum += tmp[(jj as usize * pw as usize + ii as usize)
                                    * step as usize
                                    + c as usize] as u32;
                                count += 1;
                            }
                        }
                        plane[(j as usize * pw as usize + i as usize) * step as usize
                            + c as usize] = (sum / count) as u8;
                    }
                }
            }
            tmp.clear();
            tmp.extend_from_slice(&plane[..len]);
        }
    };
    soften(ty, tw, th, 1);
    soften(tuv, cw, ch, 2);
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
    let i1 = (i0 + 1).min(inn - 1);
    Tap {
        i0,
        i1,
        w1: ((pos - i0 as f64) * 256.0) as u32,
    }
}

/// Scratch buffers of [`blend_background`], kept between frames.
#[derive(Default)]
pub struct BackgroundScratch {
    /// The box-averaged picture the blur is made from.
    pub tiny: Vec<u8>,
    pub tiny_tmp: Vec<u8>,
    /// The mask at half the picture's size.
    pub alpha: Vec<u8>,
    pub alpha_w: u32,
    pub alpha_h: u32,
}

/// What every row chunk reads.
struct Blend<'a> {
    w: u32,
    h: u32,
    aw: u32,
    ah: u32,
    th: u32,
    tw: u32,
    pitch: usize,
    alpha: &'a [u8],
    ty: &'a [u8],
    tuv: &'a [u8],
    xl: &'a [Tap],
    xc: &'a [Tap],
    image: Option<&'a [u8]>,
}

impl Blend<'_> {
    /// Rows in chunks: a luma row pair and its chroma row together. `y` / `uv` start at the
    /// chunk's first row.
    fn chunk(&self, chunk: u32, y: &mut [u8], uv: &mut [u8]) {
        let (w, h, aw, ah, th, tw) = (self.w, self.h, self.aw, self.ah, self.th, self.tw);
        let pitch = self.pitch;
        let cy0 = chunk * 32;
        let cy1 = ah.min(cy0 + 32);
        for cy in cy0..cy1 {
            let arow = &self.alpha[cy as usize * aw as usize..];
            for k in 0..2u32 {
                let yy = cy * 2 + k;
                let local = ((cy - cy0) * 2 + k) as usize;
                let row = &mut y[local * pitch..local * pitch + w as usize];
                if let Some(image) = self.image {
                    let bg = &image[yy as usize * w as usize..];
                    for x in 0..w as usize {
                        let a = arow[x / 2] as u32 + (arow[x / 2] as u32 >> 7); // 0..256
                        row[x] = ((row[x] as u32 * a + bg[x] as u32 * (256 - a)) >> 8) as u8;
                    }
                    continue;
                }
                let t = make_tap(yy, h, th);
                let r0 = &self.ty[t.i0 as usize * tw as usize..];
                let r1 = &self.ty[t.i1 as usize * tw as usize..];
                for x in 0..w as usize {
                    let a = arow[x / 2] as u32 + (arow[x / 2] as u32 >> 7);
                    if a == 256 {
                        continue; // the person: untouched
                    }
                    let xt = &self.xl[x];
                    let top = r0[xt.i0 as usize] as u32 * (256 - xt.w1)
                        + r0[xt.i1 as usize] as u32 * xt.w1;
                    let bottom = r1[xt.i0 as usize] as u32 * (256 - xt.w1)
                        + r1[xt.i1 as usize] as u32 * xt.w1;
                    let bg = (top * (256 - t.w1) + bottom * t.w1 + (1u32 << 15)) >> 16;
                    row[x] = ((row[x] as u32 * a + bg * (256 - a)) >> 8) as u8;
                }
            }
            let local = (cy - cy0) as usize;
            let crow = &mut uv[local * pitch..local * pitch + aw as usize * 2];
            if let Some(image) = self.image {
                let bg = &image[w as usize * h as usize + cy as usize * w as usize..];
                for x in 0..aw as usize {
                    let a = arow[x] as u32 + (arow[x] as u32 >> 7);
                    crow[2 * x] =
                        ((crow[2 * x] as u32 * a + bg[2 * x] as u32 * (256 - a)) >> 8) as u8;
                    crow[2 * x + 1] = ((crow[2 * x + 1] as u32 * a
                        + bg[2 * x + 1] as u32 * (256 - a))
                        >> 8) as u8;
                }
                continue;
            }
            let t = make_tap(cy, ah, th / 2);
            let r0 = &self.tuv[t.i0 as usize * tw as usize..];
            let r1 = &self.tuv[t.i1 as usize * tw as usize..];
            for x in 0..aw as usize {
                let a = arow[x] as u32 + (arow[x] as u32 >> 7);
                if a == 256 {
                    continue;
                }
                let xt = &self.xc[x];
                for c in 0..2usize {
                    let top = r0[2 * xt.i0 as usize + c] as u32 * (256 - xt.w1)
                        + r0[2 * xt.i1 as usize + c] as u32 * xt.w1;
                    let bottom = r1[2 * xt.i0 as usize + c] as u32 * (256 - xt.w1)
                        + r1[2 * xt.i1 as usize + c] as u32 * xt.w1;
                    let bg = (top * (256 - t.w1) + bottom * t.w1 + (1u32 << 15)) >> 16;
                    crow[2 * x + c] = ((crow[2 * x + c] as u32 * a + bg * (256 - a)) >> 8) as u8;
                }
            }
        }
    }
}

/// Blends the background of an NV12 picture region (planes `y` / `uv`, `pitch`) with a blurred
/// copy of itself (`strength` 0..100) or with `image` (NV12 of the same w x h), using `mask`
/// ([`K_SIZE`] square, 255 = person). Rows run in parallel when `parallel` (a big picture, or
/// many frames a second): spreading a 1080p frame over every core took half again as much CPU
/// in all.
///
/// `y` must hold `h` rows and `uv` `h / 2` rows of `pitch` bytes (the last row may stop after
/// `w` bytes).
#[expect(clippy::too_many_arguments)]
pub fn blend_background(
    y: &mut [u8],
    uv: &mut [u8],
    pitch: usize,
    w: u32,
    h: u32,
    mask: &[u8],
    mask_changed: bool,
    strength: i32,
    image: Option<&[u8]>,
    s: &mut BackgroundScratch,
    parallel: bool,
) {
    const K: u32 = K_SIZE;
    if w < 4 || h < 4 || mask.len() != K as usize * K as usize {
        return;
    }
    // A picture that is not all there would be read out of bounds in the C++.
    let need_y = (h as usize - 1) * pitch + w as usize;
    let need_uv = (h as usize / 2 - 1) * pitch + (w as usize / 2) * 2;
    if pitch < w as usize
        || y.len() < need_y
        || uv.len() < need_uv
        || image.is_some_and(|i| i.len() < w as usize * h as usize * 3 / 2)
    {
        return;
    }

    // The mask at half the picture's size: one value per chroma sample, per 2 x 2 luma.
    let (aw, ah) = (w / 2, h / 2);
    if mask_changed || s.alpha_w != aw || s.alpha_h != ah {
        s.alpha_w = aw;
        s.alpha_h = ah;
        s.alpha.resize(aw as usize * ah as usize, 0);
        let xt: Vec<Tap> = (0..aw).map(|i| make_tap(i, aw, K)).collect();
        for j in 0..ah {
            let t = make_tap(j, ah, K);
            let r0 = &mask[t.i0 as usize * K as usize..];
            let r1 = &mask[t.i1 as usize * K as usize..];
            let dst = &mut s.alpha[j as usize * aw as usize..(j as usize + 1) * aw as usize];
            for (i, d) in dst.iter_mut().enumerate() {
                let x = &xt[i];
                let top = r0[x.i0 as usize] as u32 * (256 - x.w1) + r0[x.i1 as usize] as u32 * x.w1;
                let bottom =
                    r1[x.i0 as usize] as u32 * (256 - x.w1) + r1[x.i1 as usize] as u32 * x.w1;
                *d = ((top * (256 - t.w1) + bottom * t.w1 + (1u32 << 15)) >> 16) as u8;
            }
        }
    }

    // The blurred picture, small: stronger means bigger blocks (8..40 px at 1080 lines).
    let (mut tw, mut th) = (0u32, 0u32);
    if image.is_none() {
        let block = (8.0 + strength.clamp(0, 100) as f64 * 0.32) * h as f64 / 1080.0;
        tw = 4.max((w as f64 / block.max(2.0)) as u32) & !1;
        th = 4.max((h as f64 / block.max(2.0)) as u32) & !1;
        shrink(y, uv, pitch, w, h, tw, th, &mut s.tiny, &mut s.tiny_tmp);
    }
    let (ty, tuv): (&[u8], &[u8]) = if image.is_none() {
        s.tiny.split_at(tw as usize * th as usize)
    } else {
        (&[], &[])
    };
    let (mut xl, mut xc) = (Vec::new(), Vec::new());
    if image.is_none() {
        xl = (0..w).map(|i| make_tap(i, w, tw)).collect::<Vec<Tap>>();
        xc = (0..aw)
            .map(|i| make_tap(i, aw, tw / 2))
            .collect::<Vec<Tap>>();
    }

    let blend = Blend {
        w,
        h,
        aw,
        ah,
        th,
        tw,
        pitch,
        alpha: &s.alpha,
        ty,
        tuv,
        xl: &xl,
        xc: &xc,
        image,
    };
    let chunks = ah.div_ceil(32) as usize;
    let y_chunks = y[..need_y].chunks_mut(64 * pitch);
    let uv_chunks = uv[..need_uv].chunks_mut(32 * pitch);
    let mut work: Vec<(u32, &mut [u8], &mut [u8])> = y_chunks
        .zip(uv_chunks)
        .take(chunks)
        .enumerate()
        .map(|(i, (yc, uc))| (i as u32, yc, uc))
        .collect();
    if parallel && work.len() > 1 {
        let threads = std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(1)
            .min(work.len());
        let queue = Mutex::new(work.into_iter());
        std::thread::scope(|scope| {
            for _ in 0..threads {
                scope.spawn(|| loop {
                    let next = lock(&queue).next();
                    let Some((i, yc, uc)) = next else { break };
                    blend.chunk(i, yc, uc);
                });
            }
        });
    } else {
        for (i, yc, uc) in work.iter_mut() {
            blend.chunk(*i, yc, uc);
        }
    }
}
