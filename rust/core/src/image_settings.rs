//! What the user set up for the picture (orientation, crop aspect, digital zoom) and the frame
//! geometry that follows from it. Port of `src/engine/ImageSettings.h`.

use crate::nv12::{Nv12ColorAdjust, Nv12Orientation, Nv12Rect};
use crate::util::clamp;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
#[repr(i32)]
pub enum CropAspect {
    #[default]
    Native = 0,
    R16x9 = 1,
    R4x3 = 2,
    R1x1 = 3,
    R9x16 = 4,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ImageSettings {
    pub orientation: Nv12Orientation,
    pub aspect: CropAspect,
    /// 1 .. 4
    pub zoom: f64,
    /// Zoom centre, 0..1 of the oriented frame.
    pub center_x: f64,
    pub center_y: f64,
    pub color: Nv12ColorAdjust,
}

impl Default for ImageSettings {
    fn default() -> Self {
        Self {
            orientation: Nv12Orientation::default(),
            aspect: CropAspect::Native,
            zoom: 1.0,
            center_x: 0.5,
            center_y: 0.5,
            color: Nv12ColorAdjust::default(),
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct FrameLayout {
    /// After rotation.
    pub oriented_width: u32,
    pub oriented_height: u32,
    /// What the virtual camera publishes.
    pub out_width: u32,
    pub out_height: u32,
    /// Region of the oriented frame that is shown.
    pub source: Nv12Rect,
    pub needs_orient: bool,
    /// Zoom, crop or fit active.
    pub needs_resample: bool,

    /// The picture at its own size (what the output is when nothing is fitted).
    pub picture_width: u32,
    pub picture_height: u32,
    /// Fitted: the output has another size (an app holds the virtual camera at the previous
    /// source's size, or a fixed size was chosen). The picture is scaled into this rectangle
    /// (even position and size, centred) and the rest is black bars. Otherwise the rectangle
    /// is the whole output.
    pub fitted: bool,
    pub dest_x: u32,
    pub dest_y: u32,
    pub dest_width: u32,
    pub dest_height: u32,
}

/// "Kích thước camera ảo": the source's own size (best quality), or a fixed one.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
#[repr(i32)]
pub enum OutputSize {
    #[default]
    Auto = 0,
    /// 3840 x 2160
    R2160 = 1,
    /// 1920 x 1080
    R1080 = 2,
    /// 1280 x 720
    R720 = 3,
}

/// The fixed size of a mode, `(0, 0)` for Auto.
pub fn fixed_output_size(s: OutputSize) -> (u32, u32) {
    match s {
        OutputSize::R2160 => (3840, 2160),
        OutputSize::R1080 => (1920, 1080),
        OutputSize::R720 => (1280, 720),
        OutputSize::Auto => (0, 0),
    }
}

/// What the virtual camera should publish next.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct OutputPlan {
    pub width: u32,
    pub height: u32,
    /// Publish a new virtual camera at this size (drops any app using it).
    pub rebuild: bool,
}

/// The size rule. "Tự động": the picture's own size, but an app using the camera keeps the
/// size it has (rebuilding would drop the camera in a call), and the picture is fitted into
/// it until the app lets go. A fixed size works the same way when it changes.
/// `current_*` is 0 when no camera is published. `consumer_holds`: an app streams from the
/// camera now, or did a moment ago (an app restarting its stream briefly lets go).
pub fn plan_output(
    mode: OutputSize,
    picture_w: u32,
    picture_h: u32,
    current_w: u32,
    current_h: u32,
    consumer_holds: bool,
) -> OutputPlan {
    let (mut w, mut h) = (picture_w, picture_h);
    if mode != OutputSize::Auto {
        (w, h) = fixed_output_size(mode);
    }
    let plan = |width, height, rebuild| OutputPlan {
        width,
        height,
        rebuild,
    };
    if current_w == 0 || current_h == 0 {
        return plan(w, h, true);
    }
    if current_w == w && current_h == h {
        return plan(w, h, false);
    }
    if consumer_holds {
        return plan(current_w, current_h, false);
    }
    plan(w, h, true)
}

pub fn aspect_ratio(a: CropAspect) -> f64 {
    match a {
        CropAspect::R16x9 => 16.0 / 9.0,
        CropAspect::R4x3 => 4.0 / 3.0,
        CropAspect::R1x1 => 1.0,
        CropAspect::R9x16 => 9.0 / 16.0,
        CropAspect::Native => 0.0,
    }
}

/// The region of the oriented frame shown at this zoom around this centre (0..1 of the
/// frame), kept inside it. `picture_*` is the picture at zoom 1 (the aspect crop).
pub fn view_rect(
    oriented_w: u32,
    oriented_h: u32,
    picture_w: u32,
    picture_h: u32,
    zoom_in: f64,
    center_x: f64,
    center_y: f64,
) -> Nv12Rect {
    let zoom = clamp(zoom_in, 1.0, 4.0);
    let (ow, oh) = (oriented_w as f64, oriented_h as f64);
    let mut r = Nv12Rect {
        w: picture_w as f64 / zoom,
        h: picture_h as f64 / zoom,
        ..Nv12Rect::default()
    };
    let cx = clamp(center_x, 0.0, 1.0) * ow;
    let cy = clamp(center_y, 0.0, 1.0) * oh;
    r.x = clamp(cx - r.w / 2.0, 0.0, ow - r.w);
    r.y = clamp(cy - r.h / 2.0, 0.0, oh - r.h);
    if zoom <= 1.0001 {
        // Plain crop: whole even pixels, so resampling is an exact copy.
        r.x = (r.x / 2.0).floor() * 2.0;
        r.y = (r.y / 2.0).floor() * 2.0;
    }
    r
}

/// Moves the shown region of a computed layout (automatic framing, every frame): the output's
/// size, bars and fit stay as they are.
pub fn set_layout_view(l: &mut FrameLayout, zoom: f64, center_x: f64, center_y: f64) {
    l.source = view_rect(
        l.oriented_width,
        l.oriented_height,
        l.picture_width,
        l.picture_height,
        zoom,
        center_x,
        center_y,
    );
    l.needs_resample = l.fitted
        || clamp(zoom, 1.0, 4.0) > 1.0001
        || l.picture_width != l.oriented_width
        || l.picture_height != l.oriented_height;
}

/// The picture size depends only on the camera size, rotation and aspect, so zoom and pan can
/// change live without touching the virtual camera. `fit_width`/`fit_height` (both non-zero):
/// the output has that size instead, with the picture scaled to fit inside it and black bars
/// around (see [`FrameLayout`]).
pub fn compute_layout(
    camera_width: u32,
    camera_height: u32,
    s: &ImageSettings,
    fit_width: u32,
    fit_height: u32,
) -> FrameLayout {
    let mut l = FrameLayout::default();
    let swap = s.orientation.swaps_size();
    l.oriented_width = if swap { camera_height } else { camera_width };
    l.oriented_height = if swap { camera_width } else { camera_height };
    l.needs_orient = !s.orientation.is_identity();

    // Largest centred region with the requested aspect.
    let (mut rw, mut rh) = (l.oriented_width as f64, l.oriented_height as f64);
    let ratio = aspect_ratio(s.aspect);
    if ratio > 0.0 {
        if rw / rh > ratio {
            rw = rh * ratio;
        } else {
            rh = rw / ratio;
        }
    }
    l.out_width = 2.max(round_u32(rw) & !1);
    l.out_height = 2.max(round_u32(rh) & !1);

    // Zoom shrinks the shown region around the chosen centre, kept inside.
    let zoom = clamp(s.zoom, 1.0, 4.0);
    l.source = view_rect(
        l.oriented_width,
        l.oriented_height,
        l.out_width,
        l.out_height,
        zoom,
        s.center_x,
        s.center_y,
    );

    l.needs_resample =
        zoom > 1.0001 || l.out_width != l.oriented_width || l.out_height != l.oriented_height;

    l.picture_width = l.out_width;
    l.picture_height = l.out_height;
    l.dest_width = l.out_width;
    l.dest_height = l.out_height;
    if fit_width >= 2 && fit_height >= 2 && (fit_width != l.out_width || fit_height != l.out_height)
    {
        let fit_width = fit_width & !1;
        let fit_height = fit_height & !1;
        let scale = (fit_width as f64 / l.picture_width as f64)
            .min(fit_height as f64 / l.picture_height as f64);
        // Even and inside the output; the side that fills it exactly stays exact.
        let even = |v: f64, limit: u32| clamp(round_u32(v / 2.0) * 2, 2, limit);
        l.dest_width = even(l.picture_width as f64 * scale, fit_width);
        l.dest_height = even(l.picture_height as f64 * scale, fit_height);
        l.dest_x = ((fit_width - l.dest_width) / 2) & !1;
        l.dest_y = ((fit_height - l.dest_height) / 2) & !1;
        l.out_width = fit_width;
        l.out_height = fit_height;
        l.fitted = true;
        l.needs_resample = true;
    }
    l
}

/// `static_cast<uint32_t>(std::lround(v))`: halves round away from zero; below 0 saturates to 0.
fn round_u32(v: f64) -> u32 {
    v.round() as u32
}
