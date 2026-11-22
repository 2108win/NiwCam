//! Engine driver: loads a `NiwCamEngine.dll` (the C++ one or the Rust one) and runs one fixed
//! scenario through the flat `cs_*` API, printing what happened as `key=value` lines.
//! Run it against both DLLs (`xcheck/engine.ps1`) and compare the lines.
//!
//! Safe next to the user's running app: it forces the Windows-10 style channel
//! (`CAMSTUDIO_FORCE_DSHOW=1`, session-local shared memory the installed app does not use), keeps
//! discovery and the background connector off (`cs_init_delayed` with a huge delay), uses a
//! throw-away data folder and a stand-in phone on a private port.
//!
//!   driver --dll <path> [--phone <niwcam-fake-phone.exe>] [--port 47997] [--seconds 6]

use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use windows::core::{w, PCSTR, PCWSTR};
use windows::Win32::Foundation::{CloseHandle, HANDLE, HMODULE};
use windows::Win32::System::LibraryLoader::{GetProcAddress, LoadLibraryW};
use windows::Win32::System::Memory::{MapViewOfFile, OpenFileMappingW, FILE_MAP_READ, FILE_MAP_WRITE};

use niwcam_engine::abi::{CsFormat, CsImage, CsStatus};

struct Api {
    module: HMODULE,
}

impl Api {
    fn load(path: &str) -> Api {
        let wide: Vec<u16> = path.encode_utf16().chain([0]).collect();
        // SAFETY: `wide` is a NUL-terminated UTF-16 path that outlives the call.
        let module = unsafe { LoadLibraryW(PCWSTR(wide.as_ptr())) }.unwrap_or_else(|e| panic!("LoadLibrary {path}: {e}"));
        Api { module }
    }

    fn f<T: Copy>(&self, name: &str) -> T {
        let c = std::ffi::CString::new(name).unwrap();
        // SAFETY: the module handle is valid and the name is NUL-terminated.
        let p = unsafe { GetProcAddress(self.module, PCSTR(c.as_ptr() as *const u8)) }.unwrap_or_else(|| panic!("missing export {name}"));
        // SAFETY: the caller names the exact signature of the export through `T`.
        unsafe { std::mem::transmute_copy(&p) }
    }
}

type FnI32 = unsafe extern "C" fn() -> i32;
type FnInit = unsafe extern "C" fn(i32) -> i32;
type FnVoid = unsafe extern "C" fn();
type FnAddPhone = unsafe extern "C" fn(*const u16, i32);
type FnIdx = unsafe extern "C" fn(i32) -> i32;
type FnName = unsafe extern "C" fn(i32, *mut u16, i32) -> i32;
type FnGetFormat = unsafe extern "C" fn(i32, *mut CsFormat) -> i32;
type FnStart = unsafe extern "C" fn(i32, i32, *const CsImage) -> i32;
type FnStatus = unsafe extern "C" fn(*mut CsStatus);
type FnSwitchNone = unsafe extern "C" fn(i32, i32) -> i32;
type FnSetI32 = unsafe extern "C" fn(i32);
type FnCopyPreview = unsafe extern "C" fn(*mut u32, i32, *mut u32, *mut u32, *mut u64) -> i32;
type FnAnswer = unsafe extern "C" fn(i32);
type FnCode = unsafe extern "C" fn(*mut u16, i32) -> i32;
type FnCaps = unsafe extern "C" fn(*mut u8, i32) -> i32;

fn wstr(buf: &[u16]) -> String {
    let n = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
    String::from_utf16_lossy(&buf[..n])
}

fn arg(name: &str) -> Option<String> {
    let a: Vec<String> = std::env::args().collect();
    a.iter().position(|x| x == name).and_then(|i| a.get(i + 1).cloned())
}

/// What a reader of the shared memory sees (the layout of `SharedFrameHeader`).
#[repr(C)]
struct Header {
    magic: u32,
    version: u32,
    width: u32,
    height: u32,
    stride: u32,
    slots: u32,
    frame_bytes: u64,
    latest: i64,
    write_time: i64,
    reader_active: i32,
}

fn kill(child: &mut Child) {
    let _ = child.kill();
    let _ = child.wait();
}

fn main() {
    let dll = arg("--dll").expect("--dll <path>");
    let port = arg("--port").unwrap_or_else(|| "47997".into());
    let seconds: u64 = arg("--seconds").and_then(|s| s.parse().ok()).unwrap_or(6);
    let data = std::env::temp_dir().join(format!("niwcam-driver-{}", std::process::id()));
    std::fs::create_dir_all(&data).unwrap();
    std::env::set_var("CAMSTUDIO_FORCE_DSHOW", "1");
    std::env::set_var("CAMSTUDIO_DATA_DIR", &data);
    println!("dll={dll}");

    let mut phone = arg("--phone").map(|exe| {
        Command::new(exe)
            .args(["--port", &port])
            .current_dir(&data)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("start fake phone")
    });
    std::thread::sleep(Duration::from_millis(800));

    let api = Api::load(&dll);
    let init: FnInit = api.f("cs_init_delayed");
    let shutdown: FnVoid = api.f("cs_shutdown");
    let add_phone: FnAddPhone = api.f("cs_add_phone");
    let refresh: FnI32 = api.f("cs_refresh_cameras");
    let camera_name: FnName = api.f("cs_camera_name");
    let camera_id: FnName = api.f("cs_camera_id");
    let camera_kind: FnIdx = api.f("cs_camera_kind");
    let load_formats: FnIdx = api.f("cs_load_formats");
    let get_format: FnGetFormat = api.f("cs_get_format");
    let pairing_code: FnCode = api.f("cs_pairing_code");
    let pairing_answer: FnAnswer = api.f("cs_pairing_answer");
    let start: FnStart = api.f("cs_start");
    let stop: FnVoid = api.f("cs_stop");
    let status: FnStatus = api.f("cs_get_status");
    let switch_none: FnSwitchNone = api.f("cs_switch_none");
    let preview_enabled: FnSetI32 = api.f("cs_set_preview_enabled");
    let copy_preview: FnCopyPreview = api.f("cs_copy_preview");
    let phone_caps: FnCaps = api.f("cs_phone_caps");

    // SAFETY: the engine API is called as its header documents, from one thread at a time except
    // the pairing helper below, which only calls the two thread-safe pairing functions.
    unsafe {
        let hr = init(3_600_000);
        println!("init={hr}");
        let host: Vec<u16> = "127.0.0.1\0".encode_utf16().collect();
        add_phone(host.as_ptr(), port.parse().unwrap());
        let n = refresh();
        println!("cameras={n}");
        let mut phone_index = -1;
        for i in 0..n {
            let mut b = [0u16; 256];
            camera_name(i, b.as_mut_ptr(), 256);
            let name = wstr(&b);
            camera_id(i, b.as_mut_ptr(), 256);
            let id = wstr(&b);
            let kind = camera_kind(i);
            println!("camera[{i}] kind={kind} name={name} id={id}");
            if kind == 1 && id.contains(&port) {
                phone_index = i;
            }
        }
        assert!(phone_index >= 0, "the stand-in phone was not listed");

        // Pairing: both sides accept. load_formats blocks, so answer from a helper thread.
        let done = AtomicBool::new(false);
        let count = std::thread::scope(|sc| {
            sc.spawn(|| {
                let mut b = [0u16; 32];
                while !done.load(Ordering::Relaxed) {
                    let len = pairing_code(b.as_mut_ptr(), 32);
                    if len > 0 {
                        println!("pairing_code_len={len}");
                        pairing_answer(1);
                        break;
                    }
                    std::thread::sleep(Duration::from_millis(50));
                }
            });
            let c = load_formats(phone_index);
            done.store(true, Ordering::Relaxed);
            c
        });
        println!("formats={count}");
        for i in 0..count.max(0) {
            let mut f: CsFormat = std::mem::zeroed();
            get_format(i, &mut f);
            println!("format[{i}] {}x{}@{}/{} fourcc={:08x} native={} lens={} label={}", f.width, f.height, f.fps_num, f.fps_den, f.fourcc, f.is_native, f.lens, wstr(&f.lens_label));
        }
        let mut caps = vec![0u8; 16384];
        let len = phone_caps(caps.as_mut_ptr(), caps.len() as i32);
        println!("caps_len={len}");

        preview_enabled(1);
        let image = CsImage { rotation: 0, mirror: 0, flip: 0, aspect: 0, zoom: 1.0, center_x: 0.5, center_y: 0.5, brightness: 0, contrast: 0, saturation: 0, temperature: 0 };
        let id = start(phone_index, 0, &image);
        println!("start={id}");

        // The shared memory (session-local channel): open it like a reader and hold it active.
        let mapping_name = w!("Local\\CamStudioVCam_Frames_v2");
        let mut mapping = HANDLE::default();
        let begin = Instant::now();
        let mut header: *mut Header = std::ptr::null_mut();
        while begin.elapsed() < Duration::from_secs(10) && header.is_null() {
            if let Ok(h) = OpenFileMappingW((FILE_MAP_READ | FILE_MAP_WRITE).0, false, mapping_name) {
                mapping = h;
                let view = MapViewOfFile(h, FILE_MAP_READ | FILE_MAP_WRITE, 0, 0, 0);
                header = view.Value as *mut Header;
            } else {
                std::thread::sleep(Duration::from_millis(100));
                let mut st: CsStatus = std::mem::zeroed();
                status(&mut st);
            }
        }
        println!("shared_memory_opened={}", !header.is_null());
        if !header.is_null() {
            // A reader is "active": the engine then writes every frame.
            (*(&raw mut (*header).reader_active)) += 1;
        }

        let mut last_latest = 0i64;
        let mut frames = 0u64;
        let t0 = Instant::now();
        let mut polls = Vec::new();
        let mut preview_id = 0u64;
        let mut preview_frames = 0u64;
        let mut pixels = vec![0u32; 1920 * 1080];
        while t0.elapsed() < Duration::from_secs(seconds) {
            if !header.is_null() {
                let latest = std::ptr::read_volatile(&raw const (*header).latest);
                if latest != 0 && latest >> 2 != last_latest >> 2 {
                    frames += 1;
                    last_latest = latest;
                }
            }
            let (mut pw, mut ph, mut pid) = (0u32, 0u32, 0u64);
            copy_preview(pixels.as_mut_ptr(), pixels.len() as i32, &mut pw, &mut ph, &mut pid);
            if pid != preview_id {
                preview_frames += 1;
                preview_id = pid;
            }
            if polls.len() < (t0.elapsed().as_secs() as usize) + 1 {
                let mut st: CsStatus = std::mem::zeroed();
                status(&mut st);
                polls.push(format!(
                    "t={}s state={} out={}x{} camera={}x{} fps={:.0} standby={} consumer={} noSource={} picture={}x{} preview={}x{}",
                    t0.elapsed().as_secs(), st.state, st.width, st.height, st.camera_width, st.camera_height, st.fps, st.standby, st.consumer_active, st.no_source, st.picture_width, st.picture_height, pw, ph
                ));
            }
            std::thread::sleep(Duration::from_millis(2));
        }
        for p in &polls {
            println!("{p}");
        }
        let secs = t0.elapsed().as_secs_f64();
        println!("reader_frames={frames} reader_fps={:.1}", frames as f64 / secs);
        println!("preview_updates={preview_frames} preview_fps={:.1}", preview_frames as f64 / secs);
        if !header.is_null() {
            println!("header width={} height={} stride={} slots={} frame_bytes={} version={}", (*header).width, (*header).height, (*header).stride, (*header).slots, (*header).frame_bytes, (*header).version);
        }

        // Switch to "no source": the camera stays, apps see "no signal".
        let hr = switch_none(1280, 720);
        println!("switch_none={hr}");
        std::thread::sleep(Duration::from_secs(3));
        let mut st: CsStatus = std::mem::zeroed();
        status(&mut st);
        println!("after_switch_none state={} noSource={} out={}x{} err={}", st.state, st.no_source, st.width, st.height, wstr(&st.error));

        if !header.is_null() {
            (*(&raw mut (*header).reader_active)) -= 1;
        }
        stop();
        shutdown();
        if !mapping.is_invalid() {
            let _ = CloseHandle(mapping);
        }
    }
    if let Some(p) = phone.as_mut() {
        kill(p);
    }
    let _ = std::fs::remove_dir_all(&data);
    println!("done");
}
