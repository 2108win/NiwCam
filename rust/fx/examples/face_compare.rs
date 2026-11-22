//! Windows' FaceTracker against YuNet on the same grey pictures (ADR-008).
//!
//!   face_compare <picture.png> <yunet.onnx>
//!
//! The picture is read at several shapes (the fill crops it differently each time), plus a flat
//! picture with no face. Prints the faces each detector finds, as fractions of the picture.

use std::time::{Duration, Instant};

use niwcam_core::auto_frame::FaceRect;
use niwcam_fx::face_detect::FaceDetector;
use niwcam_fx::still_picture::load_image_fill;
use niwcam_fx::yunet_detector::YuNetDetector;
use windows::Win32::System::Com::{CoInitializeEx, COINIT_MULTITHREADED};

fn show(label: &str, faces: &[FaceRect], ms: f64) {
    if faces.is_empty() {
        println!("  {label:8} no face  ({ms:.0} ms to the first answer)");
    }
    for f in faces {
        println!(
            "  {label:8} x {:.3} y {:.3} w {:.3} h {:.3}  ({ms:.0} ms)",
            f.x, f.y, f.w, f.h
        );
    }
}

fn run_windows(luma: &[u8], w: u32, h: u32) -> (Vec<FaceRect>, f64) {
    let d = FaceDetector::new();
    let t = Instant::now();
    let mut faces = Vec::new();
    let mut now = 0.0;
    while t.elapsed() < Duration::from_secs(6) {
        if d.due(now) {
            d.offer(luma, w as usize, w, h, 1, now);
        }
        if d.take_result(1, &mut faces) {
            return (faces, t.elapsed().as_secs_f64() * 1000.0);
        }
        std::thread::sleep(Duration::from_millis(5));
        now += 250.0;
    }
    (faces, 6000.0)
}

fn run_yunet(model: &str, luma: &[u8], w: u32, h: u32) -> (Vec<FaceRect>, f64, i32) {
    let d = YuNetDetector::new();
    d.set_model(model);
    let t = Instant::now();
    let mut faces = Vec::new();
    let mut now = 0.0;
    while t.elapsed() < Duration::from_secs(6) {
        if d.due(now) {
            d.offer(luma, w as usize, w, h, 1, now);
        }
        if d.take_result(1, &mut faces) {
            return (faces, t.elapsed().as_secs_f64() * 1000.0, d.state());
        }
        std::thread::sleep(Duration::from_millis(5));
        now += 250.0;
    }
    (faces, 6000.0, d.state())
}

fn main() {
    let a: Vec<String> = std::env::args().collect();
    if a.len() < 3 {
        eprintln!("usage: face_compare <picture.png> <yunet.onnx>");
        std::process::exit(2);
    }
    // SAFETY: plain COM start-up of this thread.
    unsafe {
        CoInitializeEx(None, COINIT_MULTITHREADED)
            .ok()
            .expect("COM")
    };
    for (w, h) in [
        (1280u32, 720u32),
        (1920, 1080),
        (720, 1280),
        (1000, 1000),
        (640, 360),
    ] {
        let mut nv12 = Vec::new();
        assert!(load_image_fill(&a[1], w, h, &mut nv12));
        let luma = &nv12[..w as usize * h as usize];
        println!("{w}x{h}");
        let (win, wms) = run_windows(luma, w, h);
        show("windows", &win, wms);
        let (yu, yms, state) = run_yunet(&a[2], luma, w, h);
        show("yunet", &yu, yms);
        if state != 1 {
            println!("  yunet state {state}");
        }
    }
    let flat = vec![128u8; 1280 * 720];
    println!("flat grey 1280x720 (no face)");
    let (win, wms) = run_windows(&flat, 1280, 720);
    show("windows", &win, wms);
    let (yu, yms, _) = run_yunet(&a[2], &flat, 1280, 720);
    show("yunet", &yu, yms);
}
