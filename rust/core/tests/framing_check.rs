//! Port of `tools/framing_check.cpp`: automatic framing (`auto_frame`).

use niwcam_core::auto_frame::*;

const K4K: FramingGeometry = FramingGeometry {
    frame_width: 3840.0,
    frame_height: 2160.0,
    picture_width: 3840.0,
    picture_height: 2160.0,
};
const K1080: FramingGeometry = FramingGeometry {
    frame_width: 1920.0,
    frame_height: 1080.0,
    picture_width: 1920.0,
    picture_height: 1080.0,
};
const PORTRAIT: FramingGeometry = FramingGeometry {
    frame_width: 1080.0,
    frame_height: 1920.0,
    picture_width: 1080.0,
    picture_height: 1920.0,
};

/// A face of this height (fraction of the frame), centred at (cx, cy), about 3:4.
fn face(g: &FramingGeometry, cx: f64, cy: f64, h: f64) -> FaceRect {
    let w = h * 0.75 * g.frame_height / g.frame_width;
    FaceRect {
        x: cx - w / 2.0,
        y: cy - h / 2.0,
        w,
        h,
    }
}

/// Runs the framer from `t0` for `seconds` at `fps`, with a detection every 0.2 s from `faces`.
fn run(
    f: &mut AutoFramer,
    g: &FramingGeometry,
    t0: &mut f64,
    seconds: f64,
    fps: f64,
    faces: &mut dyn FnMut(f64) -> Vec<FaceRect>,
) -> FramingView {
    let mut v = FramingView::default();
    let mut next_detect = *t0;
    let frames = (seconds * fps).round() as i32;
    for i in 1..=frames {
        let t = *t0 + i as f64 / fps;
        if t >= next_detect {
            f.on_faces(&faces(t), t);
            next_detect += 0.2;
        }
        v = f.step(g, t);
    }
    *t0 += seconds;
    v
}

struct Region {
    x: f64,
    y: f64,
    w: f64,
    h: f64,
}

fn shown(v: &FramingView, g: &FramingGeometry) -> Region {
    let (w, h) = (g.picture_width / v.zoom, g.picture_height / v.zoom);
    Region {
        x: v.center_x * g.frame_width - w / 2.0,
        y: v.center_y * g.frame_height - h / 2.0,
        w,
        h,
    }
}

fn inside(f: &FaceRect, r: &Region, g: &FramingGeometry) -> bool {
    let (x0, y0) = (f.x * g.frame_width, f.y * g.frame_height);
    let (x1, y1) = ((f.x + f.w) * g.frame_width, (f.y + f.h) * g.frame_height);
    x0 >= r.x && y0 >= r.y && x1 <= r.x + r.w && y1 <= r.y + r.h
}

fn one(fc: FaceRect) -> impl FnMut(f64) -> Vec<FaceRect> {
    move |_| vec![fc]
}

#[test]
fn nobody() {
    let mut f = AutoFramer::new();
    let mut t = 0.0;
    let v = run(&mut f, &K4K, &mut t, 5.0, 30.0, &mut |_| vec![]);
    assert!(
        v.zoom == 1.0 && v.center_x == 0.5 && v.center_y == 0.5,
        "stays on the whole picture"
    );
    assert_eq!(f.faces(), 0);
}

#[test]
fn one_face() {
    let mut f = AutoFramer::new();
    let mut t = 0.0;
    let fc = face(&K4K, 0.5, 0.45, 0.15);
    let v = run(&mut f, &K4K, &mut t, 6.0, 30.0, &mut one(fc));
    assert!(
        (v.zoom - 2160.0 / (3.0 * 0.15 * 2160.0)).abs() < 0.02,
        "zoom {}",
        v.zoom
    );
    let r = shown(&v, &K4K);
    assert!(inside(&fc, &r, &K4K));
    let face_y = (fc.y + fc.h / 2.0) * K4K.frame_height;
    assert!(
        ((face_y - r.y) / r.h - 0.40).abs() < 0.01,
        "40% from the top"
    );
    assert!((v.center_x - 0.5).abs() < 0.005);
    assert_eq!(f.faces(), 1);
}

#[test]
fn steady_and_move() {
    let mut f = AutoFramer::new();
    let mut t = 0.0;
    run(
        &mut f,
        &K4K,
        &mut t,
        5.0,
        30.0,
        &mut one(face(&K4K, 0.5, 0.45, 0.15)),
    );
    let settled = f.target();
    let mut n = 0;
    run(&mut f, &K4K, &mut t, 5.0, 30.0, &mut |_| {
        let d = if n % 2 == 1 { 0.01 } else { -0.01 };
        n += 1;
        vec![face(&K4K, 0.5 + d, 0.45 - d, 0.15 + d / 2.0)]
    });
    assert_eq!(f.target(), settled, "small movements keep the shot");
    let v = run(
        &mut f,
        &K4K,
        &mut t,
        6.0,
        30.0,
        &mut one(face(&K4K, 0.75, 0.45, 0.15)),
    );
    assert!(
        (v.center_x - 0.75).abs() < 0.01,
        "a big move: the shot follows, {}",
        v.center_x
    );
}

#[test]
fn goes_wide() {
    let mut f = AutoFramer::new();
    let mut t = 0.0;
    run(
        &mut f,
        &K4K,
        &mut t,
        5.0,
        30.0,
        &mut one(face(&K4K, 0.5, 0.45, 0.15)),
    );
    let mut none = |_| vec![];
    let v = run(&mut f, &K4K, &mut t, 3.0, 30.0, &mut none);
    assert!(v.zoom > 2.0, "missed for 3 s: the shot holds, {}", v.zoom);
    let v = run(&mut f, &K4K, &mut t, 5.0, 30.0, &mut none);
    assert!(
        (v.zoom - 1.0).abs() < 0.02 && (v.center_y - 0.5).abs() < 0.01,
        "back to the whole picture, {}",
        v.zoom
    );
    let v = run(
        &mut f,
        &K4K,
        &mut t,
        6.0,
        30.0,
        &mut one(face(&K4K, 0.3, 0.45, 0.15)),
    );
    assert!(
        v.zoom > 2.0 && (v.center_x - 0.3).abs() < 0.01,
        "framed again"
    );
}

#[test]
fn smart_zoom_limit() {
    assert_eq!(framing_max_zoom(&K4K), 3.0);
    assert_eq!(framing_max_zoom(&K1080), 1.5);
    assert_eq!(
        framing_max_zoom(&FramingGeometry {
            frame_width: 1280.0,
            frame_height: 720.0,
            picture_width: 1280.0,
            picture_height: 720.0
        }),
        1.5
    );
    assert!(
        (framing_max_zoom(&FramingGeometry {
            frame_width: 2560.0,
            frame_height: 1440.0,
            picture_width: 2560.0,
            picture_height: 1440.0
        }) - 2.0)
            .abs()
            < 1e-9
    );
    assert_eq!(framing_max_zoom(&PORTRAIT), 1.5);
    for g in [&K4K, &K1080] {
        let mut f = AutoFramer::new();
        let mut t = 0.0;
        let v = run(
            &mut f,
            g,
            &mut t,
            8.0,
            30.0,
            &mut one(face(g, 0.5, 0.5, 0.04)),
        );
        assert!(
            (v.zoom - framing_max_zoom(g)).abs() < 0.01,
            "a far face: zoom stops at the limit, {}",
            v.zoom
        );
    }
}

#[test]
fn edges() {
    let mut f = AutoFramer::new();
    let mut t = 0.0;
    let v = run(
        &mut f,
        &K4K,
        &mut t,
        6.0,
        30.0,
        &mut one(face(&K4K, 0.04, 0.08, 0.12)),
    );
    let r = shown(&v, &K4K);
    assert!(
        r.x >= -1e-6
            && r.y >= -1e-6
            && r.x + r.w <= K4K.frame_width + 1e-6
            && r.y + r.h <= K4K.frame_height + 1e-6
    );
}

#[test]
fn group_and_small_faces() {
    let mut f = AutoFramer::new();
    let mut t = 0.0;
    let (a, b) = (face(&K4K, 0.30, 0.45, 0.14), face(&K4K, 0.70, 0.47, 0.13));
    let v = run(&mut f, &K4K, &mut t, 6.0, 30.0, &mut |_| vec![a, b]);
    let r = shown(&v, &K4K);
    assert!(
        inside(&a, &r, &K4K) && inside(&b, &r, &K4K),
        "two people: both in the shot"
    );
    assert!(v.zoom > 1.0 && v.zoom < 2.0, "wider than one, {}", v.zoom);

    let (mut alone, mut with_poster) = (AutoFramer::new(), AutoFramer::new());
    let (mut t1, mut t2) = (0.0, 0.0);
    let (me, poster) = (face(&K4K, 0.4, 0.45, 0.15), face(&K4K, 0.9, 0.2, 0.04));
    let v1 = run(&mut alone, &K4K, &mut t1, 6.0, 30.0, &mut |_| vec![me]);
    let v2 = run(&mut with_poster, &K4K, &mut t2, 6.0, 30.0, &mut |_| {
        vec![me, poster]
    });
    assert!(
        (v1.zoom - v2.zoom).abs() < 1e-9 && (v1.center_x - v2.center_x).abs() < 1e-9,
        "a small face far away is left out"
    );
}

#[test]
fn portrait() {
    let mut f = AutoFramer::new();
    let mut t = 0.0;
    let fc = face(&PORTRAIT, 0.5, 0.4, 0.12);
    let v = run(&mut f, &PORTRAIT, &mut t, 6.0, 30.0, &mut one(fc));
    assert!(v.zoom > 1.0 && v.zoom <= 1.5 + 1e-9, "{}", v.zoom);
    assert!(inside(&fc, &shown(&v, &PORTRAIT), &PORTRAIT));
}

#[test]
fn frame_rate() {
    let fc = face(&K4K, 0.6, 0.45, 0.15);
    let (mut a, mut b) = (AutoFramer::new(), AutoFramer::new());
    let (mut ta, mut tb) = (0.0, 0.0);
    let va = run(&mut a, &K4K, &mut ta, 1.0, 30.0, &mut one(fc));
    let vb = run(&mut b, &K4K, &mut tb, 1.0, 60.0, &mut one(fc));
    assert!(
        (va.zoom - vb.zoom).abs() / va.zoom < 0.01 && (va.center_x - vb.center_x).abs() < 0.005,
        "{} vs {}",
        va.zoom,
        vb.zoom
    );
    assert!(va.zoom > 1.2 && va.zoom < 2.1, "{}", va.zoom);
}

#[test]
fn reset_and_shape() {
    let mut f = AutoFramer::new();
    let mut t = 0.0;
    run(
        &mut f,
        &K4K,
        &mut t,
        5.0,
        30.0,
        &mut one(face(&K4K, 0.5, 0.45, 0.15)),
    );
    f.reset();
    let v = f.step(&K4K, t + 0.033);
    assert!(
        v.zoom == 1.0 && v.center_x == 0.5,
        "the whole picture at once"
    );
    assert_eq!(f.faces(), 0);
}

#[test]
fn detection_image() {
    assert_eq!(detection_size(3840, 2160, 640), (640, 360));
    assert_eq!(detection_size(1080, 1920, 640), (360, 640));
    assert_eq!(detection_size(320, 240, 640), (320, 240));
    assert_eq!(
        detection_size(880, 1920, 640),
        (292, 640),
        "even sides (NV12)"
    );
    assert_eq!(
        detection_size(1281, 721, 2000),
        (1280, 720),
        "odd small frame made even"
    );

    let (sw, sh) = (1920usize, 1080usize);
    let pitch = sw + 64;
    let mut src = vec![77u8; pitch * sh];
    let mut dst = vec![0u8; 640 * 360];
    downscale_luma(&src, pitch, sw as u32, sh as u32, &mut dst, 640, 360);
    assert!(dst.iter().all(|&v| v == 77), "flat grey");

    for y in 0..sh {
        for x in 0..sw {
            src[y * pitch + x] = (x * 255 / (sw - 1)) as u8;
        }
    }
    downscale_luma(&src, pitch, sw as u32, sh as u32, &mut dst, 640, 360);
    for y in 0..360 {
        for x in 0..640 {
            let centre = (x as f64 + 0.5) * sw as f64 / 640.0;
            let want = centre * 255.0 / (sw - 1) as f64;
            assert!(
                (dst[y * 640 + x] as f64 - want).abs() <= 2.0,
                "gradient {x},{y}"
            );
            if x > 0 {
                assert!(
                    dst[y * 640 + x] >= dst[y * 640 + x - 1],
                    "monotonic {x},{y}"
                );
            }
        }
    }

    let odd = vec![200u8; 7 * 5];
    let mut out = vec![0u8; 3 * 2];
    downscale_luma(&odd, 7, 7, 5, &mut out, 3, 2);
    assert!(out[0] == 200 && out[5] == 200, "odd sizes");
}
