//! Does a matting model run through Windows ML (DirectML), how fast, and what does it see?
//!
//!   modnet_probe <picture> <model.onnx> <out-dir> [width height]
//!
//! MODNet takes RGB scaled to -1..1, NCHW, sides multiples of 32, and gives one alpha channel.
//! Reports the model's declared shapes, the time per run on the GPU and on the CPU, and writes the
//! alpha as a gray PNG (alpha.png) and the person over a flat colour (cutout.png).

use std::time::Instant;

use niwcam_fx::still_picture::{load_image_fill, save_png};
use windows::core::{Interface, HSTRING};
use windows::Win32::System::Com::{CoInitializeEx, COINIT_MULTITHREADED};
use windows::AI::MachineLearning::{
    LearningModel, LearningModelBinding, LearningModelDevice, LearningModelDeviceKind,
    LearningModelSession, TensorFeatureDescriptor, TensorFloat,
};

fn rgb_input(nv12: &[u8], w: usize, h: usize) -> Vec<f32> {
    let (y, uv) = nv12.split_at(w * h);
    let mut t = vec![0f32; 3 * w * h];
    for j in 0..h {
        for i in 0..w {
            let yy = 1.164 * (y[j * w + i] as i32 - 16) as f32;
            let u = uv[(j / 2) * w + (i & !1)] as f32 - 128.0;
            let v = uv[(j / 2) * w + (i & !1) + 1] as f32 - 128.0;
            let at = j * w + i;
            t[at] = ((yy + 1.596 * v).clamp(0.0, 255.0) - 127.5) / 127.5;
            t[w * h + at] = ((yy - 0.392 * u - 0.813 * v).clamp(0.0, 255.0) - 127.5) / 127.5;
            t[2 * w * h + at] = ((yy + 2.017 * u).clamp(0.0, 255.0) - 127.5) / 127.5;
        }
    }
    t
}

fn run(
    model: &LearningModel,
    kind: LearningModelDeviceKind,
    input: &[f32],
    w: i64,
    h: i64,
) -> windows::core::Result<(Vec<f32>, f64)> {
    let device = LearningModelDevice::Create(kind)?;
    let session = LearningModelSession::CreateFromModelOnDevice(model, &device)?;
    let in_name = model.InputFeatures()?.GetAt(0)?.Name()?;
    let out_name = model.OutputFeatures()?.GetAt(0)?.Name()?;
    let shape: windows_collections::IIterable<i64> = vec![1i64, 3, h, w].into();
    let mut last = Vec::new();
    let mut total = 0.0;
    let runs = 25;
    for n in 0..runs {
        let t0 = Instant::now();
        let binding = LearningModelBinding::CreateFromSession(&session)?;
        binding.Bind(&in_name, &TensorFloat::CreateFromArray(&shape, input)?)?;
        let result = session.Evaluate(&binding, &HSTRING::new())?;
        let out: TensorFloat = result.Outputs()?.Lookup(&out_name)?.cast()?;
        let view = out.GetAsVectorView()?;
        let mut v = vec![0f32; view.Size()? as usize];
        view.GetMany(0, &mut v)?;
        if n >= 5 {
            total += t0.elapsed().as_secs_f64() * 1000.0;
        }
        last = v;
    }
    Ok((last, total / (runs - 5) as f64))
}

fn main() {
    let a: Vec<String> = std::env::args().collect();
    if a.len() < 4 {
        eprintln!("usage: modnet_probe <picture> <model.onnx> <out-dir> [width height]");
        std::process::exit(2);
    }
    let (w, h) = (
        a.get(4).and_then(|v| v.parse().ok()).unwrap_or(512usize),
        a.get(5).and_then(|v| v.parse().ok()).unwrap_or(288usize),
    );
    // SAFETY: plain COM start-up of this thread.
    unsafe {
        CoInitializeEx(None, COINIT_MULTITHREADED)
            .ok()
            .expect("COM")
    };
    let model = match LearningModel::LoadFromFilePath(&HSTRING::from(a[2].as_str())) {
        Ok(m) => m,
        Err(e) => {
            println!("LOAD FAILED: {e}");
            return;
        }
    };
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
            println!("{what}: {} shape {:?}", f.Name().unwrap_or_default(), shape);
        }
    }
    let mut nv12 = Vec::new();
    assert!(
        load_image_fill(&a[1], w as u32, h as u32, &mut nv12),
        "cannot read the picture"
    );
    std::fs::create_dir_all(&a[3]).ok();
    let input = rgb_input(&nv12, w, h);
    for (name, kind) in [
        (
            "GPU (DirectX high performance)",
            LearningModelDeviceKind::DirectXHighPerformance,
        ),
        ("CPU", LearningModelDeviceKind::Cpu),
    ] {
        match run(&model, kind, &input, w as i64, h as i64) {
            Ok((out, ms)) => {
                let (min, max) = out
                    .iter()
                    .fold((f32::MAX, f32::MIN), |(lo, hi), &v| (lo.min(v), hi.max(v)));
                let mean = out.iter().sum::<f32>() / out.len() as f32;
                println!("{name}: {ms:.2} ms per run at {w}x{h}; output {} values, min {min:.3} max {max:.3} mean {mean:.3}", out.len());
                if name.starts_with("GPU") && out.len() >= w * h {
                    // alpha as a gray picture, and the person over green
                    let mut gray = vec![128u8; w * h * 3 / 2];
                    let mut cut = nv12.clone();
                    for k in 0..w * h {
                        let al = out[k].clamp(0.0, 1.0);
                        gray[k] = (16.0 + al * 219.0) as u8;
                        cut[k] = (cut[k] as f32 * al + 120.0 * (1.0 - al)) as u8;
                    }
                    for j in 0..h / 2 {
                        for i in 0..w / 2 {
                            let al = out[(2 * j) * w + 2 * i].clamp(0.0, 1.0);
                            cut[w * h + j * w + 2 * i] =
                                (cut[w * h + j * w + 2 * i] as f32 * al + 60.0 * (1.0 - al)) as u8;
                            cut[w * h + j * w + 2 * i + 1] =
                                (cut[w * h + j * w + 2 * i + 1] as f32 * al + 70.0 * (1.0 - al))
                                    as u8;
                        }
                    }
                    let _ = save_png(&format!("{}/alpha.png", a[3]), &gray, w as u32, h as u32);
                    let _ = save_png(&format!("{}/cutout.png", a[3]), &cut, w as u32, h as u32);
                }
            }
            Err(e) => println!("{name}: FAILED {e}"),
        }
    }
}
