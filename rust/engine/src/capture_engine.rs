//! Runs the virtual camera on a worker thread. Port of `src/engine/CaptureEngine.h` and the
//! engine part of `CaptureEngine.cpp`:
//!
//!   source (webcam, phone, or a window on this PC) -> NV12 -> orient -> crop/zoom (or automatic
//!     framing) -> fit -> adjust -> shared memory -> virtual camera
//!
//! and keeps a small BGRA copy of the latest frame for the preview. `switch` moves to another
//! source while the virtual camera stays registered, so apps using it never lose it.
//!
//! The per-frame work lives in [`crate::pipeline`]; this file has the control surface, the
//! status, the standby logic and the three source loops (webcam, phone, window).

use std::collections::HashMap;
use std::ffi::c_void;
use std::sync::atomic::Ordering::{Relaxed, SeqCst};
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicIsize, AtomicU32, AtomicU64};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::thread::JoinHandle;
use std::time::Duration;

use niwcam_core::image_settings::{compute_layout, FrameLayout, ImageSettings, OutputSize};
use niwcam_core::json::Value;
use niwcam_core::nv12::{
    bgra_to_nv12, nv12_crop_scale_bilinear_to, nv12_fill_bars, nv12_scale, nv12_to_bgra, Nv12Rect,
};
use niwcam_media::audio_out::AudioOut;
use niwcam_media::mf_devices::{find_camera_again, CameraDevice, NativeFormat};
use niwcam_media::recorder::Recorder;
use niwcam_media::snapshot::save_png;
use niwcam_media::video_decoder::VideoDecoder;
use niwcam_phone::phone_link::{Kind, LeaseGuard, Message, PhoneLink, Recv};
use niwcam_phone::tick_ms;
use niwcam_shm::{nv12_bytes, FrameWriter, Names};
use windows::core::{GUID, HRESULT};
use windows::Win32::Foundation::{
    ERROR_NOT_READY, ERROR_NO_DATA, E_FAIL, HWND, LPARAM, S_OK, WPARAM,
};
use windows::Win32::Media::MediaFoundation::{
    IMF2DBuffer, IMFMediaBuffer, IMFMediaSource, IMFMediaType, IMFMediaTypeHandler,
    IMFSourceReader, MFCreateAttributes, MFCreateMediaType, MFCreateSourceReaderFromMediaSource,
    MFMediaType_Video, MFVideoFormat_ARGB32, MFVideoFormat_H264, MFVideoFormat_HEVC,
    MFVideoFormat_NV12, MF_LOW_LATENCY, MF_MT_FRAME_RATE, MF_MT_FRAME_SIZE, MF_MT_MAJOR_TYPE,
    MF_MT_SUBTYPE, MF_SOURCE_READERF_ENDOFSTREAM, MF_SOURCE_READERF_ERROR,
    MF_SOURCE_READER_ENABLE_ADVANCED_VIDEO_PROCESSING, MF_SOURCE_READER_FIRST_VIDEO_STREAM,
};
use windows::Win32::System::Com::{CoInitializeEx, CoUninitialize, COINIT_MULTITHREADED};
use windows::Win32::System::Threading::{
    GetCurrentThread, SetThreadPriority, THREAD_PRIORITY_ABOVE_NORMAL,
};
use windows::Win32::UI::WindowsAndMessaging::{IsWindow, PostMessageW};

use crate::fx_bridge::{load_image_fill, render_card, CaptureResult, WindowCapture};
use crate::pipeline::{directshow_mode, pace_ready, pace_take, qpc_ms, Pipeline, K_IDLE_CLOSE_MS};

/// ~30 fps is plenty for a preview, but evenly spaced: timed on the QPC clock (`GetTickCount64`
/// moves in 15.6 ms steps, and a 33 ms gate on it gave ~21 fps with 33-85 ms gaps).
const K_PREVIEW_FPS: f64 = 30.0;
const K_PREVIEW_INTERVAL_MS: f64 = 1000.0 / K_PREVIEW_FPS;

const K_VIDEO_STREAM: u32 = MF_SOURCE_READER_FIRST_VIDEO_STREAM.0 as u32;

/// Waiting: the source dropped (phone app hidden, cable pulled, camera taken by another app) but
/// the virtual camera stays up and the engine keeps retrying; `error` says what it is waiting for.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
#[repr(i32)]
pub enum State {
    #[default]
    Stopped = 0,
    Starting = 1,
    Running = 2,
    Failed = 3,
    Waiting = 4,
}

#[derive(Clone, Debug)]
pub struct Status {
    pub state: State,
    /// Virtual camera output.
    pub width: u32,
    pub height: u32,
    /// Source frame.
    pub camera_width: u32,
    pub camera_height: u32,
    /// MJPG/NV12 for webcams, H264/HEVC for phones.
    pub camera_subtype: GUID,
    pub fps: f64,
    /// Average per-frame work in our pipeline.
    pub process_ms: f64,
    pub frames_captured: u64,
    pub frames_dropped: u64,
    /// Frame Server has loaded our media source.
    pub shared_memory: bool,
    /// Some app is streaming from the camera.
    pub consumer_active: bool,
    /// Phones: rotation the phone asked for.
    pub source_rotation: i32,
    /// Virtual camera up, source closed until something needs it.
    pub standby: bool,
    /// The picture inside the output: smaller than width x height when it is fitted with black
    /// bars.
    pub picture_width: u32,
    pub picture_height: u32,
    /// Which `start`/`switch` the frame counts belong to.
    pub source_id: u32,
    /// The camera is up with no source: apps see "no signal".
    pub no_source: bool,
    /// Phones: how late the newest frame arrived in the last second.
    pub lag_ms: f64,
    /// And frames not shown because a newer one had arrived.
    pub frames_skipped: u64,
    pub error: String,
}

impl Default for Status {
    fn default() -> Self {
        Status {
            state: State::Stopped,
            width: 0,
            height: 0,
            camera_width: 0,
            camera_height: 0,
            camera_subtype: GUID::zeroed(),
            fps: 0.0,
            process_ms: 0.0,
            frames_captured: 0,
            frames_dropped: 0,
            shared_memory: false,
            consumer_active: false,
            source_rotation: 0,
            standby: false,
            picture_width: 0,
            picture_height: 0,
            source_id: 0,
            no_source: false,
            lag_ms: 0.0,
            frames_skipped: 0,
            error: String::new(),
        }
    }
}

/// "Nền": the person stays sharp; the background is blurred (strength 0..100) or replaced by an
/// image. Cameras only (never a screen or a window), and only on frames that go somewhere.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
#[repr(i32)]
pub enum BackgroundMode {
    #[default]
    Off = 0,
    Blur = 1,
    Image = 2,
}

/// What apps see while the picture is paused: the camera's own "no signal", a card with text, the
/// last picture blurred with the text, or an image file.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
#[repr(i32)]
pub enum PauseKind {
    #[default]
    NoSignal = 0,
    Card = 1,
    Blur = 2,
    Image = 3,
}

#[derive(Clone, Debug, Default)]
pub struct PhoneAudioState {
    /// The phone streams and was asked for its sound.
    pub asked: bool,
    /// Sound arrived in the last second.
    pub receiving: bool,
    /// Why not ("" when fine).
    pub problem: String,
}

#[derive(Clone, Debug)]
pub struct BackgroundState {
    /// 1 works or not tried, 0 Windows ML / the model missing, -1 gave up.
    pub available: i32,
    /// Blending frames now.
    pub active: bool,
    /// The model's last run.
    pub infer_ms: f64,
    /// Skin smoothing: faces in the last look (-1 off, -2 no face tracker).
    pub faces: i32,
}

impl Default for BackgroundState {
    fn default() -> Self {
        BackgroundState {
            available: 1,
            active: false,
            infer_ms: 0.0,
            faces: -1,
        }
    }
}

#[derive(Clone, Debug)]
pub struct Framing {
    /// Framing moves the picture now (on, a camera, frames flowing).
    pub active: bool,
    /// 1 face detection works, 0 Windows has none, -1 it kept failing.
    pub available: i32,
    /// The source is a camera.
    pub camera_source: bool,
    /// In the last detection.
    pub faces: i32,
    /// The view shown now.
    pub zoom: f64,
    pub center_x: f64,
    pub center_y: f64,
    /// How far framing may zoom into this picture.
    pub max_zoom: f64,
}

impl Default for Framing {
    fn default() -> Self {
        Framing {
            active: false,
            available: 1,
            camera_source: false,
            faces: 0,
            zoom: 1.0,
            center_x: 0.5,
            center_y: 0.5,
            max_zoom: 1.0,
        }
    }
}

/// How one attempt at running the source ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Outcome {
    /// `stop` was called.
    Quit,
    /// The source went away: retry while the virtual camera stays up.
    Lost,
    /// Retrying cannot help (missing decoder, lens refused...).
    Fatal,
    /// Nobody needed a picture for a while: source closed, back to standby.
    Idle,
    /// `switch` was called: run the next source on the same virtual camera.
    Switch,
}

#[derive(Clone, Debug, Default)]
pub(crate) struct PausePicture {
    kind: PauseKind,
    text: String,
    path: String,
    /// Bumped on every change: the picture is rendered again.
    gen: u32,
}

/// What `m_lock` guards in the C++.
pub(crate) struct Locked {
    pub(crate) status: Status,
    pub(crate) framing: Framing,
    pub(crate) image: ImageSettings,
    pub(crate) source_rotation: i32,
    pub(crate) phone_state: String,
    pub(crate) preview: Vec<u32>,
    pub(crate) preview_w: u32,
    pub(crate) preview_h: u32,
    pub(crate) preview_id: u64,

    // The source running now, and the one `switch` asked for.
    pub(crate) camera: CameraDevice,
    pub(crate) format: NativeFormat,
    pub(crate) next_camera: CameraDevice,
    pub(crate) next_format: NativeFormat,
    /// Last id handed out by `start`/`switch`.
    pub(crate) source_seq: u32,
    pub(crate) next_source_id: u32,
    pub(crate) blocking: Option<Arc<PhoneLink>>,
    pub(crate) pause: PausePicture,

    // The phone's microphone (`set_phone_audio`).
    pub(crate) audio_target: String,
    pub(crate) audio_state: PhoneAudioState,
    pub(crate) bg_image: String,
    pub(crate) bg_model: String,
    pub(crate) bg_gen: u32,
    pub(crate) bg_state: BackgroundState,

    /// Last orientation each phone source reported (camera name + lens), so the virtual camera
    /// published in standby already has the right shape and is not rebuilt (dropping the app that
    /// just opened it) when the phone starts.
    pub(crate) rotation_memory: HashMap<String, i32>,
}

impl Locked {
    /// `ExpectedRotationLocked`.
    pub(crate) fn expected_rotation(&self, cam: &CameraDevice, format: &NativeFormat) -> i32 {
        if !cam.is_phone() {
            return 0;
        }
        if let Some(r) = self
            .rotation_memory
            .get(&format!("{}|{}", cam.name, format.phone_camera))
        {
            return *r;
        }
        // First time: phones are usually held upright. Their camera sensors are mounted sideways
        // (90), and an Android screen streams landscape (270).
        if format.phone_camera == "screen" {
            270
        } else {
            90
        }
    }

    /// `EffectiveImageLocked`: user settings + phone orientation.
    pub(crate) fn effective_image(&self) -> ImageSettings {
        let mut s = self.image;
        s.orientation.rotation = (s.orientation.rotation + self.source_rotation) % 360;
        s
    }
}

#[derive(Default)]
pub(crate) struct Snap {
    pub(crate) snap: Vec<u8>,
    pub(crate) w: u32,
    pub(crate) h: u32,
}

/// Everything the capture thread and the API threads share (the members of `CaptureEngine`).
pub(crate) struct Shared {
    pub(crate) names: Names,
    pub(crate) quit: AtomicBool,
    /// `next_*` is waiting to take over.
    pub(crate) switch: AtomicBool,
    /// `OutputSize`.
    pub(crate) output_size: AtomicI32,
    /// Bumped when the picture's shape may change.
    pub(crate) layout_gen: AtomicU32,
    pub(crate) preview_enabled: AtomicBool,
    pub(crate) auto_frame: AtomicBool,
    pub(crate) preview_max_w: AtomicU32,
    pub(crate) preview_max_h: AtomicU32,
    pub(crate) lock: Mutex<Locked>,
    /// QPC time the next preview frame is due (capture thread only), as `f64` bits.
    preview_due_ms: AtomicU64,
    pub(crate) recorder: Recorder,

    // Background effect settings (mode and strength read on every frame, the paths under `lock`).
    pub(crate) bg_mode: AtomicI32,
    pub(crate) bg_strength: AtomicI32,
    /// High-quality background (ADR-008): soft matte, guided filter, halo-free blur.
    pub(crate) bg_quality: AtomicBool,
    pub(crate) smoothing: AtomicI32,

    audio_gen: AtomicU32,
    audio_wanted: AtomicBool,
    /// The source now is a phone's camera.
    phone_camera_source: AtomicBool,
    /// A snapshot asked for: the next frame that goes to the camera is copied here.
    pub(crate) snap_wanted: AtomicBool,
    snap: Mutex<Snap>,
    snap_ready: Condvar,

    notify_window: AtomicIsize,
    notify_msg: AtomicU32,
    /// Posted, `copy_preview` not called since.
    notify_pending: AtomicBool,
}

/// `std::lock_guard`: a poisoned lock (a panic elsewhere) is still usable, as in the C++.
pub(crate) fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// "No source": nothing to open (`switch` with an empty `CameraDevice`).
fn is_no_source(cam: &CameraDevice) -> bool {
    !cam.is_phone() && !cam.is_pc_window() && cam.activate.is_none()
}

fn hwnd_of(window: isize) -> HWND {
    HWND(window as *mut c_void)
}

fn is_window(window: isize) -> bool {
    // SAFETY: IsWindow accepts any handle value.
    unsafe { IsWindow(Some(hwnd_of(window))) }.as_bool()
}

fn hr_text(hr: HRESULT) -> String {
    if hr.0 != 0 {
        format!(" (0x{:08X})", hr.0 as u32)
    } else {
        String::new()
    }
}

fn pack_size(w: u32, h: u32) -> u64 {
    ((w as u64) << 32) | h as u64
}

/// How late each frame arrives against the stream's own clock (docs/protocol.md 6): arrival time
/// minus pts, less the smallest difference seen over the last 3-6 s (the clock offset, tracked in
/// two buckets so a drift or a pts jump is forgotten within 6 s). It grows when the network, or
/// this PC, falls behind; the phone sends less at 80 ms and drops at 200 ms.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Lag {
    min_now: f64,
    min_prev: f64,
    bucket: u64,
    pub(crate) current: f64,
    pub(crate) peak: f64,
}

impl Default for Lag {
    fn default() -> Self {
        Lag {
            min_now: 1e18,
            min_prev: 1e18,
            bucket: 0,
            current: 0.0,
            peak: 0.0,
        }
    }
}

impl Lag {
    pub(crate) fn update(&mut self, arrival_ms: f64, pts_ms: f64, now: u64) {
        let d = arrival_ms - pts_ms;
        if now.wrapping_sub(self.bucket) >= 3000 {
            self.min_prev = self.min_now;
            self.min_now = 1e18;
            self.bucket = now;
        }
        self.min_now = self.min_now.min(d);
        self.current = (d - self.min_now.min(self.min_prev)).max(0.0);
        self.peak = self.peak.max(self.current);
    }
}

/// A phone "error": some only need the user to bring the app back, the rest will not get better
/// by asking again.
fn phone_error_outcome(code: &str) -> Outcome {
    if code == "permission" || code == "unsupported" || code == "encoder" {
        Outcome::Fatal
    } else {
        Outcome::Lost
    }
}

/// `Json::Value::MakeObject().Set(..)...` for the control messages.
macro_rules! jobj {
    ($($k:expr => $v:expr),* $(,)?) => {{
        let mut v = Value::object();
        $( v.set($k, $v); )*
        v
    }};
}

impl Shared {
    fn new(names: Names) -> Shared {
        Shared {
            names,
            quit: AtomicBool::new(false),
            switch: AtomicBool::new(false),
            output_size: AtomicI32::new(0),
            layout_gen: AtomicU32::new(0),
            preview_enabled: AtomicBool::new(true),
            auto_frame: AtomicBool::new(false),
            preview_max_w: AtomicU32::new(1280),
            preview_max_h: AtomicU32::new(720),
            lock: Mutex::new(Locked {
                status: Status::default(),
                framing: Framing::default(),
                image: ImageSettings::default(),
                source_rotation: 0,
                phone_state: String::new(),
                preview: Vec::new(),
                preview_w: 0,
                preview_h: 0,
                preview_id: 0,
                camera: CameraDevice::default(),
                format: NativeFormat::default(),
                next_camera: CameraDevice::default(),
                next_format: NativeFormat::default(),
                source_seq: 0,
                next_source_id: 0,
                blocking: None,
                pause: PausePicture::default(),
                audio_target: String::new(),
                audio_state: PhoneAudioState::default(),
                bg_image: String::new(),
                bg_model: String::new(),
                bg_gen: 0,
                bg_state: BackgroundState::default(),
                rotation_memory: HashMap::new(),
            }),
            preview_due_ms: AtomicU64::new(0f64.to_bits()),
            recorder: Recorder::new(),
            bg_mode: AtomicI32::new(0),
            bg_strength: AtomicI32::new(60),
            bg_quality: AtomicBool::new(false),
            smoothing: AtomicI32::new(0),
            audio_gen: AtomicU32::new(0),
            audio_wanted: AtomicBool::new(false),
            phone_camera_source: AtomicBool::new(false),
            snap_wanted: AtomicBool::new(false),
            snap: Mutex::new(Snap::default()),
            snap_ready: Condvar::new(),
            notify_window: AtomicIsize::new(0),
            notify_msg: AtomicU32::new(0),
            notify_pending: AtomicBool::new(false),
        }
    }

    /// A source loop should return now.
    fn interrupted(&self) -> bool {
        self.quit.load(SeqCst) || self.switch.load(SeqCst)
    }

    /// Posts the window message that says a preview frame is ready (or the state changed). One
    /// waits at a time: the next is posted once `copy_preview` has been called.
    pub(crate) fn notify(&self) {
        let window = self.notify_window.load(SeqCst);
        if window != 0 && !self.notify_pending.swap(true, SeqCst) {
            // SAFETY: PostMessageW accepts any window handle; a dead one just fails.
            let posted = unsafe {
                PostMessageW(
                    Some(hwnd_of(window)),
                    self.notify_msg.load(SeqCst),
                    WPARAM(0),
                    LPARAM(0),
                )
            };
            if posted.is_err() {
                self.notify_pending.store(false, SeqCst); // the queue is full or the window gone: try again next time
            }
        }
    }

    fn fail(&self, what: &str, hr: HRESULT) {
        {
            let mut g = lock(&self.lock);
            g.status.state = State::Failed;
            g.status.error = format!("{what}{}", hr_text(hr));
        }
        self.notify();
    }

    fn wait(&self, what: &str, hr: HRESULT) {
        {
            let mut g = lock(&self.lock);
            g.status.state = State::Waiting;
            g.status.error = format!("{what}{}", hr_text(hr));
            g.status.fps = 0.0;
        }
        self.notify();
    }

    fn set_blocking(&self, link: Option<Arc<PhoneLink>>) {
        lock(&self.lock).blocking = link;
    }

    /// A source at the preview's rate or slower gives every frame; a faster one is paced
    /// (`pace_ready`).
    pub(crate) fn preview_wanted(&self, source_fps: f64) -> bool {
        if !self.preview_enabled.load(SeqCst) {
            return false;
        }
        source_fps <= K_PREVIEW_FPS * 1.1
            || pace_ready(
                f64::from_bits(self.preview_due_ms.load(Relaxed)),
                K_PREVIEW_INTERVAL_MS,
            )
    }

    pub(crate) fn preview_due(&self, source_fps: f64) -> bool {
        if !self.preview_wanted(source_fps) {
            return false;
        }
        let mut due = f64::from_bits(self.preview_due_ms.load(Relaxed));
        pace_take(&mut due, K_PREVIEW_INTERVAL_MS);
        self.preview_due_ms.store(due.to_bits(), Relaxed);
        true
    }

    /// Downscaled NV12 -> BGRA (BT.601 limited range) for the on-screen preview.
    pub(crate) fn update_preview(&self, y: &[u8], uv: &[u8], pitch: usize, w: u32, h: u32) {
        // Only as many pixels as the preview panel actually shows.
        let scale = 1.0f64
            .min(self.preview_max_w.load(SeqCst) as f64 / w as f64)
            .min(self.preview_max_h.load(SeqCst) as f64 / h as f64);
        let pw = 2.max((w as f64 * scale) as u32);
        let ph = 2.max((h as f64 * scale) as u32);

        thread_local! {
            static BUF: std::cell::RefCell<Vec<u32>> = const { std::cell::RefCell::new(Vec::new()) };
        }
        BUF.with_borrow_mut(|buf| {
            buf.resize(pw as usize * ph as usize, 0);
            nv12_to_bgra(y, uv, pitch, w, h, buf, pw, ph);
            {
                let mut g = lock(&self.lock);
                std::mem::swap(&mut g.preview, buf);
                g.preview_w = pw;
                g.preview_h = ph;
                g.preview_id += 1;
            }
        });
        self.notify();
    }

    pub(crate) fn offer_snapshot(&self, nv12: &[u8], pitch: usize, w: u32, h: u32) {
        let mut s = lock(&self.snap);
        if !self.snap_wanted.load(SeqCst) {
            return;
        }
        s.snap.resize(nv12_bytes(w, h) as usize, 0);
        nv12_scale(nv12, pitch, w, h, &mut s.snap, w as usize, w, h);
        s.w = w;
        s.h = h;
        self.snap_wanted.store(false, SeqCst);
        self.snap_ready.notify_all();
    }

    /// A picture is needed: an app streams from the virtual camera, or the preview is on screen.
    /// Otherwise the source stays closed (standby).
    fn source_wanted(&self, pipeline: &mut Pipeline) -> bool {
        // A recording or a snapshot needs the picture too (window hidden, no app on the camera),
        // and a call with the camera off still needs the phone's microphone.
        self.preview_enabled.load(SeqCst)
            || pipeline.consumer_active()
            || self.recorder.active()
            || self.snap_wanted.load(SeqCst)
            || (self.audio_wanted.load(SeqCst) && self.phone_camera_source.load(SeqCst))
    }

    /// In a source loop: false once nothing has wanted a picture for `K_IDLE_CLOSE_MS`.
    fn still_wanted(&self, pipeline: &mut Pipeline, idle_since: &mut u64) -> bool {
        if self.source_wanted(pipeline) {
            *idle_since = 0;
            return true;
        }
        let now = tick_ms();
        if *idle_since == 0 {
            *idle_since = now;
        }
        now.wrapping_sub(*idle_since) < K_IDLE_CLOSE_MS
    }

    fn set_standby(&self, standby: bool) {
        {
            let mut g = lock(&self.lock);
            if g.status.standby == standby {
                return;
            }
            g.status.standby = standby;
            if standby {
                g.status.fps = 0.0;
                g.status.consumer_active = false;
                g.framing.active = false; // nothing is framed while the source is closed
            }
        }
        self.notify();
    }

    // ---- Sources ---------------------------------------------------------------------------

    fn run(self: &Arc<Self>, cam: CameraDevice, format: NativeFormat) {
        // SAFETY: plain COM initialisation of this thread, paired with CoUninitialize below.
        if unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) }.is_err() {
            self.fail("Không khởi tạo được COM", E_FAIL);
            return;
        }
        // Frames must not wait behind the UI or other apps on a busy PC.
        // SAFETY: the pseudo handle of this thread.
        unsafe {
            let _ = SetThreadPriority(GetCurrentThread(), THREAD_PRIORITY_ABOVE_NORMAL);
        }
        {
            let mut pipeline = Pipeline::new(Arc::clone(self));
            let mut cam = cam;
            let mut format = format;
            let mut webcam = cam.clone(); // found again after the webcam was unplugged
            self.phone_camera_source
                .store(cam.is_phone() && format.phone_camera != "screen", SeqCst);
            let mut lost = false;
            let mut begun = false; // the pipeline knows this source's size
            let mut stuck = false; // this source cannot run (Fatal while the camera is up): wait for another
            while !self.quit.load(SeqCst) {
                if self.switch.load(SeqCst) {
                    // Another source on the same virtual camera. Apps using it keep the last
                    // picture, frozen, until the new source's first frame.
                    pipeline.hold();
                    let mut g = lock(&self.lock);
                    self.switch.store(false, SeqCst);
                    cam = g.next_camera.clone();
                    g.camera = cam.clone();
                    format = g.next_format.clone();
                    g.format = format.clone();
                    g.source_rotation = g.expected_rotation(&cam, &format);
                    self.layout_gen.fetch_add(1, SeqCst);
                    g.phone_state.clear();
                    g.status.source_id = g.next_source_id;
                    g.status.frames_captured = 0;
                    g.status.frames_dropped = 0;
                    g.status.frames_skipped = 0;
                    g.status.lag_ms = 0.0;
                    g.status.fps = 0.0;
                    g.status.error.clear();
                    g.status.no_source = is_no_source(&cam);
                    if g.status.state == State::Waiting {
                        g.status.state = if pipeline.live() {
                            State::Running
                        } else {
                            State::Starting
                        };
                    }
                    webcam = cam.clone();
                    self.phone_camera_source
                        .store(cam.is_phone() && format.phone_camera != "screen", SeqCst);
                    lost = false;
                    begun = false;
                    stuck = false;
                }
                if stuck {
                    for _ in 0..2 {
                        if self.interrupted() {
                            break;
                        }
                        std::thread::sleep(Duration::from_millis(100));
                    }
                    continue;
                }

                if is_no_source(&cam) {
                    if !self.run_no_source(&mut pipeline, &format) {
                        break;
                    }
                    continue;
                }

                // Standby: nothing needs a picture (no app on the camera, preview hidden). The
                // virtual camera is published so apps can find and open it, but the source stays
                // closed: no webcam light, no phone encoding or Wi-Fi traffic, no decoding.
                // Opening it wakes us.
                if !self.source_wanted(&mut pipeline) {
                    if !pipeline.live() || !begun {
                        // In this source's shape (a phone at its last orientation): nothing to
                        // rebuild when it starts.
                        let hr = pipeline.begin(
                            format.width,
                            format.height,
                            format.fps_num,
                            format.fps_den,
                            &format.subtype,
                        );
                        if hr.is_err() {
                            self.fail(pipeline.failure(), hr);
                            break;
                        }
                        begun = true;
                    }
                    {
                        let mut g = lock(&self.lock);
                        if g.status.state == State::Waiting {
                            g.status.state = State::Running; // nothing to wait for now
                        }
                        g.status.error.clear();
                    }
                    self.set_standby(true);
                    for _ in 0..2 {
                        if self.interrupted() || self.source_wanted(&mut pipeline) {
                            break;
                        }
                        std::thread::sleep(Duration::from_millis(100));
                    }
                    pipeline.tick(); // a new shape or size setting applies in standby too
                    if pipeline.broken() {
                        self.fail(pipeline.failure(), E_FAIL);
                        break;
                    }
                    continue;
                }
                self.set_standby(false);

                let mut why = String::new();
                let mut hr = S_OK;
                let begins = pipeline.begins();
                pipeline.set_framing_source(
                    !cam.is_pc_window() && !(cam.is_phone() && format.phone_camera == "screen"),
                );
                let outcome = if cam.is_phone() {
                    self.run_phone(&mut pipeline, &cam, &format, &mut why, &mut hr)
                } else if cam.is_pc_window() {
                    self.run_window(&mut pipeline, &cam, &mut why, &mut hr)
                } else {
                    let mut found = true;
                    if lost {
                        match find_camera_again(&cam) {
                            Some(c) => webcam = c,
                            None => found = false,
                        }
                    }
                    if !found {
                        why = "Camera đã bị rút ra. Cắm lại cáp để tiếp tục".to_string();
                        Outcome::Lost
                    } else {
                        self.run_webcam(&mut pipeline, &webcam, &format, &mut why, &mut hr)
                    }
                };
                // The pipeline knows this source's size only if the run got to Begin (otherwise
                // standby would mix the previous source's size with this one's rotation).
                if pipeline.begins() != begins {
                    begun = true;
                }
                if outcome == Outcome::Switch {
                    continue;
                }
                lost = outcome == Outcome::Lost;
                if outcome == Outcome::Quit || self.quit.load(SeqCst) {
                    break;
                }
                if outcome == Outcome::Idle {
                    continue; // back to standby
                }
                if pipeline.broken() {
                    self.fail(pipeline.failure(), hr);
                    break;
                }
                // The camera lives as long as the app, so a source that fails before it was ever
                // published publishes it anyway (apps see "no signal") and is waited for like a
                // source lost later. Only the camera itself failing ends the session.
                if !pipeline.live() {
                    let bhr = pipeline.begin(
                        format.width,
                        format.height,
                        format.fps_num,
                        format.fps_den,
                        &format.subtype,
                    );
                    if bhr.is_err() {
                        self.fail(pipeline.failure(), bhr);
                        break;
                    }
                }
                pipeline.release_hold();
                self.wait(&why, hr);
                if outcome == Outcome::Fatal {
                    // A source picked while the camera is up cannot run (lens refused, no HEVC
                    // decoder...): keep the camera (apps keep it selected) until another one is
                    // picked.
                    stuck = true;
                    continue;
                }
                // Keep the virtual camera up (apps that use it keep it selected and show its "no
                // signal" picture) and bring the source back by itself.
                for _ in 0..20 {
                    if self.interrupted() {
                        break;
                    }
                    std::thread::sleep(Duration::from_millis(100));
                }
            }
            pipeline.end();
        }
        // SAFETY: pairs the successful CoInitializeEx above.
        unsafe { CoUninitialize() };
        self.notify();
    }

    /// No source (none found yet, or the picture is turned off): the camera stays registered, at
    /// the size it has, and apps that use it see "no signal", or the pause picture the app set.
    /// Only whether an app streams from it is watched (the app looks for a source then).
    /// Returns false when the engine must end (the camera could not be published).
    fn run_no_source(&self, pipeline: &mut Pipeline, format: &NativeFormat) -> bool {
        // The last picture, before anything else is written: a blurred pause picture.
        let mut last: Vec<u8> = Vec::new();
        let (mut last_w, mut last_h) = (0u32, 0u32);
        match pipeline.copy_latest(&mut last) {
            Some((w, h)) => {
                last_w = w;
                last_h = h;
            }
            None => last.clear(),
        }
        if !pipeline.live() {
            let hr = pipeline.begin(
                format.width,
                format.height,
                format.fps_num,
                format.fps_den,
                &MFVideoFormat_NV12,
            );
            if hr.is_err() {
                self.fail(pipeline.failure(), hr);
                return false;
            }
        }
        pipeline.release_hold();
        self.set_standby(false);
        {
            let mut g = lock(&self.lock);
            g.status.no_source = true;
            g.status.state = State::Running;
            g.status.error.clear();
            g.status.fps = 0.0;
        }
        self.notify();
        let mut still: Vec<u8> = Vec::new(); // the pause picture, NV12 at the camera's size
        let (mut still_w, mut still_h, mut still_gen) = (0u32, 0u32, u32::MAX);
        let mut still_written = 0u64;
        while !self.interrupted() {
            pipeline.tick(); // a new size setting applies with no source too
            let consumer = pipeline.consumer_active();
            let (changed, pause) = {
                let mut g = lock(&self.lock);
                let changed = g.status.consumer_active != consumer;
                g.status.consumer_active = consumer;
                (changed, g.pause.clone())
            };
            if changed {
                self.notify();
            }
            if pause.kind != PauseKind::NoSignal {
                // Rendered once per setting and size; written twice a second while an app
                // streams, else often enough to stay fresh (readers allow 2 s).
                let w = pipeline.out_width();
                let h = pipeline.out_height();
                if w != 0 && h != 0 && (pause.gen != still_gen || w != still_w || h != still_h) {
                    still_gen = pause.gen;
                    still_w = w;
                    still_h = h;
                    let mut ok = false;
                    if pause.kind == PauseKind::Image {
                        ok = load_image_fill(&pause.path, w, h, &mut still);
                    }
                    if !ok {
                        let background = if pause.kind == PauseKind::Blur && !last.is_empty() {
                            Some(&last[..])
                        } else {
                            None
                        };
                        ok = render_card(&pause.text, w, h, background, last_w, last_h, &mut still);
                    }
                    if !ok {
                        still.clear();
                    }
                    still_written = 0;
                }
                let now = tick_ms();
                let every = if consumer || self.recorder.active() {
                    500
                } else {
                    1500
                };
                if !still.is_empty() && now.wrapping_sub(still_written) >= every {
                    pipeline.write_still(&still, still_w, still_h);
                    // Paused while recording.
                    self.recorder.push(&still, still_w as i32, still_w, still_h);
                    still_written = now;
                }
                if self.snap_wanted.load(SeqCst) && !still.is_empty() {
                    self.offer_snapshot(&still, still_w as usize, still_w, still_h);
                }
            } else if !still.is_empty() {
                still.clear(); // back to "no signal" in 2 s
                still_gen = u32::MAX;
            }
            std::thread::sleep(Duration::from_millis(200));
        }
        true
    }

    fn run_webcam(
        &self,
        pipeline: &mut Pipeline,
        cam: &CameraDevice,
        format: &NativeFormat,
        why: &mut String,
        hr: &mut HRESULT,
    ) -> Outcome {
        let mut source: Option<IMFMediaSource> = None;
        let mut reader: Option<IMFSourceReader> = None;
        let outcome = self.webcam_body(pipeline, cam, format, why, hr, &mut source, &mut reader);
        // cleanup:
        drop(reader.take());
        if let Some(s) = source.take() {
            // SAFETY: plain call on the source this function opened.
            unsafe {
                let _ = s.Shutdown();
            }
        }
        if let Some(a) = &cam.activate {
            // SAFETY: plain call on the camera's activation object.
            unsafe {
                let _ = a.ShutdownObject();
            }
        }
        if self.quit.load(SeqCst) {
            Outcome::Quit
        } else if self.switch.load(SeqCst) {
            Outcome::Switch
        } else {
            outcome
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn webcam_body(
        &self,
        pipeline: &mut Pipeline,
        cam: &CameraDevice,
        format: &NativeFormat,
        why: &mut String,
        hr: &mut HRESULT,
        source_out: &mut Option<IMFMediaSource>,
        reader_out: &mut Option<IMFSourceReader>,
    ) -> Outcome {
        let mut idle_since = 0u64;
        macro_rules! step {
            ($e:expr, $what:expr) => {
                match $e {
                    Ok(v) => v,
                    Err(e) => {
                        *hr = e.code();
                        *why = $what.to_string();
                        return Outcome::Lost;
                    }
                }
            };
        }

        // 1. Lock the camera to the chosen native format before the source reader exists, so the
        //    reader cannot renegotiate a smaller one.
        let Some(activate) = cam.activate.as_ref() else {
            *hr = E_FAIL;
            *why = "Không mở được camera (app khác đang dùng độc quyền?)".to_string();
            return Outcome::Lost;
        };
        // SAFETY: plain COM calls on the activation object, the source and its handlers.
        let source: IMFMediaSource = step!(
            unsafe { activate.ActivateObject::<IMFMediaSource>() },
            "Không mở được camera (app khác đang dùng độc quyền?)"
        );
        *source_out = Some(source.clone());
        {
            let mut formats = Vec::new();
            let mut handler: Option<IMFMediaTypeHandler> = None;
            let r = niwcam_media::mf_devices::get_native_formats(
                &source,
                &mut formats,
                Some(&mut handler),
            );
            if r.is_err() {
                *hr = r;
                *why = "Không đọc được định dạng của camera".to_string();
                return Outcome::Lost;
            }
            let Some(handler) = handler else {
                *hr = E_FAIL;
                *why = "Không đọc được định dạng của camera".to_string();
                return Outcome::Lost;
            };
            // SAFETY: plain COM calls on the handler.
            let native_type: IMFMediaType = step!(
                unsafe { handler.GetMediaTypeByIndex(format.index) },
                "Định dạng không còn tồn tại"
            );
            // SAFETY: as above.
            step!(
                unsafe { handler.SetCurrentMediaType(&native_type) },
                "Camera từ chối định dạng đã chọn"
            );
        }
        pipeline.hold();

        // 2. Decode/convert to NV12 at the same size (e.g. MJPG -> NV12).
        let (width, height);
        {
            let mut attrs = None;
            // SAFETY: the out pointer is a live local.
            step!(
                unsafe { MFCreateAttributes(&mut attrs, 2) },
                "MFCreateAttributes"
            );
            let Some(attrs) = attrs else {
                *hr = E_FAIL;
                *why = "MFCreateAttributes".to_string();
                return Outcome::Lost;
            };
            // Only insert the decoder/converter when the camera is not NV12 already; for NV12
            // cameras the frames pass straight through.
            // SAFETY: plain attribute stores on a live interface.
            unsafe {
                if format.subtype != MFVideoFormat_NV12 {
                    let _ = attrs.SetUINT32(&MF_SOURCE_READER_ENABLE_ADVANCED_VIDEO_PROCESSING, 1);
                }
                let _ = attrs.SetUINT32(&MF_LOW_LATENCY, 1);
            }
            // SAFETY: the source and attributes are live.
            let reader: IMFSourceReader = step!(
                unsafe { MFCreateSourceReaderFromMediaSource(&source, &attrs) },
                "Không tạo được source reader"
            );
            *reader_out = Some(reader.clone());

            // SAFETY: a new media type, filled with plain attribute stores.
            let nv12: IMFMediaType = step!(unsafe { MFCreateMediaType() }, "MFCreateMediaType");
            // SAFETY: as above.
            unsafe {
                let _ = nv12.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video);
                let _ = nv12.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_NV12);
                let _ = nv12.SetUINT64(&MF_MT_FRAME_SIZE, pack_size(format.width, format.height));
                let _ =
                    nv12.SetUINT64(&MF_MT_FRAME_RATE, pack_size(format.fps_num, format.fps_den));
            }
            // SAFETY: the reader and type are live.
            step!(
                unsafe { reader.SetCurrentMediaType(K_VIDEO_STREAM, None, &nv12) },
                "Không chuyển được sang NV12"
            );
            // SAFETY: as above.
            let out_type: IMFMediaType = step!(
                unsafe { reader.GetCurrentMediaType(K_VIDEO_STREAM) },
                "GetCurrentMediaType"
            );
            // MFGetAttributeSize: untouched when the attribute is missing.
            let (mut w, mut h) = (0u32, 0u32);
            // SAFETY: plain read.
            if let Ok(p) = unsafe { out_type.GetUINT64(&MF_MT_FRAME_SIZE) } {
                w = (p >> 32) as u32;
                h = p as u32;
            }
            width = w;
            height = h;
        }

        // 3. Virtual camera.
        *hr = pipeline.begin(
            width,
            height,
            format.fps_num,
            format.fps_den,
            &format.subtype,
        );
        if hr.is_err() {
            *why = pipeline.failure().to_string();
            return Outcome::Fatal;
        }

        // 4. Capture loop.
        let Some(reader) = reader_out.clone() else {
            return Outcome::Lost;
        };
        while !self.interrupted() {
            // Nobody needs the picture any more: close the webcam (its light goes off).
            if !self.still_wanted(pipeline, &mut idle_since) {
                return Outcome::Idle;
            }
            let (mut stream_index, mut flags, mut ts) = (0u32, 0u32, 0i64);
            let mut sample = None;
            // SAFETY: the out pointers are live locals.
            step!(
                unsafe {
                    reader.ReadSample(
                        K_VIDEO_STREAM,
                        0,
                        Some(&mut stream_index),
                        Some(&mut flags),
                        Some(&mut ts),
                        Some(&mut sample),
                    )
                },
                "Mất tín hiệu camera (bị rút ra, hoặc app khác đang dùng)"
            );
            if flags & (MF_SOURCE_READERF_ENDOFSTREAM.0 | MF_SOURCE_READERF_ERROR.0) as u32 != 0 {
                *why = "Camera đã ngắt kết nối".to_string();
                *hr = HRESULT(0xC00D3E84u32 as i32); // MF_E_END_OF_STREAM
                return Outcome::Lost;
            }
            let Some(sample) = sample else { continue };

            // SAFETY: plain COM calls; the locked pointer is only read while locked, as a slice
            // of the NV12 frame (`pitch` bytes a row, the UV plane right after `height` rows).
            unsafe {
                let Ok(buffer) = sample.GetBufferByIndex(0) else {
                    continue;
                };
                let buffer: IMFMediaBuffer = buffer;
                let mut locked = false;
                if let Ok(b2d) = windows::core::Interface::cast::<IMF2DBuffer>(&buffer) {
                    let mut data: *mut u8 = std::ptr::null_mut();
                    let mut pitch = 0i32;
                    if b2d.Lock2D(&mut data, &mut pitch).is_ok() {
                        locked = true;
                        if pitch > 0 && !data.is_null() {
                            let len = pitch as usize * height as usize * 3 / 2;
                            pipeline.frame(
                                std::slice::from_raw_parts(data, len),
                                pitch as usize,
                                ts,
                            );
                        }
                        let _ = b2d.Unlock2D();
                    }
                }
                if !locked {
                    let mut data: *mut u8 = std::ptr::null_mut();
                    let mut len = 0u32;
                    if buffer.Lock(&mut data, None, Some(&mut len)).is_ok() {
                        if len as u64 >= nv12_bytes(width, height) && !data.is_null() {
                            pipeline.frame(
                                std::slice::from_raw_parts(data, len as usize),
                                width as usize,
                                ts,
                            );
                        }
                        let _ = buffer.Unlock();
                    }
                }
            }
            if pipeline.broken() {
                *why = pipeline.failure().to_string();
                return Outcome::Fatal;
            }
        }
        Outcome::Quit
    }

    fn run_phone(
        &self,
        pipeline: &mut Pipeline,
        cam: &CameraDevice,
        format: &NativeFormat,
        why: &mut String,
        hr: &mut HRESULT,
    ) -> Outcome {
        let Some(link_arc) = cam
            .phone
            .as_ref()
            .and_then(|p| Arc::clone(p).downcast::<PhoneLink>().ok())
        else {
            *why = "Nguồn điện thoại không hợp lệ".to_string();
            return Outcome::Fatal;
        };
        let link: &PhoneLink = &link_arc;
        let hevc = format.subtype == MFVideoFormat_HEVC;
        let fps = format.fps_num.checked_div(format.fps_den).unwrap_or(30);
        // ~0.1 bit per pixel per frame: 1080p30 ~= 6 Mbps, 4K30 ~= 25 Mbps.
        let kbps = ((format.width as u64 * format.height as u64 * fps as u64 / 10000) as u32)
            .clamp(2000, 60000);
        macro_rules! interrupted {
            () => {
                if self.quit.load(SeqCst) {
                    Outcome::Quit
                } else {
                    Outcome::Switch
                }
            };
        }
        // The connection is gone: the next attempt reconnects.
        macro_rules! lost {
            ($what:expr) => {{
                link.close();
                *why = $what.to_string();
                Outcome::Lost
            }};
        }

        // The link is ours while this source runs: the connector (which keeps idle links alive)
        // and screen windows leave it alone. A connector attempt in progress gives way at once.
        let mut told = false;
        let mut waited = 0;
        while !link.lease(200) {
            if self.interrupted() {
                return interrupted!();
            }
            if waited < 15 {
                pipeline.hold();
            } else if !told && link.in_use() && pipeline.live() {
                // Someone else keeps it (the phone's screen open in its own window, or
                // pairing): say so.
                told = true;
                pipeline.release_hold();
                self.wait(
                    "Nguồn này đang được dùng ở chỗ khác (cửa sổ màn hình riêng, hoặc đang ghép đôi)",
                    S_OK,
                );
            }
            waited += 1;
        }
        let _lease = LeaseGuard::new(link);

        // After a lost connection (phone app closed and reopened, Wi-Fi dropped): reconnect
        // quietly. Only an already-paired phone gets back in.
        macro_rules! reconnect {
            () => {{
                pipeline.hold();
                self.set_blocking(Some(Arc::clone(&link_arc))); // switch/stop cut it short
                let mut error = String::new();
                let ok = !self.interrupted()
                    && match link.reconnect() {
                        Ok(()) => true,
                        Err(e) => {
                            error = e;
                            false
                        }
                    };
                self.set_blocking(None);
                if !ok {
                    *why = if error.is_empty() {
                        "Mất kết nối với điện thoại".to_string()
                    } else {
                        error
                    };
                }
                ok && !self.interrupted() // a switch while it connected: the caller returns
            }};
        }
        // The phone's microphone (AudioOut.h): asked for in "start", turned on and off with "set".
        let phone_camera = format.phone_camera != "screen";
        let mut audio_gen = self.audio_gen.load(SeqCst);
        let mut audio_target = {
            let mut g = lock(&self.lock);
            let t = if phone_camera {
                g.audio_target.clone()
            } else {
                String::new()
            };
            g.audio_state = PhoneAudioState::default();
            g.audio_state.asked = !t.is_empty();
            t
        };
        let mut audio: Option<AudioOut> = if audio_target.is_empty() {
            None
        } else {
            Some(AudioOut::new(&audio_target))
        };
        let (mut audio_rate, mut audio_channels) = (48000u32, 1u32);
        let mut audio_problem = String::new();
        let mut audio_reported = 0u64;
        struct AudioReset<'a>(&'a Shared);
        impl Drop for AudioReset<'_> {
            fn drop(&mut self) {
                lock(&self.0.lock).audio_state = PhoneAudioState::default();
            }
        }
        let _audio_reset = AudioReset(self);
        macro_rules! send_start {
            () => {{
                link.send_control(&jobj! {
                    "type" => "start", "lens" => format.phone_camera.as_str(),
                    "width" => format.width, "height" => format.height, "fps" => fps,
                    "codec" => if hevc { "hevc" } else { "h264" }, "bitrateKbps" => kbps,
                    "audio" => audio.is_some(),
                })
            }};
        }
        // "started" / "state" say whether the phone's microphone runs, at what rate, or why not.
        macro_rules! audio_from {
            ($v:expr) => {{
                let a = &$v["audio"];
                if a.is_object() {
                    if a["on"].bool_or(false) {
                        audio_rate = a["rate"].int(48000).clamp(8000, 96000) as u32;
                        audio_channels = a["channels"].int(1).clamp(1, 2) as u32;
                        audio_problem.clear();
                    } else if audio.is_some() {
                        audio_problem = a["message"].str().to_string();
                        if audio_problem.is_empty() {
                            audio_problem = "Điện thoại không gửi tiếng".to_string();
                        }
                    }
                }
            }};
        }

        // An idle link kept by the connector may have died without anyone noticing (the phone
        // left the Wi-Fi): if it says nothing at all after "start", one fresh connection.
        let mut reused = link.connected();
        if !reused && !reconnect!() {
            return if self.interrupted() {
                interrupted!()
            } else {
                Outcome::Lost
            };
        }
        if !send_start!() {
            if !reused {
                return lost!("Mất kết nối với điện thoại");
            }
            reused = false;
            link.close();
            if !reconnect!() {
                return if self.interrupted() {
                    interrupted!()
                } else {
                    Outcome::Lost
                };
            }
            if !send_start!() {
                return lost!("Mất kết nối với điện thoại");
            }
        }

        let state_from = |v: &Value| {
            let mut g = lock(&self.lock);
            if !v["orientation"].is_null() {
                let rotation = ((v["orientation"].int(0) % 360) + 360) % 360;
                if rotation != g.source_rotation {
                    self.layout_gen.fetch_add(1, SeqCst); // the phone was turned: new shape
                }
                g.source_rotation = rotation;
                let r = g.source_rotation;
                g.rotation_memory
                    .insert(format!("{}|{}", cam.name, format.phone_camera), r);
            }
            if v["type"].str() == "state" {
                g.phone_state = v.dump();
            }
        };
        macro_rules! phone_error {
            ($v:expr) => {{
                *why = $v["message"].str_or("Điện thoại báo lỗi");
                phone_error_outcome($v["code"].str())
            }};
        }

        // Wait for "started" (or an error) before decoding anything.
        let mut started = Value::Null;
        let mut sent_at = tick_ms();
        let mut deadline = sent_at + 10000;
        let mut heard = false;
        let mut m = Message::default();
        while !self.interrupted() {
            pipeline.hold();
            match link.receive(&mut m, 500) {
                Recv::Message => {}
                r => {
                    let timed_out = r == Recv::TimedOut;
                    let now = tick_ms();
                    if reused && !heard && (!timed_out || now - sent_at >= 4000) {
                        // Silent or closed: the kept link was dead. Start over once on a new one.
                        reused = false;
                        link.close();
                        if !reconnect!() {
                            return if self.interrupted() {
                                interrupted!()
                            } else {
                                Outcome::Lost
                            };
                        }
                        if !send_start!() {
                            return lost!("Mất kết nối với điện thoại");
                        }
                        sent_at = tick_ms();
                        deadline = sent_at + 10000;
                        continue;
                    }
                    if timed_out && now < deadline {
                        continue;
                    }
                    return lost!(if timed_out {
                        "Điện thoại không bắt đầu phát (app trên điện thoại đang bị ẩn?)"
                    } else {
                        "Mất kết nối với điện thoại"
                    });
                }
            }
            heard = true;
            if m.kind != Kind::Control {
                continue;
            }
            let Ok(v) = m.json() else { continue };
            if v["type"].str() == "error" {
                return phone_error!(v);
            }
            if v["type"].str() == "started" {
                state_from(&v);
                started = v;
                break;
            }
        }
        if self.interrupted() {
            // Switched away before the phone answered: its "started" (and "stopped") for this
            // start would still come and be taken for the next start's. A fresh connection next
            // time.
            link.send_control(&jobj! { "type" => "stop" });
            link.close();
            return interrupted!();
        }

        let mut width = started["width"].int(format.width as i32) as u32;
        let mut height = started["height"].int(format.height as i32) as u32;
        let mut decoder = VideoDecoder::new();
        *hr = decoder.init(
            if started["codec"].str() == "h264" {
                false
            } else {
                hevc
            },
            width,
            height,
        );
        if hr.is_err() {
            *why = if hevc {
                "Không có bộ giải mã HEVC (cài \"HEVC Video Extensions\" từ Microsoft Store)"
            } else {
                "Không tạo được bộ giải mã video"
            }
            .to_string();
            link.send_control(&jobj! { "type" => "stop" });
            return Outcome::Fatal;
        }
        let codec = if hevc {
            MFVideoFormat_HEVC
        } else {
            MFVideoFormat_H264
        };
        *hr = pipeline.begin(width, height, fps, 1, &codec);
        if hr.is_err() {
            *why = pipeline.failure().to_string();
            link.send_control(&jobj! { "type" => "stop" });
            return Outcome::Fatal;
        }
        *hr = S_OK;

        let mut config: Vec<u8> = Vec::new(); // latest SPS/PPS(/VPS), prepended to keyframes
        let mut wait_keyframe = true; // decoding starts at a keyframe
        let mut lost_frames = 0u32;
        let mut decode_ms = 0f64;
        let mut decode_max = 0f64;
        let trace = std::env::var_os("NIWCAM_TRACE").is_some();
        let mut decoded = 0u32;
        let mut lag = Lag::default();
        let frame_ms = 1000.0 / fps.max(1) as f64;
        let mut skipped = 0u32; // frames decoded but not shown: a newer one had already arrived
        let mut last_frame = tick_ms();
        let mut last_stats = last_frame;
        let mut unit: Vec<u8> = Vec::new();
        let mut outcome = Outcome::Quit;
        let mut idle_since = 0u64;

        while !self.interrupted() {
            // Nobody needs the picture any more: the phone stops encoding and sending.
            if !self.still_wanted(pipeline, &mut idle_since) {
                outcome = Outcome::Idle;
                break;
            }
            // The microphone setting changed: tell the phone, open or close the output.
            if self.audio_gen.load(SeqCst) != audio_gen && phone_camera {
                audio_gen = self.audio_gen.load(SeqCst);
                let target = lock(&self.lock).audio_target.clone();
                if target != audio_target {
                    let was = audio.is_some();
                    audio_target = target;
                    audio = if audio_target.is_empty() {
                        None
                    } else {
                        Some(AudioOut::new(&audio_target))
                    };
                    audio_problem.clear();
                    if was != audio.is_some() {
                        link.send_control(&jobj! { "type" => "set", "audio" => audio.is_some() });
                    }
                }
            }
            let got = link.receive(&mut m, 500);
            let now = tick_ms();
            if now.wrapping_sub(audio_reported) >= 500 {
                audio_reported = now;
                let mut g = lock(&self.lock);
                g.audio_state.asked = audio.is_some();
                g.audio_state.receiving = audio.as_ref().is_some_and(|a| a.receiving());
                g.audio_state.problem = match &audio {
                    None => String::new(),
                    Some(a) => {
                        if audio_problem.is_empty() {
                            a.problem()
                        } else {
                            audio_problem.clone()
                        }
                    }
                };
            }
            if got == Recv::Failed {
                outcome = lost!("Mất kết nối với điện thoại");
                break;
            }
            if now.wrapping_sub(last_frame) > 6000 {
                // The app was hidden or the phone locked: the connection may be half-dead too, so
                // start over with a fresh one.
                link.send_control(&jobj! { "type" => "stop" });
                outcome = lost!(
                    "Điện thoại ngừng gửi hình (app trên điện thoại bị ẩn hoặc màn hình tắt?)"
                );
                break;
            }
            // Until the first frame of a new source decodes, the camera keeps the previous
            // picture.
            if wait_keyframe {
                pipeline.hold();
            }

            if got == Recv::Message && m.kind == Kind::Control {
                let Ok(v) = m.json() else { continue };
                let ty = v["type"].str().to_string();
                if ty == "error" {
                    outcome = phone_error!(v);
                    break;
                }
                if ty == "stopped" {
                    *why = "Điện thoại đã dừng phát".to_string();
                    outcome = Outcome::Lost;
                    break;
                }
                if ty == "caps" {
                    link.set_caps(&v.dump());
                }
                // A turn of the phone changes the picture's shape: the pipeline applies the size
                // rule on the next frame (fit while an app uses the camera, else a new size).
                if ty == "state" || ty == "started" {
                    state_from(&v);
                    audio_from!(v);
                }
            } else if got == Recv::Message && m.kind == Kind::Audio {
                if let Some(a) = &audio {
                    a.push(&m.payload, audio_rate, audio_channels);
                }
            } else if got == Recv::Message && m.kind == Kind::VideoConfig {
                config.clone_from(&m.payload);
            } else if got == Recv::Message && m.kind == Kind::VideoFrame {
                last_frame = now;
                if wait_keyframe && !m.keyframe() {
                    continue;
                }
                let data: &[u8] = if m.keyframe() && !config.is_empty() {
                    unit.clear();
                    unit.extend_from_slice(&config);
                    unit.extend_from_slice(&m.payload);
                    &unit
                } else {
                    &m.payload
                };
                let t0 = qpc_ms();
                lag.update(t0, m.pts_us as f64 / 1000.0, now);
                // Behind by more than a frame, and the next one is already here: decode this one
                // (the next needs it) but do not spend the colour work, effects and preview on a
                // picture the app would see for less than a frame. A weak PC catches up instead
                // of lagging.
                let behind = lag.current > frame_ms && link.pending_bytes() > 0;
                let pts100 = m.pts_us * 10;
                let dhr = decoder.decode(
                    data,
                    pts100,
                    &mut |nv12: &[u8], pitch: i32, w: u32, h: u32| {
                        if w != width || h != height {
                            // The phone changed the stream's size: the pipeline must not read past
                            // the frame. Same camera rule as any new size (fit during a call).
                            width = w;
                            height = h;
                            if pipeline.begin(width, height, fps, 1, &codec).is_err() {
                                return;
                            }
                        }
                        if behind && !wait_keyframe {
                            pipeline.count(pts100);
                            skipped += 1;
                        } else {
                            pipeline.frame(nv12, pitch as usize, pts100);
                        }
                    },
                );
                let spent = qpc_ms() - t0;
                decode_ms += spent;
                decode_max = decode_max.max(spent);
                decoded += 1;
                if dhr.is_err() {
                    // Corrupt or missing data: ask for a fresh keyframe.
                    lost_frames += 1;
                    wait_keyframe = true;
                    link.send_control(&jobj! { "type" => "keyframe" });
                } else {
                    wait_keyframe = false;
                }
                if pipeline.broken() {
                    *why = pipeline.failure().to_string();
                    outcome = Outcome::Fatal;
                    break;
                }
            }

            if now.wrapping_sub(last_stats) >= 1000 {
                if trace {
                    // NIWCAM_TRACE=1: one line a second on stderr, to find where a stream loses time.
                    eprintln!(
                        "[trace] {}x{} frames {} decode+pipeline avg {:.1} ms max {:.1} ms, lag peak {:.0} ms, skipped {}, lost {}, pending {} B, pipeline fps {:.1}",
                        width,
                        height,
                        decoded,
                        if decoded != 0 { decode_ms / decoded as f64 } else { 0.0 },
                        decode_max,
                        lag.peak,
                        skipped,
                        lost_frames,
                        link.pending_bytes(),
                        pipeline.last_fps()
                    );
                }
                decode_max = 0.0;
                link.send_control(&jobj! {
                    "type" => "stats", "recvFps" => pipeline.last_fps(),
                    "decodeMs" => if decoded != 0 { decode_ms / decoded as f64 } else { 0.0 },
                    "queueMs" => lag.peak.round(), "lost" => lost_frames as i32,
                });
                {
                    let mut g = lock(&self.lock);
                    g.status.lag_ms = lag.peak;
                    g.status.frames_skipped += skipped as u64;
                }
                lag.peak = 0.0;
                skipped = 0;
                lost_frames = 0;
                decode_ms = 0.0;
                decoded = 0;
                last_stats = now;
            }
        }

        if link.connected() {
            link.send_control(&jobj! { "type" => "stop" });
        }
        if self.quit.load(SeqCst) {
            Outcome::Quit
        } else if self.switch.load(SeqCst) {
            Outcome::Switch
        } else {
            outcome
        }
    }

    /// A window on this PC. Its size changes while it is being resized: frames of another size
    /// are fitted (with bars) into the size the source started with, and once the window has kept
    /// a new size for a second the pipeline takes that size (the camera then follows the size
    /// rule).
    fn run_window(
        &self,
        pipeline: &mut Pipeline,
        cam: &CameraDevice,
        why: &mut String,
        hr: &mut HRESULT,
    ) -> Outcome {
        if !is_window(cam.window) {
            *why = "Cửa sổ đã đóng. Hãy chọn nguồn khác".to_string();
            return Outcome::Fatal;
        }
        let mut capture = WindowCapture::new();
        if !capture.open(hwnd_of(cam.window), why) {
            return Outcome::Fatal;
        }

        const K_INTERVAL_100NS: i64 = 333333; // the camera is published at 30 fps
        let (mut src_w, mut src_h) = (0u32, 0u32); // what the pipeline gets
        let (mut pending_w, mut pending_h) = (0u32, 0u32);
        let mut pending_since = 0u64;
        let mut content: Vec<u8> = Vec::new();
        let mut frame: Vec<u8> = Vec::new();
        let mut idle_since = 0u64;
        let mut last_frame = tick_ms();
        let mut last_taken = 0u64;
        let mut last_repeat = 0u64;
        let mut have_last = false; // `frame` holds the window's newest picture
        let mut ts = 0i64;
        let mut outcome = Outcome::Quit;
        *hr = S_OK;

        while !self.interrupted() {
            // Nobody needs the picture any more: stop capturing (back to standby).
            if !self.still_wanted(pipeline, &mut idle_since) {
                outcome = Outcome::Idle;
                break;
            }
            // The capture item's Closed event does not always come (seen on Windows 11 with a
            // WinForms window): ask Windows directly.
            if !is_window(cam.window) {
                *why = "Cửa sổ đã đóng. Hãy chọn nguồn khác".to_string();
                outcome = Outcome::Fatal;
                break;
            }
            let mut now = tick_ms();
            // A window sends frames only when it changes, up to the screen's rate: take at most
            // 30 a second (waiting rather than dropping, so the newest frame of a burst is the
            // one kept), and read one back from the GPU only when it goes somewhere.
            if now.wrapping_sub(last_taken) < 30 {
                std::thread::sleep(Duration::from_millis(30 - now.wrapping_sub(last_taken)));
                now = tick_ms();
            }
            let read = src_w == 0 || pipeline.will_publish();
            let result = capture.next(100, read, &mut |bgra: &[u8], pitch: u32, w: u32, h: u32| {
                let cw = w & !1;
                let ch = h & !1;
                if cw < 2 || ch < 2 {
                    return;
                }
                let now = tick_ms();
                if src_w == 0
                    || ((cw != src_w || ch != src_h)
                        && pending_w == cw
                        && pending_h == ch
                        && now.wrapping_sub(pending_since) >= 1000)
                {
                    src_w = cw;
                    src_h = ch;
                    *hr = pipeline.begin(src_w, src_h, 30, 1, &MFVideoFormat_ARGB32);
                    if hr.is_err() {
                        return;
                    }
                }
                if (cw != src_w || ch != src_h) && (pending_w != cw || pending_h != ch) {
                    pending_w = cw;
                    pending_h = ch;
                    pending_since = now;
                }
                frame.resize(nv12_bytes(src_w, src_h) as usize, 0);
                let plane = src_w as usize * src_h as usize;
                if cw == src_w && ch == src_h {
                    let (y, uv) = frame.split_at_mut(plane);
                    bgra_to_nv12(bgra, pitch as usize, cw, ch, y, uv, src_w as usize);
                } else {
                    content.resize(nv12_bytes(cw, ch) as usize, 0);
                    {
                        let (cy, cuv) = content.split_at_mut(cw as usize * ch as usize);
                        bgra_to_nv12(bgra, pitch as usize, cw, ch, cy, cuv, cw as usize);
                    }
                    let l: FrameLayout =
                        compute_layout(cw, ch, &ImageSettings::default(), src_w, src_h);
                    nv12_fill_bars(
                        &mut frame,
                        src_w,
                        src_h,
                        l.dest_x,
                        l.dest_y,
                        l.dest_width,
                        l.dest_height,
                    );
                    let (y, uv) = frame.split_at_mut(plane);
                    nv12_crop_scale_bilinear_to(
                        &content,
                        cw as usize,
                        cw,
                        ch,
                        &Nv12Rect {
                            x: 0.0,
                            y: 0.0,
                            w: cw as f64,
                            h: ch as f64,
                        },
                        &mut y[l.dest_y as usize * src_w as usize + l.dest_x as usize..],
                        &mut uv[(l.dest_y as usize / 2) * src_w as usize + l.dest_x as usize..],
                        src_w as usize,
                        l.dest_width,
                        l.dest_height,
                    );
                }
                have_last = true;
                pipeline.frame(&frame, src_w as usize, ts);
            });
            if result == CaptureResult::Closed {
                *why = "Cửa sổ đã đóng. Hãy chọn nguồn khác".to_string();
                outcome = Outcome::Fatal;
                break;
            }
            if result == CaptureResult::Error {
                // Capture failed for now (e.g. the graphics device was reset): open it again.
                *why = "Không quay được cửa sổ, đang thử lại".to_string();
                outcome = Outcome::Lost;
                break;
            }
            if hr.is_err() {
                *why = pipeline.failure().to_string();
                outcome = Outcome::Fatal;
                break;
            }
            if result == CaptureResult::Frame {
                last_frame = now;
                last_taken = now;
                ts += K_INTERVAL_100NS;
                if !read && src_w != 0 {
                    have_last = false;
                    pipeline.count(ts); // counted, not converted: nobody watches
                    pipeline.tick();
                }
            } else if src_w != 0 && now.wrapping_sub(last_frame) >= 500 {
                // A still window: the camera keeps showing its last picture. Written again once a
                // second when we have it: a camera rebuilt meanwhile (an app left) has none to
                // hold.
                if have_last && now.wrapping_sub(last_repeat) >= 1000 {
                    last_repeat = now;
                    ts += K_INTERVAL_100NS;
                    pipeline.frame(&frame, src_w as usize, ts);
                } else {
                    pipeline.hold();
                    pipeline.tick();
                }
            }
            if pipeline.broken() {
                *why = pipeline.failure().to_string();
                outcome = Outcome::Fatal;
                break;
            }
        }
        capture.close();
        if self.quit.load(SeqCst) {
            Outcome::Quit
        } else if self.switch.load(SeqCst) {
            Outcome::Switch
        } else {
            outcome
        }
    }
}

/// Runs the virtual camera on a worker thread. Live in an `Arc`/`Box` owned by the API layer;
/// every method takes `&self`.
pub struct CaptureEngine {
    shared: Arc<Shared>,
    thread: Mutex<Option<JoinHandle<()>>>,
}

impl Default for CaptureEngine {
    fn default() -> Self {
        Self::new()
    }
}

impl Drop for CaptureEngine {
    fn drop(&mut self) {
        self.stop();
    }
}

impl CaptureEngine {
    /// An engine on the real kernel-object names.
    pub fn new() -> CaptureEngine {
        Self::with_names(Names::default())
    }

    /// An engine on other shared-memory names (tests).
    pub fn with_names(names: Names) -> CaptureEngine {
        CaptureEngine {
            shared: Arc::new(Shared::new(names)),
            thread: Mutex::new(None),
        }
    }

    /// A window that gets `msg` when a preview frame is ready (or the state changed). One message
    /// waits at a time: the next is posted once `copy_preview` has been called. Set once, before
    /// any source runs. `hwnd` is the window handle value.
    pub fn set_notify(&self, hwnd: isize, msg: u32) {
        self.shared.notify_msg.store(msg, SeqCst);
        self.shared.notify_window.store(hwnd, SeqCst);
    }

    /// Returns the id the new source's status will carry (`Status::source_id`).
    pub fn start(&self, cam: &CameraDevice, format: &NativeFormat, image: &ImageSettings) -> u32 {
        self.stop();
        let id;
        {
            let mut g = lock(&self.shared.lock);
            g.camera = cam.clone();
            g.format = format.clone();
            g.image = *image;
            g.source_rotation = g.expected_rotation(cam, format);
            g.phone_state.clear();
            g.source_seq += 1;
            id = g.source_seq;
        }
        self.start_locked();
        lock(&self.shared.lock).status.source_id = id;
        id
    }

    /// Moves to another source, or another mode of the same one, keeping the virtual camera: an
    /// app using it sees the last picture frozen until the new source's first frame, never "no
    /// signal". Starts when nothing runs. An empty `CameraDevice` is "no source": the camera stays
    /// registered (at `format`'s size if it must be published now) and shows "no signal".
    pub fn switch(&self, cam: &CameraDevice, format: &NativeFormat) -> u32 {
        let running;
        let image;
        {
            let g = lock(&self.shared.lock);
            running = lock(&self.thread).is_some()
                && g.status.state != State::Failed
                && g.status.state != State::Stopped;
            image = g.image;
        }
        if !running {
            return self.start(cam, format, &image);
        }

        let id;
        let blocking;
        {
            let mut g = lock(&self.shared.lock);
            g.next_camera = cam.clone();
            g.next_format = format.clone();
            g.source_seq += 1;
            id = g.source_seq;
            g.next_source_id = id;
            self.shared.switch.store(true, SeqCst);
            blocking = g.blocking.clone();
        }
        // The source may be stuck reconnecting to a phone that is gone (up to ~12 s): cut it
        // short.
        if let Some(b) = blocking {
            b.abort();
        }
        id
    }

    pub fn stop(&self) {
        self.shared.quit.store(true, SeqCst);
        let blocking = lock(&self.shared.lock).blocking.clone();
        if let Some(b) = blocking {
            b.abort();
        }
        let handle = lock(&self.thread).take();
        if let Some(h) = handle {
            let _ = h.join();
        }
        let mut g = lock(&self.shared.lock);
        if g.status.state != State::Failed {
            g.status.state = State::Stopped;
        }
        g.status.fps = 0.0;
        g.status.shared_memory = false;
        g.status.consumer_active = false;
    }

    pub fn get_status(&self) -> Status {
        let g = lock(&self.shared.lock);
        let mut s = g.status.clone();
        s.source_rotation = g.source_rotation;
        s
    }

    /// Applies on the next frame. Returns true when the picture changes shape (rotation 0<->90,
    /// crop aspect): the virtual camera then follows the size rule (`set_output_size`).
    pub fn set_image(&self, image: &ImageSettings) -> bool {
        let mut g = lock(&self.shared.lock);
        let live = matches!(
            g.status.state,
            State::Running | State::Starting | State::Waiting
        );
        let before = g.effective_image();
        g.image = *image;
        self.shared.layout_gen.fetch_add(1, SeqCst); // the pipeline applies it on the next frame (or in standby)
        if !live || g.status.camera_width == 0 {
            return false;
        }
        let a = compute_layout(g.status.camera_width, g.status.camera_height, &before, 0, 0);
        let b = compute_layout(
            g.status.camera_width,
            g.status.camera_height,
            &g.effective_image(),
            0,
            0,
        );
        a.picture_width != b.picture_width || a.picture_height != b.picture_height
    }

    /// "Kích thước camera ảo". Auto: the source's own size while no app uses the camera; an app
    /// using it keeps its size and other pictures are fitted inside with black bars, until it
    /// lets go. Fixed sizes fit every source.
    pub fn set_output_size(&self, size: OutputSize) {
        self.shared.output_size.store(size as i32, SeqCst);
        self.shared.layout_gen.fetch_add(1, SeqCst);
    }

    /// "Chụp ảnh": the next frame the camera shows (or what it shows now with no source) as a
    /// PNG. Blocks up to 4 s: a closed source (standby) opens first.
    pub fn snapshot(&self, path: &str) -> HRESULT {
        let s = &self.shared;
        let mut frame: Vec<u8>;
        let (mut w, mut h);
        {
            let mut g = lock(&s.snap);
            g.snap.clear();
            s.snap_wanted.store(true, SeqCst);
            let (mut g2, _) = s
                .snap_ready
                .wait_timeout_while(g, Duration::from_secs(4), |_| s.snap_wanted.load(SeqCst))
                .unwrap_or_else(|e| e.into_inner());
            s.snap_wanted.store(false, SeqCst);
            frame = std::mem::take(&mut g2.snap);
            w = g2.w;
            h = g2.h;
        }
        if frame.is_empty() {
            // No source ("no signal"): what the camera holds now, if it is fresh.
            let mut writer = FrameWriter::with_names(s.names.clone());
            writer.use_local(directshow_mode());
            writer.try_open();
            match writer.copy_latest_fresh(&mut frame) {
                Some((fw, fh)) => {
                    w = fw;
                    h = fh;
                }
                None => return ERROR_NO_DATA.to_hresult(),
            }
        }
        save_png(path, &frame, w, h)
    }

    /// "Ghi hình": what the camera shows, from now until `stop_recording`, to an MP4.
    pub fn start_recording(&self, path: &str, microphone: bool) -> HRESULT {
        let (w, h, mut fps);
        {
            let g = lock(&self.shared.lock);
            w = g.status.width;
            h = g.status.height;
            fps = 30;
            if g.format.fps_num != 0 && g.format.fps_den != 0 {
                fps = (g.format.fps_num + g.format.fps_den / 2) / g.format.fps_den;
            }
        }
        if w == 0 || h == 0 {
            return ERROR_NOT_READY.to_hresult();
        }
        let hr = self.shared.recorder.start(path, w, h, fps, microphone);
        self.shared.notify();
        hr
    }

    pub fn stop_recording(&self) -> HRESULT {
        self.shared.recorder.stop()
    }

    pub fn recording(&self) -> &Recorder {
        &self.shared.recorder
    }

    /// "Nền": the person stays sharp; the background is blurred (strength 0..100) or replaced by
    /// an image. Cameras only (never a screen or a window), and only on frames that go somewhere.
    /// "Nền" in high quality (ADR-008): on a soft matte refined against the picture, smoothed over
    /// time, with a halo-free blur and a light wrap for replacement; off: the classic chain.
    pub fn set_background_quality(&self, on: bool) {
        self.shared.bg_quality.store(on, SeqCst);
    }

    pub fn set_background(&self, mode: BackgroundMode, strength: i32, image: &str, model: &str) {
        {
            let mut g = lock(&self.shared.lock);
            if image != g.bg_image {
                g.bg_gen = g.bg_gen.wrapping_add(1);
            }
            g.bg_image = image.to_string();
            g.bg_model = model.to_string();
            if mode == BackgroundMode::Off {
                g.bg_state.active = false;
            }
        }
        self.shared
            .bg_strength
            .store(strength.clamp(0, 100), SeqCst);
        self.shared.bg_mode.store(mode as i32, SeqCst);
    }

    pub fn get_background(&self) -> BackgroundState {
        lock(&self.shared.lock).bg_state.clone()
    }

    /// "Micro điện thoại": ask a phone camera for its microphone and play it into `target` (an
    /// output device's endpoint id, or "wav:<path>"); empty: off. Applies at once.
    pub fn set_phone_audio(&self, target: &str) {
        {
            let mut g = lock(&self.shared.lock);
            if g.audio_target == target {
                return;
            }
            g.audio_target = target.to_string();
        }
        self.shared.audio_gen.fetch_add(1, SeqCst);
    }

    /// An app uses a microphone now: a phone camera source stays open with nobody on the camera
    /// (a call with the camera off still needs the phone's sound).
    pub fn set_audio_wanted(&self, wanted: bool) {
        self.shared.audio_wanted.store(wanted, SeqCst);
    }

    pub fn get_phone_audio(&self) -> PhoneAudioState {
        lock(&self.shared.lock).audio_state.clone()
    }

    /// "Làm mịn da": 0 off, 1..100. Cameras only.
    pub fn set_smoothing(&self, strength: i32) {
        self.shared.smoothing.store(strength.clamp(0, 100), SeqCst);
    }

    /// What apps see while the picture is paused: the camera's own "no signal", a card with text,
    /// the last picture blurred with the text, or an image file. Applies whenever the camera has
    /// no source; the app sets `NoSignal` unless it paused.
    pub fn set_pause_picture(&self, kind: PauseKind, text: &str, path: &str) {
        let mut g = lock(&self.shared.lock);
        if g.pause.kind == kind && g.pause.text == text && g.pause.path == path {
            return;
        }
        g.pause.kind = kind;
        g.pause.text = text.to_string();
        g.pause.path = path.to_string();
        g.pause.gen = g.pause.gen.wrapping_add(1);
    }

    /// "Tự căn khung": webcams and phone cameras are zoomed and moved to keep the faces in the
    /// shot. Screens and windows are never framed.
    pub fn set_auto_frame(&self, on: bool) {
        self.shared.auto_frame.store(on, SeqCst);
    }

    pub fn get_framing(&self) -> Framing {
        lock(&self.shared.lock).framing.clone()
    }

    pub fn set_preview_enabled(&self, enabled: bool) {
        self.shared.preview_enabled.store(enabled, SeqCst);
    }

    /// Largest preview size worth producing (the panel's size in pixels).
    pub fn set_preview_size(&self, max_width: u32, max_height: u32) {
        self.shared
            .preview_max_w
            .store(max_width.clamp(64, 3840), SeqCst);
        self.shared
            .preview_max_h
            .store(max_height.clamp(64, 3840), SeqCst);
    }

    /// Latest preview frame (BGRA, top-down). `frame_id` increases per update. Always reports the
    /// size and id; copies only when `dst` is given and big enough (`true` then).
    pub fn copy_preview(
        &self,
        dst: Option<&mut [u32]>,
        width: &mut u32,
        height: &mut u32,
        frame_id: &mut u64,
    ) -> bool {
        self.shared.notify_pending.store(false, SeqCst); // the window looked: the next frame may post again
        let g = lock(&self.shared.lock);
        *width = g.preview_w;
        *height = g.preview_h;
        *frame_id = g.preview_id;
        if g.preview.is_empty() {
            return false;
        }
        match dst {
            Some(d) if d.len() >= g.preview.len() => {
                d[..g.preview.len()].copy_from_slice(&g.preview);
                true
            }
            _ => false, // caller learns the size first
        }
    }

    /// Phones: latest "state" message (JSON) while streaming.
    pub fn phone_state(&self) -> String {
        lock(&self.shared.lock).phone_state.clone()
    }

    fn start_locked(&self) {
        let (cam, format);
        {
            let mut g = lock(&self.shared.lock);
            g.status = Status::default();
            g.status.state = State::Starting;
            g.preview.clear();
            g.preview_w = 0;
            g.preview_h = 0;
            cam = g.camera.clone();
            format = g.format.clone();
        }
        self.shared.quit.store(false, SeqCst);
        self.shared.switch.store(false, SeqCst);
        let shared = Arc::clone(&self.shared);
        let (c, f) = (cam, format);
        match std::thread::Builder::new()
            .name("niwcam-capture".into())
            .spawn(move || shared.run(c, f))
        {
            Ok(h) => *lock(&self.thread) = Some(h),
            Err(_) => self.shared.fail("Không tạo được luồng quay", E_FAIL),
        }
        self.shared.notify();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn phone_cam() -> CameraDevice {
        CameraDevice {
            name: "Redmi".into(),
            phone: Some(Arc::new(PhoneLink::new())),
            ..CameraDevice::default()
        }
    }

    #[test]
    fn lag_follows_the_clock_offset_and_forgets() {
        let mut lag = Lag::default();
        // A steady stream: arrival - pts constant (offset 5000 ms): no lag.
        for i in 0..10u64 {
            lag.update(5000.0 + i as f64 * 33.0, i as f64 * 33.0, 100 + i * 33);
        }
        assert_eq!(lag.current, 0.0);
        // A frame 120 ms late shows as 120 ms of lag and raises the peak.
        lag.update(5000.0 + 10.0 * 33.0 + 120.0, 10.0 * 33.0, 500);
        assert!((lag.current - 120.0).abs() < 1e-9);
        assert!((lag.peak - 120.0).abs() < 1e-9);
        // A pts jump: the offset is forgotten within two buckets (6 s).
        let t = 5000u64;
        lag.update(9000.0, 0.0, t);
        lag.update(9033.0, 33.0, t + 3100);
        lag.update(9066.0, 66.0, t + 6200);
        assert_eq!(lag.current, 0.0);
    }

    #[test]
    fn expected_rotation_defaults_and_memory() {
        let engine = CaptureEngine::with_names(Names::for_test("rot"));
        let g = lock(&engine.shared.lock);
        let cam = phone_cam();
        let mut fmt = NativeFormat {
            phone_camera: "cam0".into(),
            ..NativeFormat::default()
        };
        assert_eq!(g.expected_rotation(&cam, &fmt), 90);
        fmt.phone_camera = "screen".into();
        assert_eq!(g.expected_rotation(&cam, &fmt), 270);
        assert_eq!(g.expected_rotation(&CameraDevice::default(), &fmt), 0); // a webcam
        drop(g);
        lock(&engine.shared.lock)
            .rotation_memory
            .insert("Redmi|cam0".into(), 0);
        fmt.phone_camera = "cam0".into();
        assert_eq!(lock(&engine.shared.lock).expected_rotation(&cam, &fmt), 0);
    }

    #[test]
    fn effective_image_adds_the_phone_rotation() {
        let engine = CaptureEngine::with_names(Names::for_test("eff"));
        let mut g = lock(&engine.shared.lock);
        g.image.orientation.rotation = 270;
        g.source_rotation = 180;
        assert_eq!(g.effective_image().orientation.rotation, 90);
    }

    #[test]
    fn set_image_reports_a_new_shape_only_while_live() {
        let engine = CaptureEngine::with_names(Names::for_test("img"));
        let mut img = ImageSettings::default();
        img.orientation.rotation = 90;
        assert!(!engine.set_image(&img)); // nothing runs
        {
            let mut g = lock(&engine.shared.lock);
            g.status.state = State::Running;
            g.status.camera_width = 1920;
            g.status.camera_height = 1080;
        }
        img.orientation.rotation = 0;
        assert!(engine.set_image(&img)); // 90 -> 0: 1080x1920 becomes 1920x1080
        assert!(!engine.set_image(&img)); // same shape again
        img.aspect = niwcam_core::image_settings::CropAspect::R1x1;
        assert!(engine.set_image(&img)); // crop aspect changes the picture
    }

    #[test]
    fn no_source_is_an_empty_device() {
        assert!(is_no_source(&CameraDevice::default()));
        assert!(!is_no_source(&phone_cam()));
        assert!(!is_no_source(&CameraDevice {
            window: 5,
            ..CameraDevice::default()
        }));
    }

    #[test]
    fn status_error_text_carries_the_hresult() {
        let engine = CaptureEngine::with_names(Names::for_test("err"));
        engine
            .shared
            .fail("Không mở được", HRESULT(0x80070005u32 as i32));
        let s = engine.get_status();
        assert_eq!(s.state, State::Failed);
        assert_eq!(s.error, "Không mở được (0x80070005)");
        engine.shared.wait("Chờ", S_OK);
        let s = engine.get_status();
        assert_eq!((s.state, s.error.as_str()), (State::Waiting, "Chờ"));
    }

    #[test]
    fn pause_picture_bumps_the_generation_only_on_change() {
        let engine = CaptureEngine::with_names(Names::for_test("pause"));
        engine.set_pause_picture(PauseKind::Card, "Quay lại ngay", "");
        engine.set_pause_picture(PauseKind::Card, "Quay lại ngay", "");
        assert_eq!(lock(&engine.shared.lock).pause.gen, 1);
        engine.set_pause_picture(PauseKind::Blur, "Quay lại ngay", "");
        assert_eq!(lock(&engine.shared.lock).pause.gen, 2);
    }

    #[test]
    fn preview_size_and_effect_settings_are_clamped() {
        let engine = CaptureEngine::with_names(Names::for_test("clamp"));
        engine.set_preview_size(10, 99999);
        assert_eq!(engine.shared.preview_max_w.load(SeqCst), 64);
        assert_eq!(engine.shared.preview_max_h.load(SeqCst), 3840);
        engine.set_smoothing(500);
        assert_eq!(engine.shared.smoothing.load(SeqCst), 100);
        engine.set_background(BackgroundMode::Blur, -5, "", "m.onnx");
        assert_eq!(engine.shared.bg_strength.load(SeqCst), 0);
        assert_eq!(engine.shared.bg_mode.load(SeqCst), 1);
    }

    #[test]
    fn copy_preview_reports_size_first() {
        let engine = CaptureEngine::with_names(Names::for_test("prev"));
        let (mut w, mut h, mut id) = (0, 0, 0);
        assert!(!engine.copy_preview(None, &mut w, &mut h, &mut id)); // nothing yet
        let y = vec![128u8; 640 * 360];
        let uv = vec![128u8; 640 * 180];
        engine.shared.update_preview(&y, &uv, 640, 640, 360);
        assert!(!engine.copy_preview(None, &mut w, &mut h, &mut id));
        assert_eq!((w, h, id), (640, 360, 1));
        let mut small = vec![0u32; 10];
        assert!(!engine.copy_preview(Some(&mut small), &mut w, &mut h, &mut id));
        let mut px = vec![0u32; 640 * 360];
        assert!(engine.copy_preview(Some(&mut px), &mut w, &mut h, &mut id));
        assert_eq!(px[0] >> 24, 0xFF); // opaque BGRA
    }

    #[test]
    fn phone_error_codes() {
        assert_eq!(phone_error_outcome("permission"), Outcome::Fatal);
        assert_eq!(phone_error_outcome("unsupported"), Outcome::Fatal);
        assert_eq!(phone_error_outcome("encoder"), Outcome::Fatal);
        assert_eq!(phone_error_outcome("busy"), Outcome::Lost);
        assert_eq!(phone_error_outcome(""), Outcome::Lost);
    }

    #[test]
    fn jobj_keeps_the_key_order_of_the_cpp() {
        let v = jobj! { "type" => "start", "lens" => "cam0", "width" => 1280u32, "audio" => false };
        assert_eq!(
            v.dump(),
            r#"{"type":"start","lens":"cam0","width":1280,"audio":false}"#
        );
    }
}
