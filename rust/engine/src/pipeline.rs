//! Pipeline: shared by every source. Port of `CaptureEngine::Pipeline` (`CaptureEngine.cpp`).
//!
//! Owns the virtual camera registration and the shared-memory writer, and runs
//! orient -> crop/zoom -> fit -> adjust -> publish on each NV12 frame. It outlives a source:
//! `switch` hands the next source the same camera.

use std::ffi::c_void;
use std::sync::atomic::Ordering::{Relaxed, SeqCst};
use std::sync::{Arc, OnceLock};

use niwcam_core::auto_frame::{framing_max_zoom, AutoFramer, FaceRect, FramingGeometry};
use niwcam_core::image_settings::{
    compute_layout, plan_output, set_layout_view, FrameLayout, ImageSettings, OutputPlan,
    OutputSize,
};
use niwcam_core::nv12::{
    nv12_adjust, nv12_adjust_in_place, nv12_adjust_planes_in_place, nv12_crop_scale_bilinear,
    nv12_crop_scale_bilinear_to, nv12_fill_bars, nv12_orient, nv12_scale,
};
use niwcam_core::skin_smooth::{smooth_face, SkinScratch};
use niwcam_phone::tick_ms;
use niwcam_shm::{
    nv12_bytes, FrameWriter, CAMSTUDIO_ATTR_NATIVE_FPS, CAMSTUDIO_ATTR_NATIVE_SIZE,
    CAMSTUDIO_VCAM_CLSID_STRING, CAMSTUDIO_VCAM_FRIENDLY_NAME,
};
use windows::core::{s, w, GUID, HRESULT, PCWSTR};
use windows::Win32::Foundation::{E_ACCESSDENIED, E_FAIL, S_OK};
use windows::Win32::Media::MediaFoundation::{
    IMFAsyncCallback, IMFVirtualCamera, MFVirtualCameraAccess, MFVirtualCameraAccess_CurrentUser,
    MFVirtualCameraLifetime, MFVirtualCameraLifetime_Session, MFVirtualCameraType,
    MFVirtualCameraType_SoftwareCameraSource,
};
use windows::Win32::System::Diagnostics::Debug::OutputDebugStringW;
use windows::Win32::System::Environment::GetEnvironmentVariableW;
use windows::Win32::System::LibraryLoader::{
    GetProcAddress, LoadLibraryExW, LOAD_LIBRARY_SEARCH_SYSTEM32,
};
use windows::Win32::System::Performance::{QueryPerformanceCounter, QueryPerformanceFrequency};

use crate::capture_engine::{lock, BackgroundMode, Shared, State};
use crate::fx_bridge::{
    blend_background, load_image_fill, BackgroundScratch, FaceDetector, Segmenter,
};

/// Source closed after this long with no app on the camera and no preview on screen (an app
/// switching cameras or a quick glance should not reopen it).
pub(crate) const K_IDLE_CLOSE_MS: u64 = 5000;
/// An app that stopped streaming this recently still counts as using the camera: apps restart
/// their stream when they change mode (Discord does), and rebuilding the camera then drops it.
pub(crate) const K_CONSUMER_GRACE_MS: u64 = 5000;
/// How long the last picture stays on the camera while the next source opens (refreshed while it
/// is still opening). After that, or once the source is reported lost, apps show "no signal".
pub(crate) const K_HOLD_MS: u32 = 10000;

type CreateVirtualCameraFn = unsafe extern "system" fn(
    MFVirtualCameraType,
    MFVirtualCameraLifetime,
    MFVirtualCameraAccess,
    PCWSTR,
    PCWSTR,
    *const GUID,
    u32,
    *mut *mut c_void,
) -> HRESULT;

/// `MFCreateVirtualCamera` exists on Windows 11 only (mfsensorgroup.dll, build 22000). Looked up
/// at run time so the engine loads on Windows 10, where a DirectShow source filter reads the
/// frames instead and no camera is registered here (docs/windows10.md).
fn virtual_camera_api() -> Option<CreateVirtualCameraFn> {
    static FN: OnceLock<Option<CreateVirtualCameraFn>> = OnceLock::new();
    *FN.get_or_init(|| {
        // CAMSTUDIO_FORCE_DSHOW=1: the Windows 10 path on any Windows, for testing.
        // SAFETY: a plain query of an environment variable by a constant name; no buffer given.
        if unsafe { GetEnvironmentVariableW(w!("CAMSTUDIO_FORCE_DSHOW"), None) } != 0 {
            return None;
        }
        // SAFETY: constant names; the module is never unloaded.
        let module =
            unsafe { LoadLibraryExW(w!("mfsensorgroup.dll"), None, LOAD_LIBRARY_SEARCH_SYSTEM32) }
                .ok()?;
        // SAFETY: the export has exactly the signature `CreateVirtualCameraFn` describes.
        let p = unsafe { GetProcAddress(module, s!("MFCreateVirtualCamera")) }?;
        Some(unsafe {
            std::mem::transmute::<unsafe extern "system" fn() -> isize, CreateVirtualCameraFn>(p)
        })
    })
}

/// True when this PC has no virtual camera API (Windows 10) or `CAMSTUDIO_FORCE_DSHOW` is set.
pub(crate) fn directshow_mode() -> bool {
    virtual_camera_api().is_none()
}

/// The QPC clock in milliseconds (`GetTickCount64` moves in 15.6 ms steps).
pub(crate) fn qpc_ms() -> f64 {
    static FREQ: OnceLock<f64> = OnceLock::new();
    let freq = *FREQ.get_or_init(|| {
        let mut f = 0i64;
        // SAFETY: writes one i64.
        unsafe {
            let _ = QueryPerformanceFrequency(&mut f);
        }
        f as f64
    });
    let mut c = 0i64;
    // SAFETY: writes one i64.
    unsafe {
        let _ = QueryPerformanceCounter(&mut c);
    }
    c as f64 * 1000.0 / freq
}

/// Frame pacing on the QPC clock: a frame is ready at or a little before its due time, and taking
/// it moves the due time by exactly one interval, so a faster source gives evenly spread frames
/// (100 fps paced to 30 gives 30/30/40 ms gaps). A late frame (a slow or stalled source) restarts
/// the schedule from now.
pub(crate) fn pace_ready(due_ms: f64, interval_ms: f64) -> bool {
    qpc_ms() >= due_ms - interval_ms / 4.0
}

pub(crate) fn pace_take(due_ms: &mut f64, interval_ms: f64) {
    let now = qpc_ms();
    *due_ms = if now > *due_ms + interval_ms {
        now + interval_ms
    } else {
        *due_ms + interval_ms
    };
}

/// Where the frame being processed lives: the source's own buffer (read only), or one of ours.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Src {
    Data,
    Oriented,
    Output,
    Fit,
}

#[derive(Default)]
struct Buffers {
    oriented: Vec<u8>,
    output: Vec<u8>,
    fit: Vec<u8>,
}

/// The slice of a frame location (the source's `data` or one of our buffers).
macro_rules! buf {
    ($self:ident, $data:expr, $src:expr) => {
        match $src {
            Src::Data => $data,
            Src::Oriented => &$self.bufs.oriented[..],
            Src::Output => &$self.bufs.output[..],
            Src::Fit => &$self.bufs.fit[..],
        }
    };
}

/// Same, when the frame can only be the source's own or the oriented copy (before any other
/// buffer of ours was written): lets the caller borrow `output` or `fit` mutably at once.
macro_rules! src2 {
    ($self:ident, $data:expr, $src:expr) => {
        match $src {
            Src::Oriented => &$self.bufs.oriented[..],
            _ => $data,
        }
    };
}

/// The planes of a frame: Y from `y_off`, UV from `uv_off` (>= `plane_start`, where the UV plane
/// of the buffer begins), as two disjoint mutable slices.
fn planes_mut(
    buf: &mut [u8],
    plane_start: usize,
    y_off: usize,
    uv_off: usize,
) -> (&mut [u8], &mut [u8]) {
    let (a, b) = buf.split_at_mut(plane_start);
    (&mut a[y_off..], &mut b[uv_off - plane_start..])
}

/// Automatic framing, background and skin smoothing: their state and detectors.
struct Effects {
    // Automatic framing (apply_framing).
    framer: AutoFramer,
    detector: Option<FaceDetector>,
    faces: Vec<FaceRect>,
    framing_gen: u32,
    framed_shape: [u32; 5],
    /// Automatic framing applies to cameras only (a webcam, a phone's camera): a shared screen or
    /// a PC window would be zoomed into the faces on it.
    framing_source: bool,
    framing_on: bool,

    // Background effect (apply_background).
    segmenter: Option<Segmenter>,
    mask: Vec<u8>,
    bg_scratch: BackgroundScratch,
    bg_image_nv12: Vec<u8>,
    bg_image_gen: u32,
    bg_image_w: u32,
    bg_image_h: u32,
    /// The segmenter state last put in the engine's status.
    bg_reported: i32,

    // Skin smoothing (apply_smoothing).
    smooth_detector: Option<FaceDetector>,
    smooth_faces: Vec<FaceRect>,
    smooth_faces_at: f64,
    smooth_gen: u32,
    smooth_w: u32,
    smooth_h: u32,
    smooth_reported: i32,
    skin_scratch: SkinScratch,
}

impl Effects {
    fn new() -> Self {
        Effects {
            framer: AutoFramer::new(),
            detector: None,
            faces: Vec::new(),
            framing_gen: 0,
            framed_shape: [0; 5],
            framing_source: false,
            framing_on: false,
            segmenter: None,
            mask: Vec::new(),
            bg_scratch: BackgroundScratch::default(),
            bg_image_nv12: Vec::new(),
            bg_image_gen: u32::MAX,
            bg_image_w: 0,
            bg_image_h: 0,
            bg_reported: 1,
            smooth_detector: None,
            smooth_faces: Vec::new(),
            smooth_faces_at: -1e9,
            smooth_gen: 0,
            smooth_w: 0,
            smooth_h: 0,
            smooth_reported: -1,
            skin_scratch: SkinScratch::default(),
        }
    }

    /// Faces found before now belong to another picture: start again from the whole picture, and
    /// drop detections still under way (they carry the old generation).
    fn restart_framing(&mut self) {
        self.framer.reset();
        self.framing_gen = self.framing_gen.wrapping_add(1);
        if let Some(s) = &self.segmenter {
            s.reset(); // the old mask stays until the first new one
        }
    }

    /// "Nền" (Segmenter.h): wanted for a camera source with the setting on; on while the model
    /// works. A model that failed stays failed (no copy, no blend) until the setting is turned
    /// off.
    fn background_wanted(&self, engine: &Shared) -> bool {
        self.framing_source && engine.bg_mode.load(Relaxed) != 0
    }

    fn background_on(&self, engine: &Shared) -> bool {
        self.background_wanted(engine) && self.segmenter.as_ref().is_none_or(|s| s.state() == 1)
    }

    /// "Làm mịn da": faces are looked for in the picture itself (its own detector, 5 times a
    /// second), so their boxes need no mapping; frames between looks reuse the last boxes.
    fn smoothing_on(&self, engine: &Shared) -> bool {
        self.framing_source && engine.smoothing.load(Relaxed) > 0
    }

    /// An effect that costs real work on every frame: background, skin smoothing, framing (a
    /// crop-scale of the whole frame). They all move by time, not by frame count.
    fn effects_on(&self, engine: &Shared) -> bool {
        self.background_on(engine)
            || self.smoothing_on(engine)
            || (engine.auto_frame.load(Relaxed) && self.framing_source)
    }

    #[expect(clippy::too_many_arguments)]
    fn apply_background(
        &mut self,
        engine: &Shared,
        y: &mut [u8],
        uv: &mut [u8],
        pitch: usize,
        w: u32,
        h: u32,
        parallel: bool,
    ) {
        if self.segmenter.is_none() {
            let s = Segmenter::new();
            s.set_model(&lock(&engine.lock).bg_model);
            self.segmenter = Some(s);
        }
        let (image_path, gen) = {
            let g = lock(&engine.lock);
            (g.bg_image.clone(), g.bg_gen)
        };
        let now = qpc_ms();
        let Some(seg) = self.segmenter.as_ref() else {
            return;
        };
        if seg.due(now) {
            seg.offer(y, uv, pitch, w, h, now); // the picture as it is
        }
        let changed = seg.take_mask(&mut self.mask);
        let available = seg.state();
        let mut active = false;
        if !self.mask.is_empty() {
            let mut have_image = false;
            if engine.bg_mode.load(Relaxed) == BackgroundMode::Image as i32 {
                if gen != self.bg_image_gen || w != self.bg_image_w || h != self.bg_image_h {
                    self.bg_image_gen = gen;
                    self.bg_image_w = w;
                    self.bg_image_h = h;
                    if !load_image_fill(&image_path, w, h, &mut self.bg_image_nv12) {
                        self.bg_image_nv12.clear();
                    }
                }
                have_image = !self.bg_image_nv12.is_empty();
            }
            let image = if have_image {
                Some(&self.bg_image_nv12[..])
            } else {
                None
            };
            blend_background(
                y,
                uv,
                pitch,
                w,
                h,
                &self.mask,
                changed,
                engine.bg_strength.load(Relaxed),
                image,
                &mut self.bg_scratch,
                parallel,
            );
            active = true;
        }
        let mut g = lock(&engine.lock);
        g.bg_state.available = available;
        g.bg_state.active = active;
        g.bg_state.infer_ms = seg.infer_ms();
    }

    fn apply_smoothing(&mut self, engine: &Shared, y: &mut [u8], pitch: usize, w: u32, h: u32) {
        let detector = self.smooth_detector.get_or_insert_with(FaceDetector::new);
        let now = qpc_ms();
        if w != self.smooth_w || h != self.smooth_h {
            self.smooth_w = w;
            self.smooth_h = h;
            self.smooth_gen = self.smooth_gen.wrapping_add(1); // boxes found in another shape are dropped
            self.smooth_faces.clear();
        }
        if detector.due(now) {
            detector.offer(y, pitch, w, h, self.smooth_gen, now);
        }
        let mut found = Vec::new();
        if detector.take_result(self.smooth_gen, &mut found) {
            std::mem::swap(&mut self.smooth_faces, &mut found);
            self.smooth_faces_at = now;
        }
        if now - self.smooth_faces_at > 1500.0 {
            self.smooth_faces.clear(); // nobody seen for a while
        }
        let strength = engine.smoothing.load(Relaxed);
        for face in &self.smooth_faces {
            smooth_face(y, pitch, w, h, face, strength, &mut self.skin_scratch);
        }
        let faces = if detector.state() == 1 {
            self.smooth_faces.len() as i32
        } else {
            -2
        };
        if faces != self.smooth_reported {
            self.smooth_reported = faces;
            lock(&engine.lock).bg_state.faces = faces;
        }
    }

    fn stop_smoothing(&mut self, engine: &Shared) {
        self.smooth_detector = None;
        self.smooth_faces.clear();
        self.smooth_reported = -1;
        lock(&engine.lock).bg_state.faces = -1;
    }

    /// Turned off, or not a camera any more: the model's thread ends.
    fn stop_background(&mut self, engine: &Shared) {
        self.segmenter = None;
        self.bg_reported = 1;
        self.mask.clear();
        lock(&engine.lock).bg_state.active = false;
    }

    /// "Tự căn khung": now and then the detector gets a grey copy of this oriented frame, and the
    /// shown region moves a step towards the people in it. Only for frames that go somewhere
    /// (`publish` returned early otherwise) and only for cameras. Off, it costs nothing: no
    /// detector thread exists until the first framed frame.
    fn apply_framing(&mut self, engine: &Shared, l: &mut FrameLayout, frame: &[u8], pitch: usize) {
        if !engine.auto_frame.load(Relaxed) || !self.framing_source {
            if self.framing_on {
                // The detector's thread goes too; turning framing on again starts a fresh one
                // (also the way out of a detector that gave up).
                self.framing_on = false;
                self.detector = None;
                let mut g = lock(&engine.lock);
                g.framing.active = false;
                g.framing.available = 1;
            }
            return;
        }
        if !self.framing_on {
            self.framing_on = true;
            self.restart_framing();
        }
        let detector = self.detector.get_or_insert_with(FaceDetector::new);
        let now = qpc_ms();
        if detector.due(now) {
            detector.offer(
                frame,
                pitch,
                l.oriented_width,
                l.oriented_height,
                self.framing_gen,
                now,
            );
        }
        if detector.take_result(self.framing_gen, &mut self.faces) {
            self.framer.on_faces(&self.faces, now / 1000.0);
        }
        let g = FramingGeometry {
            frame_width: l.oriented_width as f64,
            frame_height: l.oriented_height as f64,
            picture_width: l.picture_width as f64,
            picture_height: l.picture_height as f64,
        };
        let v = self.framer.step(&g, now / 1000.0);
        set_layout_view(l, v.zoom, v.center_x, v.center_y);

        let mut st = lock(&engine.lock);
        let f = &mut st.framing;
        f.active = true;
        f.available = detector.state();
        f.faces = self.framer.faces() as i32;
        f.zoom = v.zoom;
        f.center_x = v.center_x;
        f.center_y = v.center_y;
        f.max_zoom = framing_max_zoom(&g);
    }
}

pub(crate) struct Pipeline {
    engine: Arc<Shared>,
    dshow: bool,
    vcam: Option<IMFVirtualCamera>,
    /// Size the virtual camera was published with.
    vcam_width: u32,
    vcam_height: u32,
    /// And its rate (0: not known, Windows 10).
    camera_fps: f64,
    /// QPC time the next camera frame is due (effects on).
    camera_due_ms: f64,
    fps_num: u32,
    fps_den: u32,
    writer: FrameWriter,
    failure: String,
    broken: bool,
    broken_hr: HRESULT,
    /// The source's frames.
    width: u32,
    height: u32,
    begins: u32,
    layout: FrameLayout,
    image: ImageSettings,
    layout_gen_seen: u32,
    last_relayout: u64,
    /// Last time an app was streaming.
    consumer_seen: u64,
    no_rebuild_until: u64,
    bufs: Buffers,
    /// The layout `bufs.fit`'s bars were painted for.
    bars_for: [u32; 6],
    interval_100ns: f64,
    work_ms: f64,
    work_frames: u32,
    first_ts: i64,
    received: u64,
    dropped: u64,
    frames: u32,
    last_write_tick: u64,
    stat_start: u64,
    last_fps: f64,
    fx: Effects,
}

impl Pipeline {
    pub(crate) fn new(engine: Arc<Shared>) -> Self {
        let dshow = directshow_mode();
        let mut writer = FrameWriter::with_names(engine.names.clone());
        writer.use_local(dshow);
        Pipeline {
            engine,
            dshow,
            vcam: None,
            vcam_width: 0,
            vcam_height: 0,
            camera_fps: 0.0,
            camera_due_ms: 0.0,
            fps_num: 30,
            fps_den: 1,
            writer,
            failure: String::new(),
            broken: false,
            broken_hr: S_OK,
            width: 0,
            height: 0,
            begins: 0,
            layout: FrameLayout::default(),
            image: ImageSettings::default(),
            layout_gen_seen: u32::MAX,
            last_relayout: 0,
            consumer_seen: 0,
            no_rebuild_until: 0,
            bufs: Buffers::default(),
            bars_for: [0; 6],
            interval_100ns: 333333.0,
            work_ms: 0.0,
            work_frames: 0,
            first_ts: -1,
            received: 0,
            dropped: 0,
            frames: 0,
            last_write_tick: 0,
            stat_start: tick_ms(),
            last_fps: 0.0,
            fx: Effects::new(),
        }
    }

    /// Windows 10: frames go to the DirectShow filter's channel, no camera is registered.
    #[allow(dead_code)]
    pub(crate) fn direct_show(&self) -> bool {
        self.dshow
    }

    /// A source (a new one, or the same one again) delivers width x height frames from now on.
    /// Publishes the virtual camera for it, or keeps the one already published when an app is
    /// using it (`plan_output`): apps using it (Discord, Zoom...) never lose it.
    pub(crate) fn begin(
        &mut self,
        width: u32,
        height: u32,
        fps_num: u32,
        fps_den: u32,
        subtype: &GUID,
    ) -> HRESULT {
        self.begins += 1;
        self.width = width;
        self.height = height;
        self.fps_num = if fps_num != 0 { fps_num } else { 30 };
        self.fps_den = if fps_num != 0 { fps_den } else { 1 };
        self.interval_100ns = if fps_num != 0 {
            1e7 * fps_den as f64 / fps_num as f64
        } else {
            333333.0
        };
        self.first_ts = -1;
        self.received = 0;
        self.dropped = 0;
        self.fx.restart_framing();
        self.relayout();
        if self.broken {
            return self.broken_hr;
        }

        let mut g = lock(&self.engine.lock);
        let s = &mut g.status;
        s.state = State::Running;
        s.error.clear();
        s.camera_width = width;
        s.camera_height = height;
        s.camera_subtype = *subtype;
        self.engine.notify();
        S_OK
    }

    pub(crate) fn end(&mut self) {
        self.writer.close();
        if let Some(vcam) = self.vcam.take() {
            // SAFETY: plain calls on the camera we created.
            unsafe {
                let _ = vcam.Stop();
            }
            Self::remove_camera(&vcam);
            // SAFETY: as above.
            unsafe {
                let _ = vcam.Shutdown();
            }
        }
        self.vcam_width = 0;
        self.vcam_height = 0;
    }

    /// Unregisters the camera outright. Without this a DirectShow app that keeps its graph after
    /// the camera went away (Discord does) holds the old instance, and Frame Server refuses the
    /// next one with `E_ACCESSDENIED`.
    fn remove_camera(vcam: &IMFVirtualCamera) {
        // SAFETY: plain call on a live interface.
        let hr = match unsafe { vcam.Remove() } {
            Ok(()) => S_OK,
            Err(e) => e.code(),
        };
        let msg: Vec<u16> = format!(
            "[NiwCamEngine] IMFVirtualCamera::Remove 0x{:08X}\n\0",
            hr.0 as u32
        )
        .encode_utf16()
        .collect();
        // SAFETY: `msg` is NUL-terminated and outlives the call.
        unsafe { OutputDebugStringW(PCWSTR(msg.as_ptr())) };
    }

    /// The virtual camera is registered (it outlives a lost source). Windows 10: the channel is
    /// open.
    pub(crate) fn live(&self) -> bool {
        if self.dshow {
            self.vcam_width != 0
        } else {
            self.vcam.is_some()
        }
    }

    /// How many times a source started (`begin`): tells whether a run got that far.
    pub(crate) fn begins(&self) -> u32 {
        self.begins
    }

    /// The camera had to be rebuilt and could not be: nothing more can be published.
    pub(crate) fn broken(&self) -> bool {
        self.broken
    }

    /// Some app is streaming from the virtual camera right now (Frame Server started our source's
    /// stream). Cheap: the mapping is looked up at most twice a second until it exists.
    pub(crate) fn consumer_active(&mut self) -> bool {
        self.writer.try_open();
        self.writer.consumer_active()
    }

    pub(crate) fn failure(&self) -> &str {
        &self.failure
    }

    /// The camera's frame size now: what a still picture is rendered at.
    pub(crate) fn out_width(&mut self) -> u32 {
        self.writer.try_open();
        self.writer.width()
    }

    pub(crate) fn out_height(&self) -> u32 {
        self.writer.height()
    }

    /// A still picture (NV12, `out_width` x `out_height`) straight to the camera, as it is.
    pub(crate) fn write_still(&mut self, nv12: &[u8], w: u32, h: u32) {
        self.writer.try_open();
        if !self.writer.is_open() {
            return;
        }
        self.writer.write(nv12, w as usize, w, h);
        self.last_write_tick = tick_ms();
    }

    pub(crate) fn copy_latest(&mut self, out: &mut Vec<u8>) -> Option<(u32, u32)> {
        self.writer.try_open();
        self.writer.copy_latest(out)
    }

    /// Keeps the last picture on the camera while the next source opens (see
    /// `FrameWriter::hold_for`).
    pub(crate) fn hold(&self) {
        self.writer.hold_for(K_HOLD_MS);
    }

    pub(crate) fn release_hold(&mut self) {
        self.writer.release_hold();
    }

    /// Picks up a change of picture shape (settings, the phone turned), of the size setting, or
    /// an app letting go of the camera. Called on every frame, and by `run` in standby; cheap when
    /// nothing changed.
    pub(crate) fn tick(&mut self) {
        let gen = self.engine.layout_gen.load(SeqCst);
        let now = tick_ms();
        if gen == self.layout_gen_seen && now.wrapping_sub(self.last_relayout) < 1000 {
            return;
        }
        self.layout_gen_seen = gen;
        self.last_relayout = now;
        if self.width != 0 {
            self.relayout();
        }
    }

    /// One source frame. `timestamp_100ns` is on the source's own clock. `data` holds the NV12
    /// planes (`pitch` bytes a row, the UV plane right after `height` rows).
    pub(crate) fn frame(&mut self, data: &[u8], pitch: usize, timestamp_100ns: i64) {
        self.count(timestamp_100ns);
        self.tick();
        if self.broken {
            return;
        }
        self.writer.try_open();
        self.publish(data, pitch);
    }

    /// A frame that arrived but is not worth converting (see `will_publish`): counted only.
    pub(crate) fn count(&mut self, timestamp_100ns: i64) {
        // Dropped = frames the timeline says should have arrived minus those that did; counting
        // per gap would also count jitter as drops.
        if self.first_ts < 0 {
            self.first_ts = timestamp_100ns;
        }
        self.received += 1;
        let missing = ((timestamp_100ns - self.first_ts) as f64 / self.interval_100ns + 1.5)
            .floor()
            - self.received as f64;
        self.dropped = if missing > 0.0 { missing as u64 } else { 0 };

        self.frames += 1;
        let now = tick_ms();
        if now.wrapping_sub(self.stat_start) >= 1000 {
            let mut g = lock(&self.engine.lock);
            let s = &mut g.status;
            s.fps = self.frames as f64 * 1000.0 / now.wrapping_sub(self.stat_start) as f64;
            s.frames_captured += self.frames as u64;
            s.frames_dropped = self.dropped;
            s.process_ms = if self.work_frames != 0 {
                self.work_ms / self.work_frames as f64
            } else {
                0.0
            };
            s.shared_memory = self.writer.is_open();
            s.consumer_active = self.writer.consumer_active();
            self.last_fps = s.fps;
            self.frames = 0;
            self.work_ms = 0.0;
            self.work_frames = 0;
            self.stat_start = now;
        }
    }

    /// Whether the next frame will go anywhere (an app, the 1-a-second keep-fresh write, the
    /// preview). A window source checks this before reading the frame back from the GPU.
    pub(crate) fn will_publish(&mut self) -> bool {
        self.writer.try_open();
        self.writer.consumer_active()
            || (self.writer.is_open() && tick_ms().wrapping_sub(self.last_write_tick) >= 1000)
            || self.engine.preview_wanted(self.nominal_fps())
            || self.engine.recorder.active()
            || self.engine.snap_wanted.load(SeqCst)
    }

    pub(crate) fn last_fps(&self) -> f64 {
        self.last_fps
    }

    /// The rate the source was opened at (`begin`).
    pub(crate) fn nominal_fps(&self) -> f64 {
        if self.fps_den != 0 {
            self.fps_num as f64 / self.fps_den as f64
        } else {
            30.0
        }
    }

    /// Automatic framing applies to cameras only (a webcam, a phone's camera): a shared screen or
    /// a PC window would be zoomed into the faces on it.
    pub(crate) fn set_framing_source(&mut self, camera: bool) {
        self.fx.framing_source = camera;
        lock(&self.engine.lock).framing.camera_source = camera;
    }

    /// Applies the size rule and computes the layout for the current settings.
    fn relayout(&mut self) {
        let (image, mode) = {
            let g = lock(&self.engine.lock);
            (
                g.effective_image(),
                match self.engine.output_size.load(SeqCst) {
                    1 => OutputSize::R2160,
                    2 => OutputSize::R1080,
                    3 => OutputSize::R720,
                    _ => OutputSize::Auto,
                },
            )
        };
        let natural = compute_layout(self.width, self.height, &image, 0, 0);
        let now = tick_ms();
        let consumer = self.consumer_active();
        if consumer {
            self.consumer_seen = now;
        }
        let holds =
            consumer || (self.consumer_seen != 0 && now - self.consumer_seen < K_CONSUMER_GRACE_MS);
        let live = self.live();
        let mut plan = plan_output(
            mode,
            natural.picture_width,
            natural.picture_height,
            if live { self.vcam_width } else { 0 },
            if live { self.vcam_height } else { 0 },
            holds,
        );
        // A rebuild that was refused a moment ago (an app still holds the old camera): stay
        // fitted for a while instead of trying again on every frame.
        if plan.rebuild && live && now < self.no_rebuild_until {
            plan = OutputPlan {
                width: self.vcam_width,
                height: self.vcam_height,
                rebuild: false,
            };
        }
        if plan.rebuild {
            self.register(plan.width, plan.height);
        }

        self.image = image;
        self.layout = if self.live() {
            compute_layout(
                self.width,
                self.height,
                &image,
                self.vcam_width,
                self.vcam_height,
            )
        } else {
            natural
        };
        // Another shape (the phone turned, rotation, aspect): faces found so far are elsewhere now.
        let l = &self.layout;
        let shape = [
            l.oriented_width,
            l.oriented_height,
            l.picture_width,
            l.picture_height,
            image.orientation.rotation as u32
                | if image.orientation.mirror { 1 << 9 } else { 0 }
                | if image.orientation.flip { 1 << 10 } else { 0 },
        ];
        if shape != self.fx.framed_shape {
            self.fx.framed_shape = shape;
            self.fx.restart_framing();
        }
        let mut g = lock(&self.engine.lock);
        let s = &mut g.status;
        s.width = self.layout.out_width;
        s.height = self.layout.out_height;
        s.picture_width = self.layout.picture_width;
        s.picture_height = self.layout.picture_height;
    }

    /// Publishes the virtual camera at this size, in place of the current one.
    fn register(&mut self, width: u32, height: u32) {
        let (old_w, old_h) = (self.vcam_width, self.vcam_height);
        let mut hr = self.create(width, height);
        if hr.is_err() && old_w != 0 && hr == E_ACCESSDENIED && self.create(old_w, old_h).is_ok() {
            // An app (DirectShow) still holds the camera we just removed: put back the size it
            // had, keep fitting into it, and try the new size later.
            self.no_rebuild_until = tick_ms() + 30000;
            hr = S_OK;
        }
        self.broken = hr.is_err();
        self.broken_hr = hr;
    }

    fn create(&mut self, width: u32, height: u32) -> HRESULT {
        if self.dshow {
            // No camera to register: the channel the DirectShow filter reads is ours. It takes
            // this size unless an app streams from it right now (then its size stays, and the
            // picture is fitted into it, like a camera an app holds).
            return match self.writer.open_local(width, height) {
                Ok((w, h)) => {
                    self.vcam_width = w;
                    self.vcam_height = h;
                    S_OK
                }
                Err(e) => {
                    self.failure = "Không mở được bộ nhớ chia sẻ của camera ảo".to_string();
                    e.code()
                }
            };
        }
        self.end();
        let Some(api) = virtual_camera_api() else {
            return E_FAIL;
        };
        let mut hr = E_FAIL;
        // E_ACCESSDENIED: an app still holds a camera instance we removed a moment ago. Retry a
        // few times with a fresh registration.
        for attempt in 0..4 {
            if attempt != 0 {
                std::thread::sleep(std::time::Duration::from_millis(500));
            }
            let name: Vec<u16> = CAMSTUDIO_VCAM_FRIENDLY_NAME
                .encode_utf16()
                .chain([0])
                .collect();
            let clsid: Vec<u16> = CAMSTUDIO_VCAM_CLSID_STRING
                .encode_utf16()
                .chain([0])
                .collect();
            let mut raw: *mut c_void = std::ptr::null_mut();
            // SAFETY: the strings are NUL-terminated and outlive the call; no categories.
            hr = unsafe {
                api(
                    MFVirtualCameraType_SoftwareCameraSource,
                    MFVirtualCameraLifetime_Session,
                    MFVirtualCameraAccess_CurrentUser,
                    PCWSTR(name.as_ptr()),
                    PCWSTR(clsid.as_ptr()),
                    std::ptr::null(),
                    0,
                    &mut raw,
                )
            };
            if hr.is_err() || raw.is_null() {
                self.failure =
                    "Không tạo được camera ảo (hãy cài lại NiwCam bằng file NiwCam-Setup)"
                        .to_string();
                return if hr.is_err() { hr } else { E_FAIL };
            }
            // SAFETY: MFCreateVirtualCamera returned a new reference to an IMFVirtualCamera.
            let vcam = unsafe { <IMFVirtualCamera as windows::core::Interface>::from_raw(raw) };
            let (nw, nh) = (
                GUID::from_values(
                    CAMSTUDIO_ATTR_NATIVE_SIZE.0,
                    CAMSTUDIO_ATTR_NATIVE_SIZE.1,
                    CAMSTUDIO_ATTR_NATIVE_SIZE.2,
                    CAMSTUDIO_ATTR_NATIVE_SIZE.3,
                ),
                GUID::from_values(
                    CAMSTUDIO_ATTR_NATIVE_FPS.0,
                    CAMSTUDIO_ATTR_NATIVE_FPS.1,
                    CAMSTUDIO_ATTR_NATIVE_FPS.2,
                    CAMSTUDIO_ATTR_NATIVE_FPS.3,
                ),
            );
            // SAFETY: plain attribute stores and a start on the camera just created.
            let started = unsafe {
                let _ = vcam.SetUINT64(&nw, ((width as u64) << 32) | height as u64);
                let _ = vcam.SetUINT64(&nh, ((self.fps_num as u64) << 32) | self.fps_den as u64);
                vcam.Start(None::<&IMFAsyncCallback>)
            };
            match started {
                Ok(()) => {
                    self.vcam = Some(vcam);
                    hr = S_OK;
                    break;
                }
                Err(e) => hr = e.code(),
            }
            Self::remove_camera(&vcam);
            // SAFETY: as above.
            unsafe {
                let _ = vcam.Shutdown();
            }
            if hr != E_ACCESSDENIED {
                break;
            }
        }
        if hr.is_err() {
            // An app (DirectShow, e.g. Discord) still holds an old instance of the camera:
            // closing it, or a fresh Frame Server, clears this.
            self.failure = if hr == E_ACCESSDENIED {
                "Một ứng dụng (thường là Discord) vẫn giữ camera ảo cũ. Tắt camera trong ứng dụng đó rồi bật lại, hoặc khởi động lại dịch vụ camera"
            } else {
                "Không bật được camera ảo"
            }
            .to_string();
            return hr;
        }
        self.vcam_width = width;
        self.vcam_height = height;
        self.camera_fps = self.fps_num as f64 / self.fps_den as f64; // what MediaSource offers apps at most
        self.consumer_seen = 0;
        S_OK
    }

    fn publish(&mut self, data: &[u8], pitch: usize) {
        let t0 = qpc_ms();
        let mut l = self.layout; // a copy: automatic framing moves the shown region every frame
        let image = self.image;
        let engine = Arc::clone(&self.engine);

        // Nobody is watching (no app on the virtual camera, preview hidden): skip the per-frame
        // work, just keep the source flowing. One frame a second still goes to the camera, so an
        // app that opens it gets a picture at once rather than "no signal" until the next frame
        // of a slow source (a still phone screen sends ~4 a second).
        let tick = tick_ms();
        let consumer = self.writer.consumer_active();
        // An app gets at most the rate the camera was registered with (MediaSource offers no
        // faster mode). With an effect on, the frames in between are not worth their work (blur
        // on a 1080p100 webcam took 1.4 cores for a 30 fps call): one frame per camera interval,
        // and the preview shows those same frames.
        let capped = consumer
            && self.fx.effects_on(&engine)
            && self.camera_fps > 0.0
            && self.nominal_fps() > self.camera_fps * 1.1;
        let (to_camera, to_preview);
        if capped {
            let interval = 1000.0 / self.camera_fps;
            to_camera = pace_ready(self.camera_due_ms, interval);
            if to_camera {
                pace_take(&mut self.camera_due_ms, interval);
            }
            to_preview = to_camera && engine.preview_due(self.camera_fps);
        } else {
            to_camera = consumer
                || (self.writer.is_open() && tick.wrapping_sub(self.last_write_tick) >= 1000);
            to_preview = engine.preview_due(self.nominal_fps());
        }
        let to_recording = engine.recorder.active();
        let to_snapshot = engine.snap_wanted.load(SeqCst);
        if !to_camera && !to_preview && !to_recording && !to_snapshot {
            return;
        }

        let mut frame = Src::Data;
        let mut owned = false; // set once the frame lives in our own buffer
        let mut frame_pitch = pitch;
        if l.needs_orient {
            self.bufs
                .oriented
                .resize(nv12_bytes(l.oriented_width, l.oriented_height) as usize, 0);
            nv12_orient(
                data,
                pitch,
                self.width,
                self.height,
                &mut self.bufs.oriented,
                &image.orientation,
            );
            frame = Src::Oriented;
            owned = true;
            frame_pitch = l.oriented_width as usize;
        }
        {
            let f = src2!(self, data, frame);
            self.fx.apply_framing(&engine, &mut l, f, frame_pitch);
        }

        // The picture alone (no bars): what the preview shows. Its Y plane starts at `pic_y`, its
        // UV plane at `pic_uv`, both inside `pic_src`.
        let (out_w, out_h) = (l.out_width as usize, l.out_height as usize);
        let mut pic_src;
        let mut pic_y;
        let mut pic_uv;
        let mut pic_pitch;
        if l.fitted {
            // Scaled into its rectangle of a frame at the camera's size. The bars are painted
            // once per layout; frames only write the rectangle.
            let bytes = nv12_bytes(l.out_width, l.out_height) as usize;
            let rect = [
                l.out_width,
                l.out_height,
                l.dest_x,
                l.dest_y,
                l.dest_width,
                l.dest_height,
            ];
            if self.bufs.fit.len() != bytes || rect != self.bars_for {
                self.bufs.fit.resize(bytes, 0);
                nv12_fill_bars(
                    &mut self.bufs.fit,
                    l.out_width,
                    l.out_height,
                    l.dest_x,
                    l.dest_y,
                    l.dest_width,
                    l.dest_height,
                );
                self.bars_for = rect;
            }
            let y_off = l.dest_y as usize * out_w + l.dest_x as usize;
            let plane = out_w * out_h;
            let uv_off = plane + (l.dest_y as usize / 2) * out_w + l.dest_x as usize;
            {
                let src = src2!(self, data, frame);
                let (dy, duv) = planes_mut(&mut self.bufs.fit, plane, y_off, uv_off);
                nv12_crop_scale_bilinear_to(
                    src,
                    frame_pitch,
                    l.oriented_width,
                    l.oriented_height,
                    &l.source,
                    dy,
                    duv,
                    out_w,
                    l.dest_width,
                    l.dest_height,
                );
                if !image.color.is_identity() {
                    // The bars stay black.
                    nv12_adjust_planes_in_place(
                        dy,
                        duv,
                        out_w,
                        l.dest_width,
                        l.dest_height,
                        &image.color,
                    );
                }
            }
            frame = Src::Fit;
            frame_pitch = out_w;
            pic_src = Src::Fit;
            pic_y = y_off;
            pic_uv = uv_off;
            pic_pitch = frame_pitch;
        } else {
            if l.needs_resample {
                self.bufs
                    .output
                    .resize(nv12_bytes(l.out_width, l.out_height) as usize, 0);
                let src = src2!(self, data, frame);
                nv12_crop_scale_bilinear(
                    src,
                    frame_pitch,
                    l.oriented_width,
                    l.oriented_height,
                    &l.source,
                    &mut self.bufs.output,
                    l.out_width,
                    l.out_height,
                );
                frame = Src::Output;
                owned = true;
                frame_pitch = out_w;
            }
            if !image.color.is_identity() {
                // In place on our own buffer, or copy-and-adjust from the source's.
                match frame {
                    Src::Data => {
                        self.bufs
                            .output
                            .resize(nv12_bytes(l.out_width, l.out_height) as usize, 0);
                        nv12_adjust(
                            data,
                            frame_pitch,
                            &mut self.bufs.output,
                            l.out_width,
                            l.out_height,
                            &image.color,
                        );
                        frame = Src::Output;
                        owned = true;
                    }
                    Src::Oriented => nv12_adjust_in_place(
                        &mut self.bufs.oriented,
                        l.out_width,
                        l.out_height,
                        &image.color,
                    ),
                    _ => nv12_adjust_in_place(
                        &mut self.bufs.output,
                        l.out_width,
                        l.out_height,
                        &image.color,
                    ),
                }
                frame_pitch = out_w;
            }
            if self.fx.background_on(&engine) && !owned {
                self.copy_to_output(data, frame_pitch, l.out_width, l.out_height);
                frame = Src::Output;
                owned = true;
                frame_pitch = out_w;
            }
            pic_src = frame;
            pic_y = 0;
            pic_uv = frame_pitch * out_h;
            pic_pitch = frame_pitch;
        }
        if self.fx.smoothing_on(&engine) && !owned && !l.fitted {
            // The source's own buffer is never written.
            self.copy_to_output(data, frame_pitch, l.out_width, l.out_height);
            frame = Src::Output;
            frame_pitch = out_w;
            pic_src = frame;
            pic_y = 0;
            pic_uv = frame_pitch * out_h;
            pic_pitch = frame_pitch;
        }
        let plane_start = pic_pitch * out_h;
        if self.fx.smoothing_on(&engine) {
            let buf = match pic_src {
                Src::Oriented => Some(&mut self.bufs.oriented[..]),
                Src::Output => Some(&mut self.bufs.output[..]),
                Src::Fit => Some(&mut self.bufs.fit[..]),
                Src::Data => None, // not reachable: the checks above made the picture ours
            };
            if let Some(buf) = buf {
                self.fx.apply_smoothing(
                    &engine,
                    &mut buf[pic_y..],
                    pic_pitch,
                    l.dest_width,
                    l.dest_height,
                );
            }
        } else if self.fx.smooth_detector.is_some() {
            self.fx.stop_smoothing(&engine);
        }
        // On every core for a big picture or many frames a second (a recording, or a camera with
        // no cap: Windows 10), else on this thread: less CPU in all.
        let many_frames = self.nominal_fps() > 35.0 && (to_recording || (consumer && !capped));
        if self.fx.background_on(&engine) {
            let buf = match pic_src {
                Src::Oriented => Some(&mut self.bufs.oriented[..]),
                Src::Output => Some(&mut self.bufs.output[..]),
                Src::Fit => Some(&mut self.bufs.fit[..]),
                Src::Data => None,
            };
            if let Some(buf) = buf {
                let (y, uv) = planes_mut(buf, plane_start, pic_y, pic_uv);
                self.fx.apply_background(
                    &engine,
                    y,
                    uv,
                    pic_pitch,
                    l.dest_width,
                    l.dest_height,
                    many_frames || l.dest_width as u64 * l.dest_height as u64 > 1920 * 1088,
                );
            }
        } else if self.fx.segmenter.is_some() && !self.fx.background_wanted(&engine) {
            self.fx.stop_background(&engine);
        } else if let Some(state) = self.fx.segmenter.as_ref().map(|s| s.state()) {
            if self.fx.bg_reported != state {
                // The model failed after the last blend: say so (Windows ML missing, an operator
                // it lacks).
                self.fx.bg_reported = state;
                let mut g = lock(&engine.lock);
                g.bg_state.available = state;
                g.bg_state.active = false;
            }
        }

        if to_camera {
            let f = buf!(self, data, frame);
            self.writer.write(f, frame_pitch, l.out_width, l.out_height);
            self.last_write_tick = tick;
        }
        if to_recording {
            let f = buf!(self, data, frame);
            engine
                .recorder
                .push(f, frame_pitch as i32, l.out_width, l.out_height);
        }
        if to_snapshot {
            let f = buf!(self, data, frame);
            engine.offer_snapshot(f, frame_pitch, l.out_width, l.out_height);
        }
        if to_preview {
            let b = buf!(self, data, pic_src);
            engine.update_preview(
                &b[pic_y..],
                &b[pic_uv..],
                pic_pitch,
                l.dest_width,
                l.dest_height,
            );
        }
        self.work_ms += qpc_ms() - t0;
        self.work_frames += 1;
    }

    /// `Nv12Scale` of the source's own frame into our output buffer, same size (a plain copy).
    /// Only called while the frame is still the source's (`!owned`).
    fn copy_to_output(&mut self, data: &[u8], frame_pitch: usize, w: u32, h: u32) {
        self.bufs.output.resize(nv12_bytes(w, h) as usize, 0);
        nv12_scale(
            data,
            frame_pitch,
            w,
            h,
            &mut self.bufs.output,
            w as usize,
            w,
            h,
        );
    }
}

impl Drop for Pipeline {
    fn drop(&mut self) {
        self.end();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pace_gives_even_frames_from_a_fast_source() {
        // 100 fps source paced to 30: of 100 frames at 10 ms, 30 go through (+-1), gaps 30/40 ms.
        let interval = 1000.0 / 30.0;
        let mut due = 0.0;
        let mut now = 0.0;
        let mut taken = Vec::new();
        for _ in 0..100 {
            // the same decision the pipeline makes, on a simulated clock
            if now >= due - interval / 4.0 {
                taken.push(now);
                due = if now > due + interval {
                    now + interval
                } else {
                    due + interval
                };
            }
            now += 10.0;
        }
        assert!((29..=31).contains(&taken.len()), "{}", taken.len());
        for w in taken.windows(2) {
            let gap = w[1] - w[0];
            assert!((30.0..=40.0).contains(&gap), "gap {gap}");
        }
    }

    #[test]
    fn pace_take_restarts_after_a_stall() {
        let mut due = 0.0;
        let now = qpc_ms();
        pace_take(&mut due, 33.0);
        // the QPC time is far past 0 + 33: the schedule restarts from now
        assert!(due >= now);
        assert!(due <= qpc_ms() + 33.0 + 1.0);
        let mut ahead = qpc_ms() + 1000.0;
        pace_take(&mut ahead, 33.0);
        assert!((ahead - (qpc_ms() + 1033.0)).abs() < 50.0);
    }

    #[test]
    fn pace_ready_allows_a_quarter_interval_early() {
        let now = qpc_ms();
        assert!(pace_ready(now + 5.0, 33.0)); // due in 5 ms, 8 ms early is allowed
        assert!(!pace_ready(now + 100.0, 33.0));
    }

    #[test]
    fn qpc_moves_forward() {
        let a = qpc_ms();
        std::thread::sleep(std::time::Duration::from_millis(20));
        let b = qpc_ms();
        assert!(b - a >= 15.0 && b - a < 500.0, "{}", b - a);
    }

    #[test]
    fn planes_split_at_the_uv_plane() {
        let mut buf: Vec<u8> = (0..24).collect();
        let (y, uv) = planes_mut(&mut buf, 16, 4, 18);
        assert_eq!(y[0], 4);
        assert_eq!(uv[0], 18);
        y[0] = 99;
        uv[0] = 98;
        assert_eq!((buf[4], buf[18]), (99, 98));
    }
}
