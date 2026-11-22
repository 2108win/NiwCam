//! Does the YuNet face detector run through Windows ML, what are its outputs, and where does it
//! find faces? (ADR-008.)
//!
//!   yunet_probe <picture> <model.onnx> [width height]
//!
//! Prints the declared input/output shapes, then decodes the multi-scale outputs the way OpenCV's
//! FaceDetectorYN does and lists the boxes above 0.6.

use std::time::Instant;

use niwcam_fx::still_picture::load_image_fill;
use windows::core::{Interface, HSTRING};
use windows::Win32::System::Com::{CoInitializeEx, COINIT_MULTITHREADED};
use windows::AI::MachineLearning::{
    LearningModel, LearningModelBinding, LearningModelDevice, LearningModelDeviceKind,
    LearningModelSession, TensorFeatureDescriptor, TensorFloat,
};

fn main() {
    let a: Vec<String> = std::env::args().collect();
    if a.len() < 3 {
        eprintln!("usage: yunet_probe <picture> <model.onnx> [width height]");
        std::process::exit(2);
    }
    let (w, h) = (
        a.get(3).and_then(|v| v.parse().ok()).unwrap_or(640usize),
        a.get(4).and_then(|v| v.parse().ok()).unwrap_or(352usize),
    );
    // SAFETY: plain COM start-up of this thread.
    unsafe {
        CoInitializeEx(None, COINIT_MULTITHREADED)
            .ok()
            .expect("COM")
    };
    let model = LearningModel::LoadFromFilePath(&HSTRING::from(a[2].as_str())).expect("load");
    for (what, features) in [
        ("input", model.InputFeatures().unwrap()),
        ("output", model.OutputFeatures().unwrap()),
    ] {
        for f in features {
            let shape = f
                .cast::<TensorFeatureDescriptor>()
                .and_then(|t| t.Shape())
                .map(|s| s.into_iter().collect::<Vec<i64>>())
                .unwrap_or_default();
            println!("{what}: {} {:?}", f.Name().unwrap_or_default(), shape);
        }
    }
    let mut nv12 = Vec::new();
    assert!(load_image_fill(&a[1], w as u32, h as u32, &mut nv12));
    // BGR, 0..255, planar
    let (y, uv) = nv12.split_at(w * h);
    let mut input = vec![0f32; 3 * w * h];
    for j in 0..h {
        for i in 0..w {
            let yy = 1.164 * (y[j * w + i] as i32 - 16) as f32;
            let u = uv[(j / 2) * w + (i & !1)] as f32 - 128.0;
            let v = uv[(j / 2) * w + (i & !1) + 1] as f32 - 128.0;
            let at = j * w + i;
            input[at] = (yy + 2.017 * u).clamp(0.0, 255.0); // B
            input[w * h + at] = (yy - 0.392 * u - 0.813 * v).clamp(0.0, 255.0); // G
            input[2 * w * h + at] = (yy + 1.596 * v).clamp(0.0, 255.0); // R
        }
    }
    let device = LearningModelDevice::Create(LearningModelDeviceKind::DirectXHighPerformance)
        .expect("device");
    let session = LearningModelSession::CreateFromModelOnDevice(&model, &device).expect("session");
    let in_name = model
        .InputFeatures()
        .unwrap()
        .GetAt(0)
        .unwrap()
        .Name()
        .unwrap();
    let shape: windows_collections::IIterable<i64> = vec![1i64, 3, h as i64, w as i64].into();
    let mut outputs: Vec<(String, Vec<f32>)> = Vec::new();
    let mut total = 0.0;
    for n in 0..15 {
        let t0 = Instant::now();
        let binding = LearningModelBinding::CreateFromSession(&session).unwrap();
        binding
            .Bind(
                &in_name,
                &TensorFloat::CreateFromArray(&shape, &input).unwrap(),
            )
            .unwrap();
        let result = session
            .Evaluate(&binding, &HSTRING::new())
            .expect("evaluate");
        outputs.clear();
        for f in model.OutputFeatures().unwrap() {
            let name = f.Name().unwrap();
            let t: TensorFloat = result
                .Outputs()
                .unwrap()
                .Lookup(&name)
                .unwrap()
                .cast()
                .unwrap();
            let view = t.GetAsVectorView().unwrap();
            let mut v = vec![0f32; view.Size().unwrap() as usize];
            view.GetMany(0, &mut v).unwrap();
            outputs.push((name.to_string(), v));
        }
        if n >= 5 {
            total += t0.elapsed().as_secs_f64() * 1000.0;
        }
    }
    println!("GPU: {:.2} ms per run at {w}x{h}", total / 10.0);
    for (name, v) in &outputs {
        println!("  {name}: {} values", v.len());
    }
    // Decode: strides 8, 16, 32; per cell cls, obj, bbox(4), kps(10).
    let mut faces: Vec<(f32, f32, f32, f32, f32)> = Vec::new();
    for stride in [8usize, 16, 32] {
        let get = |prefix: &str| {
            outputs
                .iter()
                .find(|(n, _)| n.contains(prefix) && n.ends_with(&stride.to_string()))
                .map(|(_, v)| v)
        };
        let (Some(cls), Some(obj), Some(bbox)) = (get("cls"), get("obj"), get("bbox")) else {
            println!("no cls/obj/bbox outputs for stride {stride} (names differ): decode skipped");
            continue;
        };
        let (cols, rows) = (w / stride, h / stride);
        for r in 0..rows {
            for c in 0..cols {
                let idx = r * cols + c;
                let score = (cls[idx].clamp(0.0, 1.0) * obj[idx].clamp(0.0, 1.0)).sqrt();
                if score < 0.6 {
                    continue;
                }
                let b = &bbox[idx * 4..idx * 4 + 4];
                let cx = (c as f32 + b[0]) * stride as f32;
                let cy = (r as f32 + b[1]) * stride as f32;
                let (bw, bh) = (b[2].exp() * stride as f32, b[3].exp() * stride as f32);
                faces.push((score, cx - bw / 2.0, cy - bh / 2.0, bw, bh));
            }
        }
    }
    faces.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap());
    for f in faces.iter().take(5) {
        println!(
            "face score {:.2}  x {:.0} y {:.0} w {:.0} h {:.0}  (of {w}x{h})",
            f.0, f.1, f.2, f.3, f.4
        );
    }
    if faces.is_empty() {
        println!("no face above 0.6");
    }
}
