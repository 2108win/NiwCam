//! A phone's shared screen (docs/protocol.md 1.1) shown in a window of its own, so a meeting app can
//! share exactly that window. Port of `src/engine/ScreenViewer.{h,cpp}`. Not the virtual camera: its
//! own thread, the screen's link (port 47322) and decoder, the picture turned upright from "state",
//! and BGRA at the window's size. It runs alongside the virtual camera and does not follow the main
//! window's standby.

use std::sync::atomic::{AtomicBool, AtomicI32, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

use niwcam_core::json::Value;
use niwcam_core::nv12::{
    nv12_crop_scale_bilinear, nv12_orient, nv12_to_bgra, Nv12Orientation, Nv12Rect,
};
use niwcam_media::video_decoder::VideoDecoder;
use niwcam_phone::discovery::Endpoint;
use niwcam_phone::phone_link::{Kind, LeaseGuard, Message, PhoneLink, Recv};
use niwcam_phone::tick_ms;
use windows::Win32::System::Com::{CoInitializeEx, CoUninitialize, COINIT_MULTITHREADED};

/// A window shows ~30 pictures a second at most.
const DRAW_INTERVAL_MS: u64 = 30;

#[repr(i32)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ViewerState {
    Connecting = 1,
    Showing = 2,
    /// Lost or not reachable (error says why); tried again every 2 s.
    Waiting = 3,
}

#[derive(Default, Clone, Copy)]
struct ScreenMode {
    width: u32,
    height: u32,
    fps: u32,
    hevc: bool,
}

/// The screen's full size (the biggest mode), H.264 when offered (it always decodes on Windows),
/// at most 30 fps: a window does not need more.
fn pick_mode(caps: &Value) -> Option<ScreenMode> {
    let mut out = ScreenMode {
        fps: 30,
        ..ScreenMode::default()
    };
    let mut found = false;
    for m in caps["modes"].items() {
        let (mut h264, mut hevc) = (false, false);
        for c in m["codecs"].items() {
            h264 |= c.str() == "h264";
            hevc |= c.str() == "hevc";
        }
        if !h264 && !hevc {
            continue;
        }
        let w = m["width"].int(0) as u32;
        let h = m["height"].int(0) as u32;
        if found && u64::from(w) * u64::from(h) <= u64::from(out.width) * u64::from(out.height) {
            continue;
        }
        let (mut fps, mut lowest) = (0i32, 0i32);
        for f in m["fps"].items() {
            let v = f.int(0);
            if v <= 30 && v > fps {
                fps = v;
            }
            if lowest == 0 || v < lowest {
                lowest = v;
            }
        }
        let chosen = if fps != 0 {
            fps
        } else if lowest != 0 {
            lowest
        } else {
            30
        };
        out = ScreenMode {
            width: w,
            height: h,
            fps: chosen as u32,
            hevc: !h264,
        };
        found = w != 0 && h != 0;
    }
    found.then_some(out)
}

#[derive(Default)]
struct Out {
    state: Option<ViewerState>,
    error: String,
    pixels: Vec<u32>,
    width: u32,
    height: u32,
    screen_w: u32,
    screen_h: u32,
    frame_id: u64,
}

struct Shared {
    link: Arc<PhoneLink>,
    endpoint: Endpoint,
    quit: AtomicBool,
    /// In a connect: the destructor aborts it.
    connecting: AtomicBool,
    max_w: AtomicU32,
    max_h: AtomicU32,
    /// From the phone's "started"/"state".
    rotation: AtomicI32,
    out: Mutex<Out>,
}

impl Shared {
    fn out(&self) -> std::sync::MutexGuard<'_, Out> {
        self.out.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn set_state(&self, state: ViewerState, error: &str) {
        let mut o = self.out();
        o.state = Some(state);
        o.error = error.to_string();
    }
}

pub struct ScreenViewer {
    shared: Arc<Shared>,
    thread: Option<JoinHandle<()>>,
}

impl ScreenViewer {
    pub fn new(link: Arc<PhoneLink>, endpoint: Endpoint) -> ScreenViewer {
        let shared = Arc::new(Shared {
            link,
            endpoint,
            quit: AtomicBool::new(false),
            connecting: AtomicBool::new(false),
            max_w: AtomicU32::new(0),
            max_h: AtomicU32::new(0),
            rotation: AtomicI32::new(0),
            out: Mutex::new(Out {
                state: Some(ViewerState::Connecting),
                ..Out::default()
            }),
        });
        let s = shared.clone();
        let thread = std::thread::spawn(move || run(&s));
        ScreenViewer {
            shared,
            thread: Some(thread),
        }
    }

    /// The window's picture area in pixels. 0 x 0 (minimized): frames are decoded, not drawn.
    pub fn set_size(&self, max_width: u32, max_height: u32) {
        self.shared
            .max_w
            .store(max_width.min(7680), Ordering::Relaxed);
        self.shared
            .max_h
            .store(max_height.min(7680), Ordering::Relaxed);
    }

    /// Latest picture (BGRA, top-down), at most the window's size with the screen's aspect. Always
    /// reports its size and id; copies only when `dst` is big enough.
    pub fn copy(
        &self,
        dst: Option<&mut [u32]>,
        width: &mut u32,
        height: &mut u32,
        frame_id: &mut u64,
    ) -> bool {
        let o = self.shared.out();
        *width = o.width;
        *height = o.height;
        *frame_id = o.frame_id;
        match dst {
            Some(d) if !o.pixels.is_empty() && d.len() >= o.pixels.len() => {
                d[..o.pixels.len()].copy_from_slice(&o.pixels);
                true
            }
            _ => false,
        }
    }

    /// The screen upright at its own size (0 x 0 until the first frame), and the state.
    pub fn get_state(
        &self,
        screen_width: &mut u32,
        screen_height: &mut u32,
        error: &mut String,
    ) -> ViewerState {
        let o = self.shared.out();
        *screen_width = o.screen_w;
        *screen_height = o.screen_h;
        error.clone_from(&o.error);
        o.state.unwrap_or(ViewerState::Connecting)
    }
}

impl Drop for ScreenViewer {
    /// Stops the phone's stream; the link stays connected for the connector.
    fn drop(&mut self) {
        self.shared.quit.store(true, Ordering::SeqCst);
        if self.shared.connecting.load(Ordering::SeqCst) {
            self.shared.link.abort(); // a connect can take up to ~12 s
        }
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

fn sleep_ms(ms: u64) {
    std::thread::sleep(Duration::from_millis(ms));
}

fn run(s: &Shared) {
    // SAFETY: plain COM initialisation for this thread (the decoder MFT needs it).
    let com = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) }.is_ok();
    let mut work = Work::default();
    while !s.quit.load(Ordering::SeqCst) {
        s.set_state(ViewerState::Connecting, "");
        let mut why = String::new();
        stream(s, &mut work, &mut why);
        if s.quit.load(Ordering::SeqCst) {
            break;
        }
        s.set_state(
            ViewerState::Waiting,
            if why.is_empty() {
                "Mất kết nối với điện thoại"
            } else {
                &why
            },
        );
        for _ in 0..20 {
            if s.quit.load(Ordering::SeqCst) {
                break;
            }
            sleep_ms(100);
        }
    }
    if com {
        // SAFETY: balances the successful CoInitializeEx above on this thread.
        unsafe { CoUninitialize() };
    }
}

/// Viewer thread only.
#[derive(Default)]
struct Work {
    oriented: Vec<u8>,
    scaled: Vec<u8>,
    work: Vec<u32>,
    last_shown: u64,
}

fn new_message() -> Message {
    Message {
        kind: Kind::Control,
        pts_us: 0,
        flags: 0,
        payload: Vec::new(),
    }
}

fn control(kind: &str) -> Value {
    let mut v = Value::object();
    v.set("type", kind);
    v
}

fn stream(s: &Shared, w: &mut Work, why: &mut String) {
    let link: &PhoneLink = &s.link;
    // One reader per link. The connector gives way at once; when the virtual camera streams this
    // screen, the window waits until it moves to another source.
    let mut waited = 0;
    while !link.lease(200) {
        if s.quit.load(Ordering::SeqCst) {
            return;
        }
        if waited == 5 {
            s.set_state(
                ViewerState::Waiting,
                "Màn hình này đang là nguồn của camera ảo. Chọn nguồn khác cho camera ảo để xem ở đây.",
            );
        }
        waited += 1;
    }
    let _lease = LeaseGuard::new(link);
    s.set_state(ViewerState::Connecting, "");

    if !link.connected() {
        // A link that connected before knows where to find the phone again (its IP may have
        // changed); a new one uses the address it was listed with. Never pairs: the screen cannot
        // (docs/protocol.md 1.1), pairing happens with the camera.
        s.connecting.store(true, Ordering::SeqCst);
        let result = if s.quit.load(Ordering::SeqCst) {
            Err(String::new())
        } else if link.device_id().is_empty() {
            link.connect_known(&s.endpoint)
        } else {
            link.reconnect()
        };
        s.connecting.store(false, Ordering::SeqCst);
        if let Err(e) = result {
            *why = e;
            return;
        }
    }

    let mut caps = Value::Null;
    let text = link.caps();
    if !text.is_empty() {
        if let Ok(v) = Value::parse(&text) {
            caps = v;
        }
    }
    s.connecting.store(true, Ordering::SeqCst); // closing the window cuts this short too
    let mut error = String::new();
    let have_caps = caps.is_object()
        || (!s.quit.load(Ordering::SeqCst)
            && match link.request_caps(10000) {
                Ok(v) => {
                    caps = v;
                    true
                }
                Err(e) => {
                    error = e;
                    false
                }
            });
    s.connecting.store(false, Ordering::SeqCst);
    if !have_caps {
        link.close();
        *why = error;
        return;
    }
    let Some(mode) = pick_mode(&caps) else {
        *why = "Điện thoại không có chế độ chia sẻ màn hình nào phù hợp".into();
        return;
    };
    let kbps = ((u64::from(mode.width) * u64::from(mode.height) * u64::from(mode.fps) / 8000)
        as u32)
        .clamp(2000, 40000);
    let mut start = control("start");
    start
        .set("lens", "screen")
        .set("width", mode.width)
        .set("height", mode.height)
        .set("fps", mode.fps)
        .set("codec", if mode.hevc { "hevc" } else { "h264" })
        .set("bitrateKbps", kbps);
    if !link.send_control(&start) {
        link.close();
        *why = "Mất kết nối với điện thoại".into();
        return;
    }

    let state_from = |v: &Value| {
        if !v["orientation"].is_null() {
            s.rotation.store(
                ((v["orientation"].int(0) % 360) + 360) % 360,
                Ordering::Relaxed,
            );
        }
    };

    let mut started = Value::Null;
    let deadline = tick_ms() + 10000;
    let mut m = new_message();
    while !s.quit.load(Ordering::SeqCst) {
        match link.receive(&mut m, 500) {
            Recv::Message => {}
            other => {
                let timed_out = other == Recv::TimedOut;
                if timed_out && tick_ms() < deadline {
                    continue;
                }
                link.close();
                *why = if timed_out {
                    "Điện thoại không bắt đầu chia sẻ màn hình"
                } else {
                    "Mất kết nối với điện thoại"
                }
                .into();
                return;
            }
        }
        if m.kind != Kind::Control {
            continue;
        }
        let Ok(v) = m.json() else { continue };
        if v["type"].str() == "error" {
            *why = v["message"].str_or("Điện thoại báo lỗi");
            return;
        }
        if v["type"].str() == "started" {
            state_from(&v);
            started = v;
            break;
        }
    }
    if s.quit.load(Ordering::SeqCst) {
        link.send_control(&control("stop"));
        return;
    }

    let width = started["width"].int(mode.width as i32) as u32;
    let height = started["height"].int(mode.height as i32) as u32;
    let mut decoder = VideoDecoder::new();
    let hevc = if started["codec"].str() == "h264" {
        false
    } else {
        mode.hevc
    };
    if decoder.init(hevc, width, height).is_err() {
        link.send_control(&control("stop"));
        *why = if mode.hevc {
            "Không có bộ giải mã HEVC (cài \"HEVC Video Extensions\" từ Microsoft Store)"
        } else {
            "Không tạo được bộ giải mã video"
        }
        .into();
        return;
    }
    s.set_state(ViewerState::Showing, "");

    let mut config: Vec<u8> = Vec::new();
    let mut wait_keyframe = true;
    let mut unit: Vec<u8> = Vec::new();
    let (mut lost_frames, mut frames) = (0u32, 0u32);
    let mut last_frame = tick_ms();
    let mut last_stats = last_frame;
    while !s.quit.load(Ordering::SeqCst) {
        let got = link.receive(&mut m, 500);
        let now = tick_ms();
        if got == Recv::Failed {
            link.close();
            *why = "Mất kết nối với điện thoại".into();
            return;
        }
        if now - last_frame > 6000 {
            // The phone re-sends a still screen several times a second: silence means it stopped.
            link.send_control(&control("stop"));
            link.close();
            *why = "Điện thoại ngừng chia sẻ màn hình".into();
            return;
        }
        if got == Recv::Message && m.kind == Kind::Control {
            let Ok(v) = m.json() else { continue };
            let kind = v["type"].str();
            if kind == "error" {
                *why = v["message"].str_or("Điện thoại báo lỗi");
                return;
            }
            if kind == "stopped" {
                *why = "Điện thoại đã dừng chia sẻ màn hình".into();
                return;
            }
            if kind == "state" || kind == "started" {
                state_from(&v);
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
            let hr = decoder.decode(
                data,
                m.pts_us * 10,
                &mut |nv12: &[u8], pitch: i32, fw: u32, fh: u32| {
                    frames += 1;
                    show(s, w, nv12, pitch as usize, fw, fh);
                },
            );
            if hr.is_err() {
                lost_frames += 1;
                wait_keyframe = true;
                link.send_control(&control("keyframe"));
            } else {
                wait_keyframe = false;
            }
        }
        if now - last_stats >= 1000 {
            // For the phone's bitrate adaptation (docs/protocol.md 6).
            let mut stats = control("stats");
            stats
                .set(
                    "recvFps",
                    f64::from(frames) * 1000.0 / (now - last_stats) as f64,
                )
                .set("decodeMs", 0.0)
                .set("queueMs", 0)
                .set("lost", lost_frames as i32);
            link.send_control(&stats);
            frames = 0;
            lost_frames = 0;
            last_stats = now;
        }
    }
    link.send_control(&control("stop"));
}

/// Upright, at most the window's size (never bigger than the screen itself), then BGRA.
fn show(s: &Shared, w: &mut Work, nv12: &[u8], pitch: usize, width: u32, height: u32) {
    let o = Nv12Orientation {
        rotation: s.rotation.load(Ordering::Relaxed),
        mirror: false,
        flip: false,
    };
    let (fw, fh) = if o.swaps_size() {
        (height, width)
    } else {
        (width, height)
    };
    {
        let mut out = s.out();
        out.screen_w = fw;
        out.screen_h = fh;
    }
    let max_w = s.max_w.load(Ordering::Relaxed);
    let max_h = s.max_h.load(Ordering::Relaxed);
    let now = tick_ms();
    if max_w == 0 || max_h == 0 || now - w.last_shown < DRAW_INTERVAL_MS {
        return; // minimized, or drawn just now
    }
    w.last_shown = now;

    let (frame, frame_pitch): (&[u8], usize) = if o.is_identity() {
        (nv12, pitch)
    } else {
        w.oriented
            .resize(width as usize * height as usize * 3 / 2, 0);
        nv12_orient(nv12, pitch, width, height, &mut w.oriented, &o);
        (&w.oriented, fw as usize)
    };

    let scale = 1.0f64
        .min(f64::from(max_w) / f64::from(fw))
        .min(f64::from(max_h) / f64::from(fh));
    let ow = 2.max((f64::from(fw) * scale) as u32 & !1);
    let oh = 2.max((f64::from(fh) * scale) as u32 & !1);
    let (picture, p): (&[u8], usize) = if ow != fw || oh != fh {
        // Bilinear, so text stays readable when the window is smaller than the screen.
        w.scaled.resize(ow as usize * oh as usize * 3 / 2, 0);
        let rect = Nv12Rect {
            x: 0.0,
            y: 0.0,
            w: f64::from(fw),
            h: f64::from(fh),
        };
        nv12_crop_scale_bilinear(frame, frame_pitch, fw, fh, &rect, &mut w.scaled, ow, oh);
        (&w.scaled, ow as usize)
    } else {
        (frame, frame_pitch)
    };
    let (y, uv) = picture.split_at(p * oh as usize);
    w.work.resize(ow as usize * oh as usize, 0);
    nv12_to_bgra(y, uv, p, ow, oh, &mut w.work, ow, oh);
    let mut out = s.out();
    std::mem::swap(&mut out.pixels, &mut w.work);
    out.width = ow;
    out.height = oh;
    out.frame_id += 1;
}
