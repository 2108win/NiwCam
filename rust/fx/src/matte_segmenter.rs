//! "Nền" in high quality, the matting model (ADR-008): MODNet (Apache-2.0, trimap-free portrait
//! matting, `modnet_webcam.onnx`) through Windows ML on its own thread. Unlike the 2-class
//! PP-HumanSeg of [`crate::segmenter::Segmenter`] it gives a soft alpha (hair, a hand in motion
//! blur), and it takes any size whose sides are multiples of 32, so the picture is looked at in
//! its own shape (384 x 224 for 16:9, 224 x 384 for a phone held upright) instead of a squashed
//! square. About 7 ms per run on the GPU; run 20 times a second.
//!
//! Same life cycle as `Segmenter`: no thread and no model until the first frame is offered. The mask
//! it hands out is the soft alpha (0..255) smoothed over time by `background_hq::smooth_probability`,
//! at the size [`take_mask`](MatteSegmenter::take_mask) reports, for `blend_background_hq`.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::JoinHandle;

use windows::core::{Interface, HSTRING};
use windows::Win32::System::WinRT::{RoInitialize, RoUninitialize, RO_INIT_MULTITHREADED};
use windows::AI::MachineLearning::{
    LearningModel, LearningModelBinding, LearningModelDevice, LearningModelDeviceKind,
    LearningModelSession, TensorFloat,
};

/// Pixels the model looks at (384 x 224).
const TARGET_PIXELS: f64 = 86016.0;
/// 20 a second.
pub const INTERVAL_MS: f64 = 50.0;

/// The model's input size for a w x h picture: sides multiples of 32, about [`TARGET_PIXELS`]
/// pixels, the picture's shape.
pub fn input_size(w: u32, h: u32) -> (u32, u32) {
    let aspect = w.max(1) as f64 / h.max(1) as f64;
    let side = |v: f64| ((v / 32.0).round() as u32).max(2) * 32;
    let th = side((TARGET_PIXELS / aspect).sqrt());
    let tw = side(th as f64 * aspect);
    (tw, th)
}

struct State {
    quit: bool,
    busy: bool,
    model: String,
    last_offer_ms: f64,
    job: Vec<f32>, // 1 x 3 x job_h x job_w, RGB scaled to -1..1
    job_w: u32,
    job_h: u32,
    result: Vec<u8>,
    result_w: u32,
    result_h: u32,
    has_result: bool,
    reset: bool,
}

struct Shared {
    state: Mutex<State>,
    wake: Condvar,
    unsupported: AtomicBool,
    gave_up: AtomicBool,
    infer_ms: AtomicU64, // f64 bits
}

pub struct MatteSegmenter {
    shared: Arc<Shared>,
    thread: Mutex<Option<JoinHandle<()>>>,
}

impl Default for MatteSegmenter {
    fn default() -> Self {
        Self::new()
    }
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

impl Drop for MatteSegmenter {
    fn drop(&mut self) {
        lock(&self.shared.state).quit = true;
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

impl MatteSegmenter {
    pub fn new() -> MatteSegmenter {
        MatteSegmenter {
            shared: Arc::new(Shared {
                state: Mutex::new(State {
                    quit: false,
                    busy: false,
                    model: String::new(),
                    last_offer_ms: -1e9,
                    job: Vec::new(),
                    job_w: 0,
                    job_h: 0,
                    result: Vec::new(),
                    result_w: 0,
                    result_h: 0,
                    has_result: false,
                    reset: false,
                }),
                wake: Condvar::new(),
                unsupported: AtomicBool::new(false),
                gave_up: AtomicBool::new(false),
                infer_ms: AtomicU64::new(0f64.to_bits()),
            }),
            thread: Mutex::new(None),
        }
    }

    pub fn set_model(&self, path: &str) {
        lock(&self.shared.state).model = path.to_owned();
    }

    /// Capture thread: the last job is done and [`INTERVAL_MS`] passed.
    pub fn due(&self, now_ms: f64) -> bool {
        if self.shared.unsupported.load(Ordering::Relaxed)
            || self.shared.gave_up.load(Ordering::Relaxed)
        {
            return false;
        }
        let st = lock(&self.shared.state);
        !st.busy && now_ms - st.last_offer_ms >= INTERVAL_MS
    }

    /// Starts over (another source: the old mask is someone else).
    pub fn reset(&self) {
        let mut st = lock(&self.shared.state);
        st.reset = true;
        st.has_result = false;
    }

    /// Hands the worker an RGB sample of this NV12 picture (planes y / uv, `pitch` bytes a row).
    pub fn offer(&self, y: &[u8], uv: &[u8], pitch: usize, w: u32, h: u32, now_ms: f64) {
        if w < 32 || h < 32 {
            return;
        }
        if y.len() < (h as usize - 1) * pitch + w as usize
            || uv.len() < (h as usize / 2 - 1) * pitch + w as usize
        {
            return;
        }
        let (tw, th) = input_size(w, h);
        let job = sample_rgb(y, uv, pitch, w, h, tw, th);
        {
            let mut st = lock(&self.shared.state);
            if st.busy {
                return;
            }
            st.job = job;
            st.job_w = tw;
            st.job_h = th;
            st.busy = true;
            st.last_offer_ms = now_ms;
            let mut thread = lock(&self.thread);
            if thread.is_none() {
                let shared = Arc::clone(&self.shared);
                match std::thread::Builder::new()
                    .name("niwcam-matte".into())
                    .spawn(move || worker(&shared))
                {
                    Ok(h) => *thread = Some(h),
                    Err(_) => st.busy = false,
                }
            }
        }
        self.shared.wake.notify_one();
    }

    /// The newest soft alpha (0..255, smoothed over time) when it changed since the last call, with
    /// its size.
    pub fn take_mask(&self, mask: &mut Vec<u8>) -> Option<(u32, u32)> {
        let mut st = lock(&self.shared.state);
        if !st.has_result {
            return None;
        }
        st.has_result = false;
        mask.clone_from(&st.result);
        Some((st.result_w, st.result_h))
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

/// The model's input: RGB, (value - 127.5) / 127.5, planar (NCHW), each cell the mean of 2 x 2
/// samples of the picture (BT.601 limited range like the rest of the pipeline).
#[doc(hidden)]
pub fn sample_rgb(y: &[u8], uv: &[u8], pitch: usize, w: u32, h: u32, tw: u32, th: u32) -> Vec<f32> {
    let (twu, thu) = (tw as usize, th as usize);
    let mut job = vec![0f32; 3 * twu * thu];
    let (r, rest) = job.split_at_mut(twu * thu);
    let (g, b) = rest.split_at_mut(twu * thu);
    let rgb = |sx: usize, sy: usize| {
        let yy = 1.164f32 * (y[sy * pitch + sx] as i32 - 16) as f32;
        let u = uv[(sy / 2) * pitch + (sx & !1)] as f32 - 128.0;
        let v = uv[(sy / 2) * pitch + (sx & !1) + 1] as f32 - 128.0;
        (
            (yy + 1.596 * v).clamp(0.0, 255.0),
            (yy - 0.392 * u - 0.813 * v).clamp(0.0, 255.0),
            (yy + 2.017 * u).clamp(0.0, 255.0),
        )
    };
    let (wu, hu) = (w as usize, h as usize);
    for j in 0..thu {
        let y0 = j * hu / thu;
        let y1 = (((j + 1) * hu / thu).max(y0 + 1) - 1).min(hu - 1);
        for i in 0..twu {
            let x0 = i * wu / twu;
            let x1 = (((i + 1) * wu / twu).max(x0 + 1) - 1).min(wu - 1);
            let (mut sr, mut sg, mut sb) = (0f32, 0f32, 0f32);
            for (sx, sy) in [(x0, y0), (x1, y0), (x0, y1), (x1, y1)] {
                let (pr, pg, pb) = rgb(sx, sy);
                sr += pr;
                sg += pg;
                sb += pb;
            }
            let at = j * twu + i;
            r[at] = (sr * 0.25 - 127.5) / 127.5;
            g[at] = (sg * 0.25 - 127.5) / 127.5;
            b[at] = (sb * 0.25 - 127.5) / 127.5;
        }
    }
    job
}

struct Model {
    session: LearningModelSession,
    input: HSTRING,
    output: HSTRING,
}

fn load_model(path: &str) -> windows::core::Result<Model> {
    let model = LearningModel::LoadFromFilePath(&HSTRING::from(path))?;
    let input = model.InputFeatures()?.GetAt(0)?.Name()?;
    let output = model.OutputFeatures()?.GetAt(0)?.Name()?;
    // The GPU when it can run the model, else the CPU (about ten times slower).
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

/// One run: the alpha over the picture, smoothed over time into a mask.
fn infer(
    model: &Model,
    job: &[f32],
    (tw, th): (u32, u32),
    reset: bool,
    smooth: &mut Vec<f32>,
    infer_ms: &AtomicU64,
) -> windows::core::Result<Vec<u8>> {
    let t0 = std::time::Instant::now();
    let binding = LearningModelBinding::CreateFromSession(&model.session)?;
    let shape: windows_collections::IIterable<i64> = vec![1i64, 3, th as i64, tw as i64].into();
    binding.Bind(&model.input, &TensorFloat::CreateFromArray(&shape, job)?)?;
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

    let n = tw as usize * th as usize;
    if out.len() < n {
        return Err(windows::Win32::Foundation::E_UNEXPECTED.into());
    }
    out.truncate(n);
    for v in &mut out {
        *v = v.clamp(0.0, 1.0);
    }
    crate::background_hq::smooth_probability(smooth, &out, reset);
    Ok(smooth
        .iter()
        .map(|&p| (p.clamp(0.0, 1.0) * 255.0 + 0.5) as u8)
        .collect())
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
        // No Windows ML, the model missing, or an operator it lacks.
        _ => shared.unsupported.store(true, Ordering::Relaxed),
    }

    let mut errors = 0;
    let mut job: Vec<f32> = Vec::new();
    let mut smooth: Vec<f32> = Vec::new();
    loop {
        let (reset, size);
        {
            let mut st = lock(&shared.state);
            while !(st.quit || st.busy) {
                st = shared.wake.wait(st).unwrap_or_else(|e| e.into_inner());
            }
            if st.quit {
                break;
            }
            std::mem::swap(&mut job, &mut st.job);
            size = (st.job_w, st.job_h);
            reset = st.reset;
            st.reset = false;
        }
        let mut mask: Option<Vec<u8>> = None;
        if !shared.unsupported.load(Ordering::Relaxed) && !shared.gave_up.load(Ordering::Relaxed) {
            let outcome = model.as_ref().map(|m| {
                std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    infer(m, &job, size, reset, &mut smooth, &shared.infer_ms)
                }))
            });
            match outcome {
                Some(Ok(Ok(m))) => {
                    mask = Some(m);
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
        if let Some(m) = mask {
            st.result = m;
            st.result_w = size.0;
            st.result_h = size.1;
            st.has_result = true;
        }
    }
    drop(model);
    if apartment {
        // SAFETY: balances the successful RoInitialize above, on the same thread.
        unsafe { RoUninitialize() };
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn input_sizes_follow_the_picture() {
        assert_eq!(input_size(1920, 1080), (384, 224));
        assert_eq!(input_size(1080, 1920), (224, 384));
        let (w, h) = input_size(1440, 1080); // 4:3
        assert!(
            w % 32 == 0 && h % 32 == 0 && (w as f64 / h as f64 - 4.0 / 3.0).abs() < 0.2,
            "{w}x{h}"
        );
        let (w, h) = input_size(64, 64);
        assert!(w >= 64 && h >= 64);
    }

    #[test]
    fn sampling_is_normalised_and_planar() {
        let (w, h) = (64usize, 64usize);
        let y = vec![235u8; w * h]; // white
        let uv = vec![128u8; w * h / 2];
        let job = sample_rgb(&y, &uv, w, w as u32, h as u32, 32, 32);
        assert_eq!(job.len(), 3 * 32 * 32);
        assert!(
            job.iter().all(|&v| (v - 1.0).abs() < 0.02),
            "white is +1: {}",
            job[0]
        );
        let black = sample_rgb(&vec![16u8; w * h], &uv, w, w as u32, h as u32, 32, 32);
        assert!(black.iter().all(|&v| (v + 1.0).abs() < 0.02));
    }
}
