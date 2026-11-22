//! The flat C API of `NiwCamEngine.dll` (`src/engine/EngineApi.{h,cpp}`), consumed by the WinUI app
//! through P/Invoke. Same names, same signatures, same results. Every export is wrapped so a panic
//! never crosses into the app: it returns the function's failure value instead.

use std::collections::{BTreeMap, HashMap};
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::atomic::{AtomicI32, AtomicU32, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock};
use std::time::Duration;

use niwcam_core::image_settings::{CropAspect, ImageSettings, OutputSize};
use niwcam_core::json::Value;
use niwcam_media::audio_out::AudioOut;
use niwcam_media::mf_devices::{
    enumerate_cameras, is_our_virtual_camera, read_camera_formats, to_lower, CameraDevice,
    FormatOption, LensFacing, NativeFormat,
};
use niwcam_phone::connector::{endpoint_key, Connector, LinkState};
use niwcam_phone::discovery::{self, Endpoint};
use niwcam_phone::phone_link::{forget_pair_key, has_pair_key, LeaseGuard, PhoneLink};
use windows::Win32::Foundation::{E_FAIL, E_INVALIDARG, E_UNEXPECTED, HWND, S_FALSE, S_OK};
use windows::Win32::Media::MediaFoundation::{
    MFMediaType_Video, MFShutdown, MFStartup, MFTEnumEx, MFVideoFormat_ARGB32, MFVideoFormat_H264,
    MFVideoFormat_HEVC, MFVideoFormat_NV12, MFSTARTUP_FULL, MFT_CATEGORY_VIDEO_DECODER,
    MFT_ENUM_FLAG_LOCALMFT, MFT_ENUM_FLAG_SYNCMFT, MFT_REGISTER_TYPE_INFO, MF_VERSION,
};
use windows::Win32::System::Com::CoTaskMemFree;
use windows::Win32::UI::WindowsAndMessaging::{GetWindowTextW, IsWindow};

use crate::abi::{CsBackground, CsFormat, CsFraming, CsImage, CsStatus};
use crate::capture_engine::{BackgroundMode, CaptureEngine, PauseKind};
use crate::screen_viewer::ScreenViewer;

const CS_PHONE_MANUAL: i32 = 1;
const CS_PHONE_PAIRED: i32 = 2;

#[derive(Default)]
struct Globals {
    /// Created in cs_init, never at DLL load.
    engine: Option<Arc<CaptureEngine>>,
    /// The camera list as of the last cs_refresh_cameras. Indices change on every refresh; ids do not.
    cameras: Vec<CameraDevice>,
    /// Stable id per entry (cs_camera_id).
    ids: Vec<String>,
    /// 0 webcam, 1 phone camera, 2 phone screen.
    kinds: Vec<i32>,
    /// Parallel to the phone entries in `cameras`.
    endpoints: Vec<Endpoint>,
    formats: Vec<FormatOption>,
    /// Whose formats `formats` are.
    formats_id: String,
    phone_caps: String,
    last_error: String,
    pairing_code: String,
    /// The phone the virtual camera shows, if any.
    streaming_link: Option<Arc<PhoneLink>>,
    /// Screen windows, by cs_viewer_open id.
    viewers: BTreeMap<i32, ScreenViewer>,
    next_viewer: i32,
    windows: Vec<(isize, String)>,
    init_count: i32,
}

static G: Mutex<Globals> = Mutex::new(Globals {
    engine: None,
    cameras: Vec::new(),
    ids: Vec::new(),
    kinds: Vec::new(),
    endpoints: Vec::new(),
    formats: Vec::new(),
    formats_id: String::new(),
    phone_caps: String::new(),
    last_error: String::new(),
    pairing_code: String::new(),
    streaming_link: None,
    viewers: BTreeMap::new(),
    next_viewer: 0,
    windows: Vec::new(),
    init_count: 0,
});
/// The newest cs_load_formats call.
static FORMATS_REQUEST: AtomicU32 = AtomicU32::new(0);
/// -1 waiting, 0 declined, 1 accepted (PC user).
static PAIRING_DECISION: AtomicI32 = AtomicI32::new(-1);
/// Keeps paired phones connected. Created once and only stopped in cs_shutdown: discovery callbacks
/// may still call wake() on it.
static CONNECTOR: OnceLock<Connector> = OnceLock::new();

fn g() -> MutexGuard<'static, Globals> {
    G.lock().unwrap_or_else(|e| e.into_inner())
}

fn engine() -> Option<Arc<CaptureEngine>> {
    g().engine.clone()
}

/// Runs an export body; a panic gives `fallback` instead of unwinding into the app.
fn guard<T>(fallback: T, f: impl FnOnce() -> T) -> T {
    catch_unwind(AssertUnwindSafe(f)).unwrap_or(fallback)
}

// ---- strings -----------------------------------------------------------------------------------

/// # Safety
/// `p` is null or a NUL-terminated UTF-16 string.
unsafe fn wide(p: *const u16) -> String {
    if p.is_null() {
        return String::new();
    }
    let mut n = 0;
    // SAFETY: the caller promises a NUL-terminated string.
    while unsafe { *p.add(n) } != 0 {
        n += 1;
    }
    // SAFETY: `n` elements were just read.
    String::from_utf16_lossy(unsafe { std::slice::from_raw_parts(p, n) })
}

/// `wcsncpy_s(buffer, capacity, s, _TRUNCATE)`; returns the full length in UTF-16 units.
///
/// # Safety
/// `buffer` is null or has room for `capacity` UTF-16 units.
unsafe fn copy_out_w(s: &str, buffer: *mut u16, capacity: i32) -> i32 {
    let units: Vec<u16> = s.encode_utf16().collect();
    if !buffer.is_null() && capacity > 0 {
        let n = units.len().min(capacity as usize - 1);
        // SAFETY: n + 1 <= capacity units are written.
        unsafe {
            std::ptr::copy_nonoverlapping(units.as_ptr(), buffer, n);
            *buffer.add(n) = 0;
        }
    }
    units.len() as i32
}

/// The UTF-8 version: copies with its NUL only when it fits; returns the length needed.
///
/// # Safety
/// `buffer` is null or has room for `capacity` bytes.
unsafe fn copy_out_a(s: &str, buffer: *mut u8, capacity: i32) -> i32 {
    if !buffer.is_null() && capacity > s.len() as i32 {
        // SAFETY: len + 1 <= capacity bytes are written.
        unsafe {
            std::ptr::copy_nonoverlapping(s.as_ptr(), buffer, s.len());
            *buffer.add(s.len()) = 0;
        }
    }
    s.len() as i32 + 1
}

fn to_image(image: *const CsImage) -> ImageSettings {
    let mut s = ImageSettings::default();
    if image.is_null() {
        return s;
    }
    // SAFETY: the caller passes a CsImage.
    let i = unsafe { *image };
    s.orientation.rotation = ((i.rotation % 360) + 360) % 360;
    if s.orientation.rotation % 90 != 0 {
        s.orientation.rotation = 0;
    }
    s.orientation.mirror = i.mirror != 0;
    s.orientation.flip = i.flip != 0;
    s.aspect = match i.aspect {
        1 => CropAspect::R16x9,
        2 => CropAspect::R4x3,
        3 => CropAspect::R1x1,
        4 => CropAspect::R9x16,
        _ => CropAspect::Native,
    };
    s.zoom = i.zoom;
    s.center_x = i.center_x;
    s.center_y = i.center_y;
    let clamp100 = |v: i32| v.clamp(-100, 100);
    s.color.brightness = clamp100(i.brightness);
    s.color.contrast = clamp100(i.contrast);
    s.color.saturation = clamp100(i.saturation);
    s.color.temperature = clamp100(i.temperature);
    s
}

fn has_hevc_decoder() -> bool {
    static HAS: OnceLock<bool> = OnceLock::new();
    *HAS.get_or_init(|| {
        let input = MFT_REGISTER_TYPE_INFO {
            guidMajorType: MFMediaType_Video,
            guidSubtype: MFVideoFormat_HEVC,
        };
        let mut activates: *mut Option<windows::Win32::Media::MediaFoundation::IMFActivate> =
            std::ptr::null_mut();
        let mut count = 0u32;
        // SAFETY: out pointers are valid locals; MFTEnumEx allocates the array with CoTaskMemAlloc.
        let r = unsafe {
            MFTEnumEx(
                MFT_CATEGORY_VIDEO_DECODER,
                MFT_ENUM_FLAG_SYNCMFT | MFT_ENUM_FLAG_LOCALMFT,
                Some(&input),
                None,
                &mut activates,
                &mut count,
            )
        };
        if r.is_err() {
            return false;
        }
        // SAFETY: the array holds `count` interface pointers we own; each is released, then the array is freed.
        unsafe {
            for i in 0..count as usize {
                drop(activates.add(i).read());
            }
            CoTaskMemFree(Some(activates as *const _));
        }
        count > 0
    })
}

/// Phone capabilities -> the format list: per lens (in the phone's order), biggest first, every
/// listed frame rate. HEVC when both ends support it (better quality per Mbps over Wi-Fi), else H.264.
fn phone_formats(caps: &Value) -> Vec<FormatOption> {
    let mut lenses: HashMap<String, (String, LensFacing)> = HashMap::new();
    let mut order: Vec<String> = Vec::new();
    for l in caps["lenses"].items() {
        let facing = l["facing"].str();
        let id = l["id"].str().to_string();
        lenses.insert(
            id.clone(),
            (
                l["label"].str_or(&id),
                match facing {
                    "front" => LensFacing::Front,
                    "external" => LensFacing::External,
                    _ => LensFacing::Back,
                },
            ),
        );
        order.push(id);
    }

    let mut out: Vec<FormatOption> = Vec::new();
    for lens_id in &order {
        let mut first = true;
        for m in caps["modes"].items() {
            if m["lens"].str() != lens_id {
                continue;
            }
            let (mut h264, mut hevc) = (false, false);
            for c in m["codecs"].items() {
                h264 |= c.str() == "h264";
                hevc |= c.str() == "hevc";
            }
            // H.264 first: it always decodes on Windows. HEVC only when the phone offers nothing
            // else and the HEVC extension is installed.
            let use_hevc = hevc && !h264 && has_hevc_decoder();
            if !h264 && !use_hevc {
                continue;
            }
            let mut fps_list: Vec<i32> = m["fps"].items().iter().map(|f| f.int(0)).collect();
            fps_list.sort_unstable_by(|a, b| b.cmp(a));
            for fps in fps_list {
                let (label, facing) = &lenses[lens_id];
                let native = NativeFormat {
                    index: out.len() as u32,
                    width: m["width"].int(0) as u32,
                    height: m["height"].int(0) as u32,
                    fps_num: fps as u32,
                    fps_den: 1,
                    subtype: if use_hevc {
                        MFVideoFormat_HEVC
                    } else {
                        MFVideoFormat_H264
                    },
                    phone_camera: lens_id.clone(),
                    lens: *facing,
                };
                out.push(FormatOption {
                    native,
                    largest: first, // modes are listed biggest first
                    lens_label: label.clone(),
                });
                first = false;
            }
        }
    }
    out
}

fn valid_camera(g: &Globals, camera: i32) -> bool {
    camera >= 0 && (camera as usize) < g.cameras.len()
}

fn endpoint_of(g: &Globals, camera: i32) -> Endpoint {
    let webcams = g.cameras.len() - g.endpoints.len();
    g.endpoints[camera as usize - webcams].clone()
}

/// The id a phone source's pairing key is saved under: the hello id once connected, else the TXT id
/// (a screen advertises "<id>:screen", its hello says <id>).
fn pairing_id(cam: &CameraDevice, e: &Endpoint) -> String {
    let mut id = phone_of(cam).map(|p| p.device_id()).unwrap_or_default();
    if id.is_empty() {
        id = e.device_id.clone();
    }
    if let Some(stripped) = id.strip_suffix(":screen") {
        if id.len() > 7 {
            id = stripped.to_string();
        }
    }
    id
}

fn phone_of(cam: &CameraDevice) -> Option<Arc<PhoneLink>> {
    cam.phone
        .as_ref()
        .and_then(|p| p.clone().downcast::<PhoneLink>().ok())
}

fn connector() -> &'static Connector {
    CONNECTOR.get_or_init(Connector::new)
}

// ---- lifetime ----------------------------------------------------------------------------------

#[no_mangle]
pub extern "C" fn cs_init() -> i32 {
    cs_init_delayed(0)
}

#[no_mangle]
pub extern "C" fn cs_init_delayed(delay_ms: i32) -> i32 {
    guard(E_UNEXPECTED.0, || {
        let mut gl = g();
        let first = gl.init_count == 0;
        gl.init_count += 1;
        if first {
            // SAFETY: plain Media Foundation start-up.
            if let Err(e) = unsafe { MFStartup(MF_VERSION, MFSTARTUP_FULL) } {
                gl.init_count = 0;
                return e.code().0;
            }
            gl.engine = Some(Arc::new(CaptureEngine::new()));
            let c = connector();
            discovery::set_on_change(Some(Arc::new(|| connector().wake())));
            // Discovery starts on the connector's thread, after the delay (it may start adb).
            c.start(
                delay_ms.max(0) as u32,
                Some(Box::new(discovery::start_discovery)),
            );
        }
        S_OK.0
    })
}

#[no_mangle]
pub extern "C" fn cs_shutdown() {
    guard((), || {
        let (engine, viewers) = {
            let mut gl = g();
            if gl.init_count == 0 {
                return;
            }
            gl.init_count -= 1;
            if gl.init_count > 0 {
                return;
            }
            let engine = gl.engine.take();
            let viewers = std::mem::take(&mut gl.viewers);
            gl.cameras.clear();
            gl.ids.clear();
            gl.kinds.clear();
            gl.endpoints.clear();
            gl.formats.clear();
            gl.streaming_link = None;
            (engine, viewers)
        };
        if let Some(e) = engine {
            e.stop(); // joins the capture thread
        }
        drop(viewers); // and the screen windows' threads
        let c = connector();
        c.stop();
        c.clear(); // closes the idle links
        discovery::stop_discovery();
        // SAFETY: balances MFStartup.
        let _ = unsafe { MFShutdown() };
    })
}

// ---- cameras -----------------------------------------------------------------------------------

#[no_mangle]
pub extern "C" fn cs_refresh_cameras() -> i32 {
    guard(0, || {
        // Cheap enough for the UI thread: the phone list is kept current in the background.
        let webcams = enumerate_cameras();
        let phones = if CONNECTOR.get().is_some() {
            discovery::known_phones()
        } else {
            Vec::new()
        };
        let mut gl = g();
        gl.cameras.clear();
        gl.ids.clear();
        gl.kinds.clear();
        gl.endpoints.clear();
        for c in webcams {
            if is_our_virtual_camera(&c) {
                continue; // never loop our own output back
            }
            let key = if c.symbolic_link.is_empty() {
                c.name.clone()
            } else {
                to_lower(&c.symbolic_link)
            };
            gl.ids.push(format!("cam:{key}"));
            gl.kinds.push(0);
            gl.cameras.push(c);
        }
        for e in phones {
            let shown = if e.name.is_empty() { &e.host } else { &e.name };
            let suffix = if e.usb() {
                " (USB)".to_string()
            } else if e.manual {
                format!(" ({})", e.host)
            } else {
                " (Wi\u{2011}Fi)".to_string()
            };
            let link = connector().link(&e); // the same link every time: it may be connected already
            let cam = CameraDevice {
                name: format!("{shown}{suffix}"),
                phone: Some(link as Arc<dyn std::any::Any + Send + Sync>),
                ..CameraDevice::default()
            };
            gl.ids.push(format!("phone:{}", endpoint_key(&e)));
            gl.kinds.push(if e.screen { 2 } else { 1 });
            gl.cameras.push(cam);
            gl.endpoints.push(e);
        }
        let n = gl.cameras.len() as i32;
        drop(gl);
        if CONNECTOR.get().is_some() {
            connector().wake();
        }
        n
    })
}

/// # Safety
/// `buffer` is null or has room for `capacity` UTF-16 units.
#[no_mangle]
pub unsafe extern "C" fn cs_camera_name(camera: i32, buffer: *mut u16, capacity: i32) -> i32 {
    guard(-1, || {
        let gl = g();
        if !valid_camera(&gl, camera) {
            return -1;
        }
        // SAFETY: forwarded from this function's contract.
        unsafe { copy_out_w(&gl.cameras[camera as usize].name, buffer, capacity) }
    })
}

/// # Safety
/// `buffer` is null or has room for `capacity` UTF-16 units.
#[no_mangle]
pub unsafe extern "C" fn cs_camera_id(camera: i32, buffer: *mut u16, capacity: i32) -> i32 {
    guard(-1, || {
        let gl = g();
        if !valid_camera(&gl, camera) {
            return -1;
        }
        // SAFETY: forwarded from this function's contract.
        unsafe { copy_out_w(&gl.ids[camera as usize], buffer, capacity) }
    })
}

#[no_mangle]
pub extern "C" fn cs_camera_kind(camera: i32) -> i32 {
    guard(-1, || {
        let gl = g();
        if !valid_camera(&gl, camera) {
            return -1;
        }
        gl.kinds[camera as usize]
    })
}

/// # Safety
/// `error` is null or has room for `capacity` UTF-16 units.
#[no_mangle]
pub unsafe extern "C" fn cs_camera_state(camera: i32, error: *mut u16, capacity: i32) -> i32 {
    guard(-1, || {
        let endpoint = {
            let gl = g();
            if !valid_camera(&gl, camera) {
                return -1;
            }
            if !gl.cameras[camera as usize].is_phone() {
                // SAFETY: forwarded from this function's contract.
                unsafe { copy_out_w("", error, capacity) };
                return LinkState::Connected as i32; // a webcam is always there
            }
            endpoint_of(&gl, camera)
        };
        let mut why = String::new();
        let state = match CONNECTOR.get() {
            Some(c) => c.state(&endpoint_key(&endpoint), Some(&mut why)),
            None => LinkState::Idle,
        };
        // SAFETY: forwarded from this function's contract.
        unsafe { copy_out_w(&why, error, capacity) };
        state as i32
    })
}

#[no_mangle]
pub extern "C" fn cs_sources_generation() -> u32 {
    guard(0, || CONNECTOR.get().map_or(0, |c| c.generation()))
}

#[no_mangle]
pub extern "C" fn cs_load_formats(camera: i32) -> i32 {
    guard(E_FAIL.0, || load_formats(camera))
}

fn load_formats(camera: i32) -> i32 {
    // A phone's load can take long (pairing): a newer call (another camera picked meanwhile) wins.
    let request = FORMATS_REQUEST.fetch_add(1, Ordering::SeqCst) + 1;
    let (cam, endpoint, id) = {
        let mut gl = g();
        if !valid_camera(&gl, camera) {
            return E_INVALIDARG.0;
        }
        let cam = gl.cameras[camera as usize].clone();
        let id = gl.ids[camera as usize].clone();
        let endpoint = if cam.is_phone() {
            Some(endpoint_of(&gl, camera))
        } else {
            None
        };
        gl.last_error.clear();
        (cam, endpoint, id)
    };
    let set_error = |text: String| g().last_error = text;

    let mut formats: Vec<FormatOption> = Vec::new();
    let mut caps_text = String::new();
    match (&endpoint, phone_of(&cam)) {
        (Some(endpoint), Some(link)) => {
            // Usually instant: the connector (or an earlier load) already has the phone's caps, and
            // they are kept across reconnects. Otherwise connect now, pairing if needed.
            caps_text = link.caps();
            // Held until the end of this branch: connecting, pairing and asking for caps read the
            // link, and the connector (or anyone else) must stay off it meanwhile.
            let mut _lease: Option<LeaseGuard<'_>> = None;
            if caps_text.is_empty() {
                // A connector attempt in progress is cut short: this one may pair.
                if !link.lease(30000) {
                    set_error("Điện thoại đang bận, hãy thử lại sau giây lát".into());
                    return E_FAIL.0;
                }
                _lease = Some(LeaseGuard::new(&link));
                caps_text = link.caps(); // the connector may have just got them
            }
            if caps_text.is_empty() {
                let connect = || -> Result<(), String> {
                    PAIRING_DECISION.store(-1, Ordering::SeqCst);
                    link.connect(
                        endpoint,
                        Some(&|code: &str| {
                            let shown = if code.is_empty() {
                                String::new()
                            } else {
                                // substr(0, 3) + " " + substr(3)
                                let head: String = code.chars().take(3).collect();
                                let tail: String = code.chars().skip(3).collect();
                                format!("{head} {tail}")
                            };
                            g().pairing_code = shown;
                        }),
                        Some(&|| {
                            // Wait for the PC user's answer in the pairing dialog.
                            let mut i = 0;
                            while i < 1200 && PAIRING_DECISION.load(Ordering::SeqCst) < 0 {
                                std::thread::sleep(Duration::from_millis(100));
                                i += 1;
                            }
                            PAIRING_DECISION.load(Ordering::SeqCst) == 1
                        }),
                    )
                };
                let mut error = String::new();
                let mut ok = true;
                if !link.connected() {
                    if let Err(e) = connect() {
                        ok = false;
                        error = e;
                    }
                }
                let mut caps = Value::Null;
                if ok {
                    match link.request_caps(niwcam_phone::phone_link::REQUEST_CAPS_DEFAULT_MS) {
                        Ok(v) => caps = v,
                        Err(_) => {
                            // The old connection may be dead (phone app restarted): one fresh try.
                            link.close();
                            ok = false;
                            match connect() {
                                Ok(()) => match link
                                    .request_caps(niwcam_phone::phone_link::REQUEST_CAPS_DEFAULT_MS)
                                {
                                    Ok(v) => {
                                        caps = v;
                                        ok = true;
                                    }
                                    Err(e) => error = e,
                                },
                                Err(e) => error = e,
                            }
                        }
                    }
                }
                if !ok {
                    link.close();
                    set_error(error);
                    return E_FAIL.0;
                }
                caps_text = caps.dump();
            }
            let Ok(caps) = Value::parse(&caps_text) else {
                set_error("Thông tin camera của điện thoại bị hỏng".into());
                return E_FAIL.0;
            };
            formats = phone_formats(&caps);
            if formats.is_empty() {
                set_error("Điện thoại không có chế độ quay nào phù hợp".into());
                return E_FAIL.0;
            }
        }
        _ => {
            let hr = read_camera_formats(&cam, &mut formats);
            if hr.is_err() {
                set_error("Không đọc được định dạng của camera".into());
                return hr.0;
            }
        }
    }

    let mut gl = g();
    let count = formats.len() as i32;
    if request != FORMATS_REQUEST.load(Ordering::SeqCst) {
        return count; // superseded: the newer call's table stays
    }
    gl.formats = formats;
    gl.formats_id = id;
    gl.phone_caps = caps_text;
    count
}

/// # Safety
/// `format` is null or points to a `CsFormat`.
#[no_mangle]
pub unsafe extern "C" fn cs_get_format(index: i32, format: *mut CsFormat) -> i32 {
    guard(E_INVALIDARG.0, || {
        let gl = g();
        if format.is_null() || index < 0 || index as usize >= gl.formats.len() {
            return E_INVALIDARG.0;
        }
        let o = &gl.formats[index as usize];
        let mut f = CsFormat {
            width: o.native.width,
            height: o.native.height,
            fps_num: o.native.fps_num,
            fps_den: o.native.fps_den,
            fourcc: o.native.subtype.data1,
            is_native: i32::from(o.largest),
            lens: o.native.lens as i32,
            lens_label: [0; 64],
        };
        for (d, s) in f
            .lens_label
            .iter_mut()
            .take(63)
            .zip(o.lens_label.encode_utf16())
        {
            *d = s;
        }
        // SAFETY: `format` was checked for null; the caller passes a CsFormat.
        unsafe { *format = f };
        S_OK.0
    })
}

/// # Safety
/// `buffer` is null or has room for `capacity` UTF-16 units.
#[no_mangle]
pub unsafe extern "C" fn cs_last_error(buffer: *mut u16, capacity: i32) -> i32 {
    guard(0, || {
        let gl = g();
        // SAFETY: forwarded from this function's contract.
        unsafe { copy_out_w(&gl.last_error, buffer, capacity) }
    })
}

/// # Safety
/// `buffer` is null or has room for `capacity` UTF-16 units.
#[no_mangle]
pub unsafe extern "C" fn cs_pairing_code(buffer: *mut u16, capacity: i32) -> i32 {
    guard(0, || {
        let gl = g();
        // SAFETY: forwarded from this function's contract.
        unsafe { copy_out_w(&gl.pairing_code, buffer, capacity) }
    })
}

#[no_mangle]
pub extern "C" fn cs_pairing_answer(accept: i32) {
    PAIRING_DECISION.store(i32::from(accept != 0), Ordering::SeqCst);
}

/// # Safety
/// `host` is null or a NUL-terminated UTF-16 string.
#[no_mangle]
pub unsafe extern "C" fn cs_add_phone(host: *const u16, port: i32) {
    guard((), || {
        if host.is_null() || port <= 0 || port > 65535 {
            return;
        }
        // SAFETY: forwarded from this function's contract.
        discovery::add_manual(&unsafe { wide(host) }, port as u16);
    })
}

#[no_mangle]
pub extern "C" fn cs_phone_flags(camera: i32) -> i32 {
    guard(0, || {
        let gl = g();
        if !valid_camera(&gl, camera) || !gl.cameras[camera as usize].is_phone() {
            return 0;
        }
        let e = endpoint_of(&gl, camera);
        let paired = has_pair_key(&pairing_id(&gl.cameras[camera as usize], &e));
        (if e.manual { CS_PHONE_MANUAL } else { 0 }) | (if paired { CS_PHONE_PAIRED } else { 0 })
    })
}

/// # Safety
/// `buffer` is null or has room for `capacity` UTF-16 units.
#[no_mangle]
pub unsafe extern "C" fn cs_phone_identity(camera: i32, buffer: *mut u16, capacity: i32) -> i32 {
    guard(0, || {
        let gl = g();
        if !buffer.is_null() && capacity > 0 {
            // SAFETY: the caller's buffer has room for at least one unit.
            unsafe { *buffer = 0 };
        }
        if !valid_camera(&gl, camera) || !gl.cameras[camera as usize].is_phone() {
            return 0;
        }
        let e = endpoint_of(&gl, camera);
        // SAFETY: forwarded from this function's contract.
        unsafe {
            copy_out_w(
                &pairing_id(&gl.cameras[camera as usize], &e),
                buffer,
                capacity,
            )
        };
        match e.os.as_str() {
            "android" => 1,
            "ios" => 2,
            _ => 0,
        }
    })
}

#[no_mangle]
pub extern "C" fn cs_forget_phone(camera: i32, what: i32) {
    guard((), || {
        let (endpoint, links) = {
            let gl = g();
            if !valid_camera(&gl, camera) || !gl.cameras[camera as usize].is_phone() {
                return;
            }
            let endpoint = endpoint_of(&gl, camera);
            let id = pairing_id(&gl.cameras[camera as usize], &endpoint);
            let mut links: Vec<Arc<PhoneLink>> = Vec::new();
            if what & CS_PHONE_PAIRED != 0 {
                forget_pair_key(&id);
                // Every route to this phone (Wi-Fi, cable, its screen) still holds a session.
                for i in 0..gl.cameras.len() as i32 {
                    let cam = &gl.cameras[i as usize];
                    if cam.is_phone()
                        && (i == camera
                            || (!id.is_empty() && pairing_id(cam, &endpoint_of(&gl, i)) == id))
                    {
                        if let Some(l) = phone_of(cam) {
                            links.push(l);
                        }
                    }
                }
            }
            (endpoint, links)
        };
        for l in &links {
            l.abort(); // the lease holder closes it
        }
        if what & CS_PHONE_MANUAL != 0 && endpoint.manual {
            discovery::remove_manual(&endpoint.host, endpoint.port);
        }
        if let Some(c) = CONNECTOR.get() {
            c.wake();
        }
    })
}

// ---- source control ----------------------------------------------------------------------------

fn start_or_switch(camera: i32, format: i32, image: *const CsImage, keep_camera: bool) -> i32 {
    let (cam, fmt, engine) = {
        let mut gl = g();
        let Some(engine) = gl.engine.clone() else {
            return E_UNEXPECTED.0;
        };
        if !valid_camera(&gl, camera) {
            return E_INVALIDARG.0;
        }
        if gl.ids[camera as usize] != gl.formats_id
            || format < 0
            || format as usize >= gl.formats.len()
        {
            return E_INVALIDARG.0;
        }
        let cam = gl.cameras[camera as usize].clone();
        let fmt = gl.formats[format as usize].native.clone();
        gl.streaming_link = phone_of(&cam);
        (cam, fmt, engine)
    };
    let id = if keep_camera {
        engine.set_image(&to_image(image));
        engine.switch(&cam, &fmt)
    } else {
        engine.start(&cam, &fmt, &to_image(image))
    };
    (id & 0x7FFF_FFFF) as i32
}

/// # Safety
/// `image` is null or points to a `CsImage`.
#[no_mangle]
pub unsafe extern "C" fn cs_start(camera: i32, format: i32, image: *const CsImage) -> i32 {
    guard(E_FAIL.0, || start_or_switch(camera, format, image, false))
}

/// # Safety
/// `image` is null or points to a `CsImage`.
#[no_mangle]
pub unsafe extern "C" fn cs_switch(camera: i32, format: i32, image: *const CsImage) -> i32 {
    guard(E_FAIL.0, || start_or_switch(camera, format, image, true))
}

#[no_mangle]
pub extern "C" fn cs_switch_none(width: i32, height: i32) -> i32 {
    guard(E_FAIL.0, || {
        let engine = {
            let mut gl = g();
            let Some(engine) = gl.engine.clone() else {
                return E_UNEXPECTED.0;
            };
            gl.streaming_link = None;
            engine
        };
        let fmt = NativeFormat {
            width: if (2..=7680).contains(&width) {
                width as u32 & !1
            } else {
                1920
            },
            height: if (2..=7680).contains(&height) {
                height as u32 & !1
            } else {
                1080
            },
            fps_num: 30,
            fps_den: 1,
            subtype: MFVideoFormat_NV12,
            ..NativeFormat::default()
        };
        (engine.switch(&CameraDevice::default(), &fmt) & 0x7FFF_FFFF) as i32
    })
}

#[no_mangle]
pub extern "C" fn cs_stop() {
    guard((), || {
        if let Some(e) = engine() {
            e.stop();
        }
        g().streaming_link = None;
    })
}

/// # Safety
/// `image` is null or points to a `CsImage`.
#[no_mangle]
pub unsafe extern "C" fn cs_set_image(image: *const CsImage) -> i32 {
    guard(0, || {
        engine().map_or(0, |e| i32::from(e.set_image(&to_image(image))))
    })
}

#[no_mangle]
pub extern "C" fn cs_set_output_size(mode: i32) {
    guard((), || {
        if let Some(e) = engine() {
            let size = match mode {
                0 => OutputSize::Auto,
                1 => OutputSize::R2160,
                2 => OutputSize::R1080,
                3 => OutputSize::R720,
                _ => return,
            };
            e.set_output_size(size);
        }
    })
}

#[no_mangle]
pub extern "C" fn cs_set_auto_frame(enabled: i32) {
    guard((), || {
        if let Some(e) = engine() {
            e.set_auto_frame(enabled != 0);
        }
    })
}

/// # Safety
/// `text` and `image_path` are null or NUL-terminated UTF-16 strings.
#[no_mangle]
pub unsafe extern "C" fn cs_set_pause_picture(kind: i32, text: *const u16, image_path: *const u16) {
    guard((), || {
        let Some(e) = engine() else { return };
        let kind = match kind.clamp(0, 3) {
            1 => PauseKind::Card,
            2 => PauseKind::Blur,
            3 => PauseKind::Image,
            _ => PauseKind::NoSignal,
        };
        // SAFETY: forwarded from this function's contract.
        e.set_pause_picture(kind, &unsafe { wide(text) }, &unsafe { wide(image_path) });
    })
}

/// # Safety
/// `image_path` and `model_path` are null or NUL-terminated UTF-16 strings.
#[no_mangle]
pub unsafe extern "C" fn cs_set_background(
    mode: i32,
    strength: i32,
    image_path: *const u16,
    model_path: *const u16,
) {
    guard((), || {
        let Some(e) = engine() else { return };
        let mode = match mode.clamp(0, 2) {
            1 => BackgroundMode::Blur,
            2 => BackgroundMode::Image,
            _ => BackgroundMode::Off,
        };
        // SAFETY: forwarded from this function's contract.
        e.set_background(mode, strength, &unsafe { wide(image_path) }, &unsafe {
            wide(model_path)
        });
    })
}

/// "Nền" in high quality (ADR-008): 1 on, 0 off (the classic chain). Added next to the C++ API:
/// the C++ engine has no such function, the app checks for it before calling.
#[no_mangle]
pub extern "C" fn cs_set_background_quality(enabled: i32) {
    guard((), || {
        if let Some(e) = engine() {
            e.set_background_quality(enabled != 0);
        }
    })
}

#[no_mangle]
pub extern "C" fn cs_set_smoothing(strength: i32) {
    guard((), || {
        if let Some(e) = engine() {
            e.set_smoothing(strength);
        }
    })
}

/// # Safety
/// `out` is null or points to a `CsBackground`.
#[no_mangle]
pub unsafe extern "C" fn cs_get_background(out: *mut CsBackground) {
    guard((), || {
        if out.is_null() {
            return;
        }
        let mut o = CsBackground {
            available: 0,
            active: 0,
            infer_ms: 0.0,
            faces: 0,
        };
        if let Some(e) = engine() {
            let s = e.get_background();
            o = CsBackground {
                available: s.available,
                active: i32::from(s.active),
                infer_ms: s.infer_ms,
                faces: s.faces,
            };
        }
        // SAFETY: `out` was checked for null.
        unsafe { *out = o };
    })
}

/// # Safety
/// `target` is null or a NUL-terminated UTF-16 string.
#[no_mangle]
pub unsafe extern "C" fn cs_set_phone_audio(target: *const u16) {
    guard((), || {
        if let Some(e) = engine() {
            // SAFETY: forwarded from this function's contract.
            e.set_phone_audio(&unsafe { wide(target) });
        }
    })
}

#[no_mangle]
pub extern "C" fn cs_set_audio_wanted(wanted: i32) {
    guard((), || {
        if let Some(e) = engine() {
            e.set_audio_wanted(wanted != 0);
        }
    })
}

/// # Safety
/// `problem` is null or has room for `capacity` UTF-16 units.
#[no_mangle]
pub unsafe extern "C" fn cs_phone_audio_state(problem: *mut u16, capacity: i32) -> i32 {
    guard(-1, || {
        let Some(e) = engine() else { return -1 };
        let s = e.get_phone_audio();
        // SAFETY: forwarded from this function's contract.
        unsafe { copy_out_w(&s.problem, problem, capacity) };
        if !s.asked {
            -1
        } else if s.receiving {
            1
        } else {
            0
        }
    })
}

/// # Safety
/// `buffer` is null or has room for `capacity` UTF-16 units.
#[no_mangle]
pub unsafe extern "C" fn cs_audio_outputs(buffer: *mut u16, capacity: i32) -> i32 {
    guard(0, || {
        let mut all = String::new();
        for (id, name) in AudioOut::outputs() {
            all.push_str(&id);
            all.push('\t');
            all.push_str(&name);
            all.push('\n');
        }
        // SAFETY: forwarded from this function's contract.
        unsafe { copy_out_w(&all, buffer, capacity) }
    })
}

/// # Safety
/// `path` is null or a NUL-terminated UTF-16 string.
#[no_mangle]
pub unsafe extern "C" fn cs_snapshot(path: *const u16) -> i32 {
    guard(E_FAIL.0, || {
        let Some(e) = engine() else {
            return E_UNEXPECTED.0;
        };
        if path.is_null() {
            return E_UNEXPECTED.0;
        }
        // SAFETY: forwarded from this function's contract.
        e.snapshot(&unsafe { wide(path) }).0
    })
}

/// # Safety
/// `path` is null or a NUL-terminated UTF-16 string.
#[no_mangle]
pub unsafe extern "C" fn cs_record_start(path: *const u16, microphone: i32) -> i32 {
    guard(E_FAIL.0, || {
        let Some(e) = engine() else {
            return E_UNEXPECTED.0;
        };
        if path.is_null() {
            return E_UNEXPECTED.0;
        }
        // SAFETY: forwarded from this function's contract.
        e.start_recording(&unsafe { wide(path) }, microphone != 0).0
    })
}

#[no_mangle]
pub extern "C" fn cs_record_stop() -> i32 {
    guard(E_FAIL.0, || {
        engine().map_or(S_FALSE.0, |e| e.stop_recording().0)
    })
}

/// # Safety
/// `has_audio` is null or points to an `i32`; `problem` is null or has room for `capacity` UTF-16 units.
#[no_mangle]
pub unsafe extern "C" fn cs_record_status(
    has_audio: *mut i32,
    problem: *mut u16,
    capacity: i32,
) -> i64 {
    guard(0, || {
        let Some(e) = engine() else { return 0 };
        let r = e.recording();
        if !has_audio.is_null() {
            // SAFETY: checked for null.
            unsafe { *has_audio = i32::from(r.has_audio()) };
        }
        // SAFETY: forwarded from this function's contract.
        unsafe { copy_out_w(&r.audio_problem(), problem, capacity) };
        r.elapsed_ms()
    })
}

/// # Safety
/// `out` is null or points to a `CsFraming`.
#[no_mangle]
pub unsafe extern "C" fn cs_get_framing(out: *mut CsFraming) {
    guard((), || {
        if out.is_null() {
            return;
        }
        let mut o = CsFraming {
            zoom: 1.0,
            center_x: 0.5,
            center_y: 0.5,
            max_zoom: 1.0,
            active: 0,
            faces: 0,
            available: 1,
            camera_source: 0,
        };
        if let Some(e) = engine() {
            let f = e.get_framing();
            o = CsFraming {
                zoom: f.zoom,
                center_x: f.center_x,
                center_y: f.center_y,
                max_zoom: f.max_zoom,
                active: i32::from(f.active),
                faces: f.faces,
                available: f.available,
                camera_source: i32::from(f.camera_source),
            };
        }
        // SAFETY: `out` was checked for null.
        unsafe { *out = o };
    })
}

/// # Safety
/// `out` is null or points to a `CsStatus`.
#[no_mangle]
pub unsafe extern "C" fn cs_get_status(out: *mut CsStatus) {
    guard((), || {
        if out.is_null() {
            return;
        }
        // SAFETY: CsStatus is plain data; all-zero is the C++ `*out = {}`.
        let mut o: CsStatus = unsafe { std::mem::zeroed() };
        if let Some(e) = engine() {
            let s = e.get_status();
            o.state = s.state as i32;
            o.width = s.width;
            o.height = s.height;
            o.camera_width = s.camera_width;
            o.camera_height = s.camera_height;
            o.camera_fourcc = s.camera_subtype.data1;
            o.fps = s.fps;
            o.process_ms = s.process_ms;
            o.frames_captured = s.frames_captured;
            o.frames_dropped = s.frames_dropped;
            o.shared_memory = i32::from(s.shared_memory);
            o.consumer_active = i32::from(s.consumer_active);
            o.source_rotation = s.source_rotation;
            o.standby = i32::from(s.standby);
            o.picture_width = s.picture_width;
            o.picture_height = s.picture_height;
            o.source_id = s.source_id;
            o.no_source = i32::from(s.no_source);
            o.lag_ms = s.lag_ms;
            o.frames_skipped = s.frames_skipped;
            for (d, c) in o.error.iter_mut().take(255).zip(s.error.encode_utf16()) {
                *d = c;
            }
        }
        // SAFETY: `out` was checked for null.
        unsafe { *out = o };
    })
}

#[no_mangle]
pub extern "C" fn cs_set_preview_enabled(enabled: i32) {
    guard((), || {
        if let Some(e) = engine() {
            e.set_preview_enabled(enabled != 0);
        }
    })
}

/// # Safety
/// `window` is an HWND (or null).
#[no_mangle]
pub unsafe extern "C" fn cs_set_notify(window: *mut std::ffi::c_void, message: u32) {
    guard((), || {
        if let Some(e) = engine() {
            e.set_notify(window as isize, message);
        }
    })
}

#[no_mangle]
pub extern "C" fn cs_set_preview_size(max_width: i32, max_height: i32) {
    guard((), || {
        if let Some(e) = engine() {
            if max_width > 0 && max_height > 0 {
                e.set_preview_size(max_width as u32, max_height as u32);
            }
        }
    })
}

/// # Safety
/// `pixels` is null or has room for `capacity` pixels; the other pointers are null or valid.
#[no_mangle]
pub unsafe extern "C" fn cs_copy_preview(
    pixels: *mut u32,
    capacity: i32,
    width: *mut u32,
    height: *mut u32,
    frame_id: *mut u64,
) -> i32 {
    guard(0, || {
        let (mut w, mut h, mut id) = (0u32, 0u32, 0u64);
        let mut copied = false;
        if let Some(e) = engine() {
            let dst = if pixels.is_null() || capacity <= 0 {
                None
            } else {
                // SAFETY: the caller promises `capacity` pixels of room.
                Some(unsafe { std::slice::from_raw_parts_mut(pixels, capacity as usize) })
            };
            copied = e.copy_preview(dst, &mut w, &mut h, &mut id);
        }
        // SAFETY: each pointer is null or valid.
        unsafe {
            if !width.is_null() {
                *width = w;
            }
            if !height.is_null() {
                *height = h;
            }
            if !frame_id.is_null() {
                *frame_id = id;
            }
        }
        i32::from(copied)
    })
}

/// # Safety
/// `buffer` is null or has room for `capacity` bytes.
#[no_mangle]
pub unsafe extern "C" fn cs_phone_caps(buffer: *mut u8, capacity: i32) -> i32 {
    guard(0, || {
        let gl = g();
        // SAFETY: forwarded from this function's contract.
        unsafe { copy_out_a(&gl.phone_caps, buffer, capacity) }
    })
}

/// # Safety
/// `buffer` is null or has room for `capacity` bytes.
#[no_mangle]
pub unsafe extern "C" fn cs_phone_state(buffer: *mut u8, capacity: i32) -> i32 {
    guard(0, || {
        let state = engine().map(|e| e.phone_state()).unwrap_or_default();
        // SAFETY: forwarded from this function's contract.
        unsafe { copy_out_a(&state, buffer, capacity) }
    })
}

/// # Safety
/// `text` is null or a NUL-terminated UTF-8 string.
#[no_mangle]
pub unsafe extern "C" fn cs_phone_send(text: *const std::ffi::c_char) -> i32 {
    guard(E_FAIL.0, || {
        // Held, not looked up by index: refreshes may move it.
        let link = g().streaming_link.clone();
        let Some(link) = link else { return E_FAIL.0 };
        if text.is_null() {
            return E_FAIL.0;
        }
        // SAFETY: forwarded from this function's contract.
        let bytes = unsafe { std::ffi::CStr::from_ptr(text) }.to_bytes();
        match Value::parse_bytes(bytes) {
            Ok(v) => {
                if link.send_control(&v) {
                    S_OK.0
                } else {
                    E_FAIL.0
                }
            }
            Err(_) => E_INVALIDARG.0,
        }
    })
}

// ---- a phone's shared screen in its own window -------------------------------------------------

#[no_mangle]
pub extern "C" fn cs_viewer_open(camera: i32) -> i32 {
    guard(E_FAIL.0, || {
        let mut gl = g();
        if gl.engine.is_none() || !valid_camera(&gl, camera) || gl.kinds[camera as usize] != 2 {
            return E_INVALIDARG.0;
        }
        gl.next_viewer = gl.next_viewer.max(1);
        let id = gl.next_viewer;
        gl.next_viewer += 1;
        let Some(link) = phone_of(&gl.cameras[camera as usize]) else {
            return E_INVALIDARG.0;
        };
        let endpoint = endpoint_of(&gl, camera);
        gl.viewers.insert(id, ScreenViewer::new(link, endpoint));
        id
    })
}

#[no_mangle]
pub extern "C" fn cs_viewer_set_size(viewer: i32, max_width: i32, max_height: i32) {
    guard((), || {
        if let Some(v) = g().viewers.get(&viewer) {
            v.set_size(max_width.max(0) as u32, max_height.max(0) as u32);
        }
    })
}

/// # Safety
/// `pixels` is null or has room for `capacity` pixels; the other pointers are null or valid.
#[no_mangle]
pub unsafe extern "C" fn cs_viewer_copy(
    viewer: i32,
    pixels: *mut u32,
    capacity: i32,
    width: *mut u32,
    height: *mut u32,
    frame_id: *mut u64,
) -> i32 {
    guard(0, || {
        let (mut w, mut h, mut id) = (0u32, 0u32, 0u64);
        let mut copied = false;
        {
            let gl = g();
            if let Some(v) = gl.viewers.get(&viewer) {
                let dst = if pixels.is_null() || capacity <= 0 {
                    None
                } else {
                    // SAFETY: the caller promises `capacity` pixels of room.
                    Some(unsafe { std::slice::from_raw_parts_mut(pixels, capacity as usize) })
                };
                copied = v.copy(dst, &mut w, &mut h, &mut id);
            }
        }
        // SAFETY: each pointer is null or valid.
        unsafe {
            if !width.is_null() {
                *width = w;
            }
            if !height.is_null() {
                *height = h;
            }
            if !frame_id.is_null() {
                *frame_id = id;
            }
        }
        i32::from(copied)
    })
}

/// # Safety
/// The out pointers are null or valid; `error` has room for `capacity` UTF-16 units.
#[no_mangle]
pub unsafe extern "C" fn cs_viewer_state(
    viewer: i32,
    screen_width: *mut u32,
    screen_height: *mut u32,
    error: *mut u16,
    capacity: i32,
) -> i32 {
    guard(-1, || {
        let (mut w, mut h) = (0u32, 0u32);
        let mut why = String::new();
        let mut state = -1;
        {
            let gl = g();
            if let Some(v) = gl.viewers.get(&viewer) {
                state = v.get_state(&mut w, &mut h, &mut why) as i32;
            }
        }
        // SAFETY: each pointer is null or valid.
        unsafe {
            if !screen_width.is_null() {
                *screen_width = w;
            }
            if !screen_height.is_null() {
                *screen_height = h;
            }
            copy_out_w(&why, error, capacity);
        }
        state
    })
}

#[no_mangle]
pub extern "C" fn cs_viewer_close(viewer: i32) {
    guard((), || {
        let closing = g().viewers.remove(&viewer);
        drop(closing); // joins its thread (a connect in progress is aborted)
    })
}

// ---- a window on this PC as the camera's source ------------------------------------------------

#[no_mangle]
pub extern "C" fn cs_list_windows() -> i32 {
    guard(0, || {
        let windows: Vec<(isize, String)> = niwcam_fx::window_source::list_capture_windows()
            .into_iter()
            .map(|w| (w.hwnd.0 as isize, w.title))
            .collect();
        let mut gl = g();
        gl.windows = windows;
        gl.windows.len() as i32
    })
}

/// # Safety
/// `buffer` is null or has room for `capacity` UTF-16 units.
#[no_mangle]
pub unsafe extern "C" fn cs_window_title(index: i32, buffer: *mut u16, capacity: i32) -> i32 {
    guard(-1, || {
        let gl = g();
        if index < 0 || index as usize >= gl.windows.len() {
            return -1;
        }
        // SAFETY: forwarded from this function's contract.
        unsafe { copy_out_w(&gl.windows[index as usize].1, buffer, capacity) }
    })
}

#[no_mangle]
pub extern "C" fn cs_window_handle(index: i32) -> i64 {
    guard(0, || {
        let gl = g();
        if index < 0 || index as usize >= gl.windows.len() {
            return 0;
        }
        gl.windows[index as usize].0 as i64
    })
}

#[no_mangle]
pub extern "C" fn cs_switch_window(handle: i64, image: *const CsImage) -> i32 {
    guard(E_FAIL.0, || {
        let hwnd = HWND(handle as isize as *mut std::ffi::c_void);
        // SAFETY: IsWindow accepts any handle value.
        if hwnd.0.is_null() || !unsafe { IsWindow(Some(hwnd)) }.as_bool() {
            return E_INVALIDARG.0;
        }
        let mut title = [0u16; 256];
        // SAFETY: the buffer is a valid 256-unit slice.
        let n = unsafe { GetWindowTextW(hwnd, &mut title) }.max(0) as usize;
        let cam = CameraDevice {
            window: handle as isize,
            name: String::from_utf16_lossy(&title[..n]),
            ..CameraDevice::default()
        };
        let (width, height) = niwcam_fx::window_source::capture_window_size(hwnd);
        let fmt = NativeFormat {
            width,
            height,
            fps_num: 30,
            fps_den: 1,
            subtype: MFVideoFormat_ARGB32,
            ..NativeFormat::default()
        };
        let engine = {
            let mut gl = g();
            let Some(engine) = gl.engine.clone() else {
                return E_UNEXPECTED.0;
            };
            gl.streaming_link = None;
            engine
        };
        engine.set_image(&to_image(image));
        (engine.switch(&cam, &fmt) & 0x7FFF_FFFF) as i32
    })
}
