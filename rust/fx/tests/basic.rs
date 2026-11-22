//! Quick self-contained checks (the real bar is `xcheck/run.ps1`, which compares with the C++).
#![allow(clippy::too_many_arguments)]

use niwcam_fx::face_detect::FaceDetector;
use niwcam_fx::segmenter::{blend_background, sample_input, BackgroundScratch, Segmenter, K_SIZE};
use niwcam_fx::still_picture;
use niwcam_fx::window_source::{capture_window_size, WindowCapture};
use windows::Win32::Foundation::HWND;
use windows::Win32::System::Com::{CoInitializeEx, COINIT_MULTITHREADED};

fn com() {
    // SAFETY: initialises COM for the test's thread; the apartment ends with the thread.
    unsafe {
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
    }
}

fn frame(w: usize, h: usize) -> Vec<u8> {
    let mut f = vec![0u8; w * h * 3 / 2];
    for y in 0..h {
        for x in 0..w {
            f[y * w + x] = (16 + (x * 7 + y * 3) % 200) as u8;
        }
    }
    for (i, v) in f[w * h..].iter_mut().enumerate() {
        *v = (64 + (i * 5) % 128) as u8;
    }
    f
}

fn mask_ellipse() -> Vec<u8> {
    let k = K_SIZE as usize;
    let mut m = vec![0u8; k * k];
    for j in 0..k {
        for i in 0..k {
            let dx = (i as f64 - 96.0) / 50.0;
            let dy = (j as f64 - 100.0) / 80.0;
            m[j * k + i] = (255.0 * (1.15 - (dx * dx + dy * dy).sqrt()).clamp(0.0, 1.0)) as u8;
        }
    }
    m
}

#[test]
fn card_renders_and_rejects_bad_input() {
    com();
    let mut out = Vec::new();
    assert!(still_picture::render_card(
        "Quay lại ngay",
        640,
        360,
        None,
        0,
        0,
        &mut out
    ));
    assert_eq!(out.len(), 640 * 360 * 3 / 2);
    assert!(!still_picture::render_card(
        "x", 1, 360, None, 0, 0, &mut out
    ));
    assert!(!still_picture::load_image_fill("", 64, 64, &mut out));
    assert!(!still_picture::load_image_fill(
        r"C:\no\such\file.png",
        64,
        64,
        &mut out
    ));
    // A blurred background of the same size (the dark pause card on top of a picture).
    let bg = frame(320, 240);
    assert!(still_picture::render_card(
        "Quay lại ngay",
        640,
        360,
        Some(&bg),
        320,
        240,
        &mut out
    ));
    // A background too short for its stated size is refused (the C++ would read past it).
    assert!(!still_picture::render_card(
        "x",
        640,
        360,
        Some(&bg[..100]),
        320,
        240,
        &mut out
    ));
}

#[test]
fn save_png_roundtrip_and_errors() {
    com();
    let dir = std::env::temp_dir().join(format!("niwcam-fx-test-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("a.png");
    let f = frame(64, 48);
    assert_eq!(
        still_picture::save_png(path.to_str().unwrap(), &f, 64, 48).0,
        0
    );
    assert!(std::fs::metadata(&path).unwrap().len() > 100);
    // Loading it back fills the frame.
    let mut out = Vec::new();
    assert!(still_picture::load_image_fill(
        path.to_str().unwrap(),
        128,
        96,
        &mut out
    ));
    assert_eq!(out.len(), 128 * 96 * 3 / 2);
    assert!(still_picture::save_png(path.to_str().unwrap(), &[], 64, 48).is_err());
    assert!(still_picture::save_png(path.to_str().unwrap(), &f, 1, 1).is_err());
    // A path that cannot be written: an error and no file left behind.
    let bad = dir.join("no_such_dir").join("b.png");
    assert!(still_picture::save_png(bad.to_str().unwrap(), &f, 64, 48).is_err());
    assert!(!bad.exists());
    let _ = std::fs::remove_dir_all(&dir);
}

fn blended(
    w: usize,
    h: usize,
    padding: usize,
    mask: &[u8],
    strength: i32,
    image: Option<&[u8]>,
    parallel: bool,
) -> Vec<u8> {
    let src = frame(w, h);
    let pitch = w + padding;
    let mut buf = vec![0xABu8; pitch * h * 3 / 2];
    for y in 0..h {
        buf[y * pitch..y * pitch + w].copy_from_slice(&src[y * w..(y + 1) * w]);
    }
    for y in 0..h / 2 {
        buf[pitch * h + y * pitch..pitch * h + y * pitch + w]
            .copy_from_slice(&src[w * h + y * w..w * h + (y + 1) * w]);
    }
    let (y, uv) = buf.split_at_mut(pitch * h);
    let mut s = BackgroundScratch::default();
    blend_background(
        y, uv, pitch, w as u32, h as u32, mask, true, strength, image, &mut s, parallel,
    );
    buf
}

#[test]
fn blend_parallel_equals_serial_and_keeps_padding() {
    let mask = mask_ellipse();
    for (w, h, pad) in [(640, 360, 0), (666, 378, 14), (200, 130, 6)] {
        let a = blended(w, h, pad, &mask, 60, None, false);
        let b = blended(w, h, pad, &mask, 60, None, true);
        assert_eq!(a, b, "{w}x{h}");
        // Padding bytes (0xAB) beyond each luma row stay as they were.
        for y in 0..h {
            assert!(a[y * (w + pad) + w..(y + 1) * (w + pad)]
                .iter()
                .all(|&v| v == 0xAB));
        }
    }
}

#[test]
fn blend_person_untouched_background_replaced() {
    let (w, h) = (320usize, 180usize);
    let src = frame(w, h);
    let all_person = vec![255u8; (K_SIZE * K_SIZE) as usize];
    assert_eq!(blended(w, h, 0, &all_person, 50, None, false), src);
    // Nobody in the picture + an image: the image shows (limited rounding: a * 256 >> 8).
    let image = vec![100u8; w * h * 3 / 2];
    let none = vec![0u8; (K_SIZE * K_SIZE) as usize];
    assert_eq!(blended(w, h, 0, &none, 50, Some(&image), false), image);
    // A mask of the wrong size, or a picture that is too small, changes nothing.
    assert_eq!(blended(w, h, 0, &[255u8; 10], 50, None, false), src);
}

#[test]
fn sample_input_is_normalised() {
    let (w, h) = (320usize, 180usize);
    let f = frame(w, h);
    let t = sample_input(&f[..w * h], &f[w * h..], w, w as u32, h as u32);
    assert_eq!(t.len(), 3 * 192 * 192);
    assert!(t.iter().all(|v| (-1.0..=1.0).contains(v)));
}

#[test]
fn segmenter_without_model_is_unsupported() {
    let s = Segmenter::new();
    assert_eq!(s.state(), 1);
    assert!(s.due(1000.0));
    s.set_model(r"C:\no\such\model.onnx");
    let f = frame(320, 180);
    s.offer(&f[..320 * 180], &f[320 * 180..], 320, 320, 180, 1000.0);
    assert!(!s.due(1010.0)); // busy / interval
    for _ in 0..500 {
        if s.state() != 1 {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    assert_eq!(s.state(), 0);
    assert!(!s.due(5000.0));
    let mut mask = Vec::new();
    assert!(!s.take_mask(&mut mask));
}

#[test]
fn face_detector_gating() {
    let d = FaceDetector::new();
    assert!(d.due(1000.0));
    assert_eq!(d.state(), 1);
    let mut faces = Vec::new();
    assert!(!d.take_result(1, &mut faces));
}

#[test]
fn window_helpers_on_bad_handles() {
    com();
    let (w, h) = capture_window_size(HWND(std::ptr::null_mut()));
    assert_eq!((w, h), (2, 2)); // no rectangle: the minimum
    let mut cap = WindowCapture::new();
    let mut error = String::new();
    assert!(!cap.open(HWND(std::ptr::null_mut()), &mut error));
    assert!(error.starts_with("Không quay được cửa sổ này ("), "{error}");
}

#[test]
fn types_can_be_shared_between_threads() {
    fn send_sync<T: Send + Sync>() {}
    send_sync::<Segmenter>();
    send_sync::<FaceDetector>();
}
