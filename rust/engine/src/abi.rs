//! The flat C API's structs (`src/engine/EngineApi.h`), `#[repr(C)]` with natural alignment: the
//! WinUI app reads them through P/Invoke, so a single wrong offset breaks it silently.
//! `rust/engine/xcheck/abi.ps1` prints `sizeof`/`offsetof` from the C++ header and from here and
//! requires them to be equal. `wchar_t` is `u16`.

#[repr(C)]
#[derive(Clone, Copy)]
pub struct CsFormat {
    pub width: u32,
    pub height: u32,
    pub fps_num: u32,
    pub fps_den: u32,
    /// Native subtype, e.g. 'MJPG', 'NV12', 'H264', 'HEVC'.
    pub fourcc: u32,
    /// The camera's (or this lens') largest mode.
    pub is_native: i32,
    /// Phones: 1 back, 2 front, 3 external; 0 for webcams.
    pub lens: i32,
    pub lens_label: [u16; 64],
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct CsImage {
    /// 0, 90, 180, 270 (clockwise).
    pub rotation: i32,
    pub mirror: i32,
    pub flip: i32,
    /// CropAspect: 0 native, 1 16:9, 2 4:3, 3 1:1, 4 9:16.
    pub aspect: i32,
    /// 1 .. 4
    pub zoom: f64,
    /// 0 .. 1
    pub center_x: f64,
    pub center_y: f64,
    /// -100 .. 100
    pub brightness: i32,
    pub contrast: i32,
    pub saturation: i32,
    /// + warmer, - cooler
    pub temperature: i32,
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct CsStatus {
    /// 0 stopped, 1 starting, 2 running, 3 failed, 4 waiting.
    pub state: i32,
    pub width: u32,
    pub height: u32,
    pub camera_width: u32,
    pub camera_height: u32,
    pub camera_fourcc: u32,
    pub fps: f64,
    pub process_ms: f64,
    pub frames_captured: u64,
    pub frames_dropped: u64,
    pub shared_memory: i32,
    pub consumer_active: i32,
    pub source_rotation: i32,
    pub standby: i32,
    pub picture_width: u32,
    pub picture_height: u32,
    pub source_id: u32,
    pub no_source: i32,
    pub lag_ms: f64,
    pub frames_skipped: u64,
    pub error: [u16; 256],
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct CsBackground {
    pub available: i32,
    pub active: i32,
    pub infer_ms: f64,
    pub faces: i32,
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct CsFraming {
    pub zoom: f64,
    pub center_x: f64,
    pub center_y: f64,
    pub max_zoom: f64,
    pub active: i32,
    pub faces: i32,
    pub available: i32,
    pub camera_source: i32,
}

/// `offset_of!`/`size_of` as lines, in the same format `xcheck/abi_cpp.cpp` prints.
pub fn layout_lines() -> Vec<String> {
    use std::mem::{offset_of, size_of};
    let mut v = Vec::new();
    macro_rules! s {
        ($t:ty, $name:literal) => {
            v.push(format!("sizeof {} {}", $name, size_of::<$t>()));
        };
    }
    macro_rules! o {
        ($t:ty, $tn:literal, $f:ident, $fname:literal) => {
            v.push(format!("offsetof {}.{} {}", $tn, $fname, offset_of!($t, $f)));
        };
    }
    s!(CsFormat, "CsFormat");
    o!(CsFormat, "CsFormat", width, "width");
    o!(CsFormat, "CsFormat", height, "height");
    o!(CsFormat, "CsFormat", fps_num, "fpsNum");
    o!(CsFormat, "CsFormat", fps_den, "fpsDen");
    o!(CsFormat, "CsFormat", fourcc, "fourcc");
    o!(CsFormat, "CsFormat", is_native, "isNative");
    o!(CsFormat, "CsFormat", lens, "lens");
    o!(CsFormat, "CsFormat", lens_label, "lensLabel");
    s!(CsImage, "CsImage");
    o!(CsImage, "CsImage", rotation, "rotation");
    o!(CsImage, "CsImage", mirror, "mirror");
    o!(CsImage, "CsImage", flip, "flip");
    o!(CsImage, "CsImage", aspect, "aspect");
    o!(CsImage, "CsImage", zoom, "zoom");
    o!(CsImage, "CsImage", center_x, "centerX");
    o!(CsImage, "CsImage", center_y, "centerY");
    o!(CsImage, "CsImage", brightness, "brightness");
    o!(CsImage, "CsImage", contrast, "contrast");
    o!(CsImage, "CsImage", saturation, "saturation");
    o!(CsImage, "CsImage", temperature, "temperature");
    s!(CsStatus, "CsStatus");
    o!(CsStatus, "CsStatus", state, "state");
    o!(CsStatus, "CsStatus", width, "width");
    o!(CsStatus, "CsStatus", height, "height");
    o!(CsStatus, "CsStatus", camera_width, "cameraWidth");
    o!(CsStatus, "CsStatus", camera_height, "cameraHeight");
    o!(CsStatus, "CsStatus", camera_fourcc, "cameraFourcc");
    o!(CsStatus, "CsStatus", fps, "fps");
    o!(CsStatus, "CsStatus", process_ms, "processMs");
    o!(CsStatus, "CsStatus", frames_captured, "framesCaptured");
    o!(CsStatus, "CsStatus", frames_dropped, "framesDropped");
    o!(CsStatus, "CsStatus", shared_memory, "sharedMemory");
    o!(CsStatus, "CsStatus", consumer_active, "consumerActive");
    o!(CsStatus, "CsStatus", source_rotation, "sourceRotation");
    o!(CsStatus, "CsStatus", standby, "standby");
    o!(CsStatus, "CsStatus", picture_width, "pictureWidth");
    o!(CsStatus, "CsStatus", picture_height, "pictureHeight");
    o!(CsStatus, "CsStatus", source_id, "sourceId");
    o!(CsStatus, "CsStatus", no_source, "noSource");
    o!(CsStatus, "CsStatus", lag_ms, "lagMs");
    o!(CsStatus, "CsStatus", frames_skipped, "framesSkipped");
    o!(CsStatus, "CsStatus", error, "error");
    s!(CsBackground, "CsBackground");
    o!(CsBackground, "CsBackground", available, "available");
    o!(CsBackground, "CsBackground", active, "active");
    o!(CsBackground, "CsBackground", infer_ms, "inferMs");
    o!(CsBackground, "CsBackground", faces, "faces");
    s!(CsFraming, "CsFraming");
    o!(CsFraming, "CsFraming", zoom, "zoom");
    o!(CsFraming, "CsFraming", center_x, "centerX");
    o!(CsFraming, "CsFraming", center_y, "centerY");
    o!(CsFraming, "CsFraming", max_zoom, "maxZoom");
    o!(CsFraming, "CsFraming", active, "active");
    o!(CsFraming, "CsFraming", faces, "faces");
    o!(CsFraming, "CsFraming", available, "available");
    o!(CsFraming, "CsFraming", camera_source, "cameraSource");
    v
}
