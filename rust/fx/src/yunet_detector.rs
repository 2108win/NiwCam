//! Face detection with YuNet (OpenCV Zoo `face_detection_yunet_2026may.onnx`, MIT) through Windows
//! ML, in place of Windows' FaceTracker (ADR-008): a better detector (WIDER Face val easy 0.884,
//! medium 0.866, hard 0.750 in the zoo's own evaluation), 3.8 ms on the GPU at 640 x 352, and no
//! dependence on the Media Feature Pack (N editions have no FaceTracker).
//!
//! Same life cycle and API as [`crate::face_detect::FaceDetector`] (a thread of its own that starts
//! with the first offered picture; one picture at a time; results tagged with the generation they
//! were found for), so the engine can use either. The picture is looked at in grey, copied to the
//! three channels: the engine hands the detectors the luma plane only.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::JoinHandle;

use niwcam_core::auto_frame::{downscale_luma, FaceRect};
use windows::core::{Interface, HSTRING};
use windows::Win32::System::WinRT::{RoInitialize, RoUninitialize, RO_INIT_MULTITHREADED};
use windows::AI::MachineLearning::{
    LearningModel, LearningModelBinding, LearningModelDevice, LearningModelDeviceKind,
    LearningModelSession, TensorFloat,
};

/// A detection every 100 ms (10 a second): framing reacts a little faster than with the 200 ms of
/// the Windows tracker, and one run is 4 ms.
pub const INTERVAL_MS: f64 = 100.0;
/// The long side of the picture the model looks at.
pub const MAX_SIDE: u32 = 640;
pub const MAX_ERRORS: i32 = 10;
const SCORE_THRESHOLD: f32 = 0.6;
const NMS_IOU: f32 = 0.3;
const MAX_FACES: usize = 8;

struct State {
    quit: bool,
    busy: bool,
    model: String,
    last_offer_ms: f64,
    job: Vec<u8>,
    job_width: u32,
    job_height: u32,
    job_generation: u32,
    result: Vec<FaceRect>,
    result_generation: u32,
    has_result: bool,
}

struct Shared {
    state: Mutex<State>,
    wake: Condvar,
    unsupported: AtomicBool,
    gave_up: AtomicBool,
}

pub struct YuNetDetector {
    shared: Arc<Shared>,
    thread: Mutex<Option<JoinHandle<()>>>,
    scratch: Mutex<Vec<u8>>,
}

impl Default for YuNetDetector {
    fn default() -> Self {
        Self::new()
    }
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

impl Drop for YuNetDetector {
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

impl YuNetDetector {
    pub fn new() -> YuNetDetector {
        YuNetDetector {
            shared: Arc::new(Shared {
                state: Mutex::new(State {
                    quit: false,
                    busy: false,
                    model: String::new(),
                    last_offer_ms: -1e9,
                    job: Vec::new(),
                    job_width: 0,
                    job_height: 0,
                    job_generation: 0,
                    result: Vec::new(),
                    result_generation: 0,
                    has_result: false,
                }),
                wake: Condvar::new(),
                unsupported: AtomicBool::new(false),
                gave_up: AtomicBool::new(false),
            }),
            thread: Mutex::new(None),
            scratch: Mutex::new(Vec::new()),
        }
    }

    pub fn set_model(&self, path: &str) {
        lock(&self.shared.state).model = path.to_owned();
    }

    /// Capture thread: the last picture is done and [`INTERVAL_MS`] passed.
    pub fn due(&self, now_ms: f64) -> bool {
        let st = lock(&self.shared.state);
        if self.shared.unsupported.load(Ordering::Relaxed)
            || self.shared.gave_up.load(Ordering::Relaxed)
            || now_ms - st.last_offer_ms < INTERVAL_MS
        {
            return false;
        }
        !st.busy
    }

    /// Hands the worker a grey copy of this luma plane (`pitch` bytes a row), tagged `generation`.
    pub fn offer(
        &self,
        luma: &[u8],
        pitch: usize,
        width: u32,
        height: u32,
        generation: u32,
        now_ms: f64,
    ) {
        lock(&self.shared.state).last_offer_ms = now_ms;
        if width < 32 || height < 32 || luma.len() < (height as usize - 1) * pitch + width as usize
        {
            return;
        }
        let (w, h) = model_size(width, height);
        let mut scratch = lock(&self.scratch);
        scratch.resize(w as usize * h as usize, 0);
        downscale_luma(luma, pitch, width, height, &mut scratch, w, h);
        {
            let mut st = lock(&self.shared.state);
            if st.busy {
                return;
            }
            std::mem::swap(&mut st.job, &mut scratch);
            st.job_width = w;
            st.job_height = h;
            st.job_generation = generation;
            st.busy = true;
            let mut thread = lock(&self.thread);
            if thread.is_none() {
                let shared = Arc::clone(&self.shared);
                match std::thread::Builder::new()
                    .name("niwcam-yunet".into())
                    .spawn(move || worker(&shared))
                {
                    Ok(h) => *thread = Some(h),
                    Err(_) => st.busy = false,
                }
            }
        }
        self.shared.wake.notify_one();
    }

    /// The faces of the last detection, when it is new and belongs to `generation`.
    pub fn take_result(&self, generation: u32, faces: &mut Vec<FaceRect>) -> bool {
        let mut st = lock(&self.shared.state);
        if !st.has_result {
            return false;
        }
        st.has_result = false;
        if st.result_generation != generation {
            return false;
        }
        std::mem::swap(faces, &mut st.result);
        true
    }

    /// 1 works or not tried yet; 0 the model or Windows ML is missing; -1 it kept failing.
    pub fn state(&self) -> i32 {
        if self.shared.unsupported.load(Ordering::Relaxed) {
            0
        } else if self.shared.gave_up.load(Ordering::Relaxed) {
            -1
        } else {
            1
        }
    }
}

/// The size the model looks at: the long side at most [`MAX_SIDE`], both sides multiples of 32
/// (the model's strides are 8, 16 and 32).
pub fn model_size(width: u32, height: u32) -> (u32, u32) {
    let scale = (MAX_SIDE as f64 / width.max(height).max(1) as f64).min(1.0);
    let side = |v: u32| (((v as f64 * scale) / 32.0).round() as u32).max(1) * 32;
    (side(width), side(height))
}

/// One candidate box in pixels of the model's picture, with its score.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Candidate {
    pub score: f32,
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
}

/// OpenCV's `FaceDetectorYN` decoding of one stride: per cell the score is sqrt(cls * obj) and the
/// box is centred at (col + dx, row + dy) * stride with the size exp(dw), exp(dh) * stride.
pub fn decode(
    cls: &[f32],
    obj: &[f32],
    bbox: &[f32],
    stride: usize,
    w: usize,
    h: usize,
    out: &mut Vec<Candidate>,
) {
    let (cols, rows) = (w / stride, h / stride);
    for r in 0..rows {
        for c in 0..cols {
            let idx = r * cols + c;
            if idx >= cls.len() || idx >= obj.len() || idx * 4 + 3 >= bbox.len() {
                return;
            }
            let score = (cls[idx].clamp(0.0, 1.0) * obj[idx].clamp(0.0, 1.0)).sqrt();
            if score < SCORE_THRESHOLD {
                continue;
            }
            let b = &bbox[idx * 4..idx * 4 + 4];
            let s = stride as f32;
            let (cx, cy) = ((c as f32 + b[0]) * s, (r as f32 + b[1]) * s);
            let (bw, bh) = (b[2].exp() * s, b[3].exp() * s);
            out.push(Candidate {
                score,
                x: cx - bw / 2.0,
                y: cy - bh / 2.0,
                w: bw,
                h: bh,
            });
        }
    }
}

fn iou(a: &Candidate, b: &Candidate) -> f32 {
    let (x0, y0) = (a.x.max(b.x), a.y.max(b.y));
    let (x1, y1) = ((a.x + a.w).min(b.x + b.w), (a.y + a.h).min(b.y + b.h));
    let inter = (x1 - x0).max(0.0) * (y1 - y0).max(0.0);
    let union = a.w * a.h + b.w * b.h - inter;
    if union <= 0.0 {
        0.0
    } else {
        inter / union
    }
}

/// Greedy non-maximum suppression, best score first, at most [`MAX_FACES`] kept.
pub fn nms(mut c: Vec<Candidate>) -> Vec<Candidate> {
    c.sort_by(|a, b| b.score.total_cmp(&a.score));
    let mut kept: Vec<Candidate> = Vec::new();
    for cand in c {
        if kept.len() >= MAX_FACES {
            break;
        }
        if kept.iter().all(|k| iou(k, &cand) <= NMS_IOU) {
            kept.push(cand);
        }
    }
    kept
}

/// Boxes as fractions of the picture, clamped into it.
pub fn to_fractions(c: &[Candidate], w: u32, h: u32) -> Vec<FaceRect> {
    c.iter()
        .filter_map(|f| {
            let (x0, y0) = (
                (f.x / w as f32).clamp(0.0, 1.0),
                (f.y / h as f32).clamp(0.0, 1.0),
            );
            let (x1, y1) = (
                ((f.x + f.w) / w as f32).clamp(0.0, 1.0),
                ((f.y + f.h) / h as f32).clamp(0.0, 1.0),
            );
            (x1 > x0 && y1 > y0).then_some(FaceRect {
                x: x0 as f64,
                y: y0 as f64,
                w: (x1 - x0) as f64,
                h: (y1 - y0) as f64,
            })
        })
        .collect()
}

struct Model {
    session: LearningModelSession,
    input: HSTRING,
    outputs: Vec<HSTRING>,
}

fn load_model(path: &str) -> windows::core::Result<Model> {
    let model = LearningModel::LoadFromFilePath(&HSTRING::from(path))?;
    let input = model.InputFeatures()?.GetAt(0)?.Name()?;
    let mut outputs = Vec::new();
    for f in model.OutputFeatures()? {
        outputs.push(f.Name()?);
    }
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
        outputs,
    })
}

fn detect(model: &Model, grey: &[u8], w: u32, h: u32) -> windows::core::Result<Vec<FaceRect>> {
    let n = w as usize * h as usize;
    // BGR, 0..255, planar: the grey picture in all three channels.
    let mut input = vec![0f32; 3 * n];
    for (i, &v) in grey.iter().enumerate().take(n) {
        let v = v as f32;
        input[i] = v;
        input[n + i] = v;
        input[2 * n + i] = v;
    }
    let binding = LearningModelBinding::CreateFromSession(&model.session)?;
    let shape: windows_collections::IIterable<i64> = vec![1i64, 3, h as i64, w as i64].into();
    binding.Bind(&model.input, &TensorFloat::CreateFromArray(&shape, &input)?)?;
    let result = model.session.Evaluate(&binding, &HSTRING::new())?;
    let outputs = result.Outputs()?;
    let read = |name: &str| -> windows::core::Result<Vec<f32>> {
        let key = model
            .outputs
            .iter()
            .find(|o| *o == name)
            .ok_or(windows::core::Error::from(
                windows::Win32::Foundation::E_UNEXPECTED,
            ))?;
        let t: TensorFloat = outputs.Lookup(key)?.cast()?;
        let view = t.GetAsVectorView()?;
        let mut v = vec![0f32; view.Size()? as usize];
        view.GetMany(0, &mut v)?;
        Ok(v)
    };
    let mut candidates = Vec::new();
    for stride in [8usize, 16, 32] {
        let cls = read(&format!("cls_{stride}"))?;
        let obj = read(&format!("obj_{stride}"))?;
        let bbox = read(&format!("bbox_{stride}"))?;
        decode(
            &cls,
            &obj,
            &bbox,
            stride,
            w as usize,
            h as usize,
            &mut candidates,
        );
    }
    Ok(to_fractions(&nms(candidates), w, h))
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
        _ => shared.unsupported.store(true, Ordering::Relaxed),
    }
    let mut errors = 0;
    let mut job: Vec<u8> = Vec::new();
    loop {
        let (w, h, generation);
        {
            let mut st = lock(&shared.state);
            while !(st.quit || st.busy) {
                st = shared.wake.wait(st).unwrap_or_else(|e| e.into_inner());
            }
            if st.quit {
                break;
            }
            std::mem::swap(&mut job, &mut st.job);
            w = st.job_width;
            h = st.job_height;
            generation = st.job_generation;
        }
        let mut found: Option<Vec<FaceRect>> = None;
        if !shared.unsupported.load(Ordering::Relaxed) && !shared.gave_up.load(Ordering::Relaxed) {
            let outcome = model.as_ref().map(|m| {
                std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| detect(m, &job, w, h)))
            });
            match outcome {
                Some(Ok(Ok(faces))) => {
                    found = Some(faces);
                    errors = 0;
                }
                _ => {
                    errors += 1;
                    if errors >= MAX_ERRORS {
                        shared.gave_up.store(true, Ordering::Relaxed);
                    }
                }
            }
        }
        let mut st = lock(&shared.state);
        st.busy = false;
        if let Some(faces) = found {
            st.result = faces;
            st.result_generation = generation;
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
    fn model_sizes_are_multiples_of_32() {
        assert_eq!(model_size(1920, 1080), (640, 352));
        assert_eq!(model_size(1080, 1920), (352, 640));
        assert_eq!(model_size(320, 240), (320, 256));
        let (w, h) = model_size(64, 64);
        assert!(w % 32 == 0 && h % 32 == 0 && w >= 32);
    }

    #[test]
    fn decode_places_a_box_like_opencv() {
        // One 8-stride cell at (col 3, row 2): centre (3.5, 2.5) * 8, size e^0 * 8 = 8.
        let (w, h, stride) = (64usize, 32usize, 8usize);
        let cells = (w / stride) * (h / stride);
        let mut cls = vec![0f32; cells];
        let mut obj = vec![0f32; cells];
        let mut bbox = vec![0f32; cells * 4];
        let idx = 2 * (w / stride) + 3;
        cls[idx] = 0.9;
        obj[idx] = 0.9;
        bbox[idx * 4] = 0.5;
        bbox[idx * 4 + 1] = 0.5;
        let mut out = Vec::new();
        decode(&cls, &obj, &bbox, stride, w, h, &mut out);
        assert_eq!(out.len(), 1);
        let c = out[0];
        assert!((c.score - 0.9).abs() < 1e-6);
        assert!(
            (c.x - 24.0).abs() < 1e-4 && (c.y - 16.0).abs() < 1e-4 && (c.w - 8.0).abs() < 1e-4,
            "{c:?}"
        );
    }

    #[test]
    fn nms_keeps_the_best_of_overlapping_boxes() {
        let a = Candidate {
            score: 0.9,
            x: 10.0,
            y: 10.0,
            w: 40.0,
            h: 40.0,
        };
        let b = Candidate {
            score: 0.8,
            x: 12.0,
            y: 12.0,
            w: 40.0,
            h: 40.0,
        };
        let far = Candidate {
            score: 0.7,
            x: 200.0,
            y: 10.0,
            w: 40.0,
            h: 40.0,
        };
        let kept = nms(vec![b, far, a]);
        assert_eq!(kept, vec![a, far]);
    }

    #[test]
    fn fractions_are_clamped_into_the_picture() {
        let c = Candidate {
            score: 0.9,
            x: -10.0,
            y: -4.0,
            w: 50.0,
            h: 20.0,
        };
        let r = to_fractions(&[c], 100, 100);
        assert_eq!(r.len(), 1);
        assert!(
            (r[0].x - 0.0).abs() < 1e-9
                && (r[0].w - 0.4).abs() < 1e-6
                && (r[0].h - 0.16).abs() < 1e-6
        );
    }
}
