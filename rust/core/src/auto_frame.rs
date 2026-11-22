//! Automatic framing ("Tự căn khung"): from the faces found in the picture, the digital zoom and
//! centre that keep the people in a head-and-shoulders shot, moved there smoothly. Pure logic;
//! the faces come from the face detector. Port of `src/engine/AutoFrame.h`.

use crate::util::clamp;

/// A face, as a fraction of the oriented frame (0..1 on each axis).
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct FaceRect {
    pub x: f64,
    pub y: f64,
    pub w: f64,
    pub h: f64,
}

/// The oriented frame, and the picture shown at zoom 1 (its aspect crop), in pixels.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct FramingGeometry {
    pub frame_width: f64,
    pub frame_height: f64,
    pub picture_width: f64,
    pub picture_height: f64,
}

/// A digital zoom and its centre (0..1 of the oriented frame), as in `ImageSettings`.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct FramingView {
    pub zoom: f64,
    pub center_x: f64,
    pub center_y: f64,
}

impl Default for FramingView {
    fn default() -> Self {
        Self {
            zoom: 1.0,
            center_x: 0.5,
            center_y: 0.5,
        }
    }
}

/// "Smart Zoom": how far framing may zoom into this picture. A third of a 4K picture still has
/// 720 lines, what meeting apps send; smaller pictures get 1.5x (a little soft, still useful).
pub fn framing_max_zoom(g: &FramingGeometry) -> f64 {
    let short_side = g.picture_width.min(g.picture_height);
    clamp(short_side / 720.0, 1.5, 3.0)
}

/// Keeps a view's centre where the whole view stays inside the frame.
pub fn clamp_view(mut v: FramingView, g: &FramingGeometry) -> FramingView {
    if g.frame_width <= 0.0 || g.frame_height <= 0.0 {
        return v;
    }
    let fx = (g.picture_width / v.zoom / g.frame_width).min(1.0);
    let fy = (g.picture_height / v.zoom / g.frame_height).min(1.0);
    v.center_x = clamp(v.center_x, fx / 2.0, 1.0 - fx / 2.0);
    v.center_y = clamp(v.center_y, fy / 2.0, 1.0 - fy / 2.0);
    v
}

/// The shot for these faces: everyone in it, about three face heights tall (head and
/// shoulders), the faces a little above the middle. Faces much smaller than the largest are
/// left out: a poster, a TV, someone far behind.
pub fn compose_shot(faces: &[FaceRect], g: &FramingGeometry) -> FramingView {
    let largest = faces
        .iter()
        .fold(0.0f64, |m, f| m.max(f.h * g.frame_height));
    if largest <= 0.0 {
        return FramingView::default();
    }

    let (mut x0, mut y0, mut x1, mut y1, mut face_w) = (1e9f64, 1e9f64, -1e9f64, -1e9f64, 0.0f64);
    for f in faces {
        if f.h * g.frame_height < 0.4 * largest {
            continue;
        }
        x0 = x0.min(f.x * g.frame_width);
        y0 = y0.min(f.y * g.frame_height);
        x1 = x1.max((f.x + f.w) * g.frame_width);
        y1 = y1.max((f.y + f.h) * g.frame_height);
        face_w = face_w.max(f.w * g.frame_width);
    }
    let need_w = (face_w * 3.2).max((x1 - x0) + face_w * 2.2);
    let need_h = (largest * 3.0).max((y1 - y0) + largest * 2.0);

    let mut v = FramingView {
        zoom: clamp(
            (g.picture_width / need_w).min(g.picture_height / need_h),
            1.0,
            framing_max_zoom(g),
        ),
        ..FramingView::default()
    };
    let view_h = g.picture_height / v.zoom;
    v.center_x = (x0 + x1) / 2.0 / g.frame_width;
    v.center_y = ((y0 + y1) / 2.0 + 0.10 * view_h) / g.frame_height; // faces at 40% from the top
    clamp_view(v, g)
}

/// No face this long: back to the whole picture.
pub const WIDE_AFTER_SEC: f64 = 4.0;
/// Of the view: smaller moves keep the shot.
pub const MOVE_FRACTION: f64 = 0.10;
pub const ZOOM_RATIO: f64 = 1.15;
/// Time constants of the easing.
pub const PAN_SECONDS: f64 = 0.6;
pub const ZOOM_SECONDS: f64 = 0.9;

#[derive(Clone, Debug, Default)]
pub struct AutoFramer {
    started: bool,
    faces: Vec<FaceRect>, // the last faces seen
    face_count: usize,
    new_faces: bool,
    last_face_sec: f64,
    last_sec: f64,
    geometry: FramingGeometry,
    view: FramingView,
    target: FramingView,
}

impl AutoFramer {
    pub fn new() -> Self {
        Self::default()
    }

    /// Back to the whole picture at once: a new source, a new shape, or framing just turned on.
    pub fn reset(&mut self) {
        self.started = false;
        self.faces.clear();
        self.face_count = 0;
        self.new_faces = false;
    }

    /// A detection result (may be empty). Empty results keep the shot for a while: a face is
    /// often missed for a moment (turned away, a hand in front).
    pub fn on_faces(&mut self, faces: &[FaceRect], now_sec: f64) {
        self.face_count = faces.len();
        if faces.is_empty() {
            return;
        }
        self.faces = faces.to_vec();
        self.last_face_sec = now_sec;
        self.new_faces = true;
    }

    /// Once per frame: the view to show now. `now_sec` is a steady clock in seconds.
    pub fn step(&mut self, g: &FramingGeometry, now_sec: f64) -> FramingView {
        if !self.started {
            self.started = true;
            self.view = FramingView::default();
            self.target = FramingView::default();
            self.last_sec = now_sec;
        }
        let dt = clamp(now_sec - self.last_sec, 0.0, 0.25);
        self.last_sec = now_sec;

        if !self.faces.is_empty() && now_sec - self.last_face_sec > WIDE_AFTER_SEC {
            self.faces.clear(); // nobody for a while: show the whole room
            self.new_faces = true;
        }
        if self.new_faces
            || g.picture_width != self.geometry.picture_width
            || g.picture_height != self.geometry.picture_height
        {
            self.new_faces = false;
            self.geometry = *g;
            let shot = if self.faces.is_empty() {
                clamp_view(FramingView::default(), g)
            } else {
                compose_shot(&self.faces, g)
            };
            // Re-frame only when the people moved enough: a steady shot, not one that follows
            // every small movement.
            let view_w = g.picture_width / self.target.zoom;
            let view_h = g.picture_height / self.target.zoom;
            let moved = (shot.center_x - self.target.center_x).abs() * g.frame_width
                > MOVE_FRACTION * view_w
                || (shot.center_y - self.target.center_y).abs() * g.frame_height
                    > MOVE_FRACTION * view_h;
            let zoomed = (shot.zoom / self.target.zoom).ln().abs() > ZOOM_RATIO.ln();
            if moved || zoomed {
                self.target = shot;
            }
        }
        self.target = clamp_view(self.target, g);

        // Ease towards the target, the same whatever the frame rate; zoom in log space.
        let a = 1.0 - (-dt / PAN_SECONDS).exp();
        let b = 1.0 - (-dt / ZOOM_SECONDS).exp();
        self.view.zoom =
            (self.view.zoom.ln() + (self.target.zoom.ln() - self.view.zoom.ln()) * b).exp();
        self.view.center_x += (self.target.center_x - self.view.center_x) * a;
        self.view.center_y += (self.target.center_y - self.view.center_y) * a;
        self.view = clamp_view(self.view, g);
        self.view
    }

    /// Faces in the last detection result (0: nobody seen just now).
    pub fn faces(&self) -> usize {
        self.face_count
    }

    pub fn target(&self) -> FramingView {
        self.target
    }
}

/// Size of the grey copy the face detector looks at: the long side at most `max_long` pixels,
/// both sides even (the detector takes it as NV12).
pub fn detection_size(width: u32, height: u32, max_long: u32) -> (u32, u32) {
    let scale = (max_long as f64 / width.max(height).max(1) as f64).min(1.0);
    let dw = 2.max((width as f64 * scale) as u32 & !1);
    let dh = 2.max((height as f64 * scale) as u32 & !1);
    (dw, dh)
}

/// Shrinks a luma plane to dw x dh (tightly packed): each pixel averages the 2x2 block at the
/// centre of its area. Enough for a face detector, and cheap (four reads per pixel).
pub fn downscale_luma(src: &[u8], pitch: usize, w: u32, h: u32, dst: &mut [u8], dw: u32, dh: u32) {
    if w < 2 || h < 2 || dw == 0 || dh == 0 {
        return;
    }
    for y in 0..dh as u64 {
        let sy = (h as u64 - 2).min((2 * y + 1) * h as u64 / (2 * dh as u64)) as usize;
        let r0 = &src[sy * pitch..];
        let r1 = &src[(sy + 1) * pitch..];
        let out = &mut dst[y as usize * dw as usize..(y as usize + 1) * dw as usize];
        for (x, o) in out.iter_mut().enumerate() {
            let sx = (w as u64 - 2).min((2 * x as u64 + 1) * w as u64 / (2 * dw as u64)) as usize;
            *o = ((r0[sx] as u32 + r0[sx + 1] as u32 + r1[sx] as u32 + r1[sx + 1] as u32 + 2) / 4)
                as u8;
        }
    }
}
