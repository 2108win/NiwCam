//! Streams a REAL phone (or any listed camera) through a `NiwCamEngine.dll` for a while and prints, every
//! second, what the engine reports: output fps, lag, dropped/skipped frames, per-frame pipeline time and
//! this process's CPU. For finding why a high resolution or frame rate stutters.
//!
//!   phone_perf --dll <path> --camera <name part> [--usb|--wifi] [--list] [--format 3840x2160@60] [--seconds 20]
//!
//! Uses the user's real pairing keys (no new code is asked) and the session-local channel
//! (`CAMSTUDIO_FORCE_DSHOW=1`), so the installed app's virtual camera is not touched. The phone serves
//! one PC at a time: close NiwCam first, or this takes the phone from it. `NIWCAM_TRACE=1` adds the engine's
//! own per-second line (decode time) on stderr.

use std::time::{Duration, Instant};

use windows::core::{w, PCSTR, PCWSTR};
use windows::Win32::Foundation::{FILETIME, HMODULE};
use windows::Win32::System::LibraryLoader::{GetProcAddress, LoadLibraryW};
use windows::Win32::System::Memory::{
    MapViewOfFile, OpenFileMappingW, FILE_MAP_READ, FILE_MAP_WRITE,
};
use windows::Win32::System::Threading::{GetCurrentProcess, GetProcessTimes};

use NiwCamEngine::abi::{CsFormat, CsImage, CsStatus};

struct Api(HMODULE);
impl Api {
    fn f<T: Copy>(&self, name: &str) -> T {
        let c = std::ffi::CString::new(name).unwrap();
        // SAFETY: valid module handle, NUL-terminated name; `T` is the export's exact signature.
        unsafe {
            let p = GetProcAddress(self.0, PCSTR(c.as_ptr() as *const u8))
                .unwrap_or_else(|| panic!("missing {name}"));
            std::mem::transmute_copy(&p)
        }
    }
}

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

fn arg(name: &str) -> Option<String> {
    let a: Vec<String> = std::env::args().collect();
    a.iter()
        .position(|x| x == name)
        .and_then(|i| a.get(i + 1).cloned())
}
fn flag(name: &str) -> bool {
    std::env::args().any(|a| a == name)
}
fn wstr(b: &[u16]) -> String {
    String::from_utf16_lossy(&b[..b.iter().position(|&c| c == 0).unwrap_or(b.len())])
}
fn cpu_seconds() -> f64 {
    let (mut c, mut e, mut k, mut u) = (
        FILETIME::default(),
        FILETIME::default(),
        FILETIME::default(),
        FILETIME::default(),
    );
    // SAFETY: plain query of this process.
    unsafe {
        let _ = GetProcessTimes(GetCurrentProcess(), &mut c, &mut e, &mut k, &mut u);
    }
    let t = |f: FILETIME| {
        ((u64::from(f.dwHighDateTime) << 32) | u64::from(f.dwLowDateTime)) as f64 / 1e7
    };
    t(k) + t(u)
}

fn main() {
    let dll = arg("--dll").expect("--dll");
    let part = arg("--camera")
        .expect("--camera <name part>")
        .to_lowercase();
    let seconds: u64 = arg("--seconds").and_then(|s| s.parse().ok()).unwrap_or(20);
    std::env::set_var("CAMSTUDIO_FORCE_DSHOW", "1");
    // The app tells the engine where the pairing keys are; without it the engine's own default is another folder.
    if std::env::var_os("CAMSTUDIO_DATA_DIR").is_none() {
        let local = std::env::var("LOCALAPPDATA").expect("LOCALAPPDATA");
        std::env::set_var("CAMSTUDIO_DATA_DIR", format!(r"{local}\NiwCam"));
    }
    let wide: Vec<u16> = dll.encode_utf16().chain([0]).collect();
    // SAFETY: NUL-terminated path.
    let module = unsafe { LoadLibraryW(PCWSTR(wide.as_ptr())) }.expect("load dll");
    let api = Api(module);
    type Idx = unsafe extern "C" fn(i32) -> i32;
    type Name = unsafe extern "C" fn(i32, *mut u16, i32) -> i32;
    let init: unsafe extern "C" fn(i32) -> i32 = api.f("cs_init_delayed");
    let refresh: unsafe extern "C" fn() -> i32 = api.f("cs_refresh_cameras");
    let cam_name: Name = api.f("cs_camera_name");
    let cam_kind: Idx = api.f("cs_camera_kind");
    let cam_state: Name = api.f("cs_camera_state");
    let load_formats: Idx = api.f("cs_load_formats");
    let get_format: unsafe extern "C" fn(i32, *mut CsFormat) -> i32 = api.f("cs_get_format");
    let pairing_code: unsafe extern "C" fn(*mut u16, i32) -> i32 = api.f("cs_pairing_code");
    let pairing_answer: unsafe extern "C" fn(i32) = api.f("cs_pairing_answer");
    let start: unsafe extern "C" fn(i32, i32, *const CsImage) -> i32 = api.f("cs_start");
    let stop: unsafe extern "C" fn() = api.f("cs_stop");
    let shutdown: unsafe extern "C" fn() = api.f("cs_shutdown");
    let status: unsafe extern "C" fn(*mut CsStatus) = api.f("cs_get_status");
    let preview: unsafe extern "C" fn(i32) = api.f("cs_set_preview_enabled");

    // SAFETY: the engine API as its header documents it.
    unsafe {
        println!("init={}", init(0));
        let mut found = -1;
        let mut name = String::new();
        let t0 = Instant::now();
        while found < 0 && t0.elapsed() < Duration::from_secs(45) {
            let n = refresh();
            for i in 0..n {
                let mut b = [0u16; 256];
                cam_name(i, b.as_mut_ptr(), 256);
                let nm = wstr(&b);
                let low = nm.to_lowercase();
                let usb = low.contains("(usb)");
                let route_ok = (!flag("--usb") || usb) && (!flag("--wifi") || !usb);
                if low.contains(&part) && route_ok && cam_kind(i) != 2 {
                    let mut e = [0u16; 256];
                    let st = cam_state(i, e.as_mut_ptr(), 256);
                    println!(
                        "camera[{i}] {nm} kind={} state={st} {}",
                        cam_kind(i),
                        wstr(&e)
                    );
                    if cam_kind(i) == 0 || st == 1 || t0.elapsed() > Duration::from_secs(20) {
                        found = i;
                        name = nm;
                    }
                }
            }
            if found < 0 {
                std::thread::sleep(Duration::from_millis(1000));
            }
        }
        assert!(found >= 0, "no matching camera found in 45 s");
        println!("using: {name}");
        // A pairing code would mean the key is not found: decline, never pair by accident.
        let done = std::sync::atomic::AtomicBool::new(false);
        let count = std::thread::scope(|sc| {
            sc.spawn(|| {
                let mut b = [0u16; 32];
                while !done.load(std::sync::atomic::Ordering::Relaxed) {
                    if pairing_code(b.as_mut_ptr(), 32) > 0 {
                        println!("!! the phone asks for pairing: declining");
                        pairing_answer(0);
                        break;
                    }
                    std::thread::sleep(Duration::from_millis(50));
                }
            });
            let c = load_formats(found);
            done.store(true, std::sync::atomic::Ordering::Relaxed);
            c
        });
        println!("formats={count}");
        let mut want: Option<i32> = None;
        let target = arg("--format");
        for i in 0..count.max(0) {
            let mut f: CsFormat = std::mem::zeroed();
            get_format(i, &mut f);
            let fps = f.fps_num as f64 / f.fps_den.max(1) as f64;
            let label = format!("{}x{}@{}", f.width, f.height, fps.round() as i32);
            println!(
                "  format[{i}] {label} fourcc={:08x} native={} {}",
                f.fourcc,
                f.is_native,
                wstr(&f.lens_label)
            );
            if want.is_none() && target.as_deref() == Some(label.as_str()) {
                want = Some(i);
            }
        }
        if flag("--list") {
            shutdown();
            return;
        }
        let index = want.unwrap_or(0);
        println!("start format[{index}]");
        preview(1);
        let image = CsImage {
            rotation: 0,
            mirror: 0,
            flip: 0,
            aspect: 0,
            zoom: 1.0,
            center_x: 0.5,
            center_y: 0.5,
            brightness: 0,
            contrast: 0,
            saturation: 0,
            temperature: 0,
        };
        println!("start={}", start(found, index, &image));

        // Open the shared memory like an app would and keep a reader "active", so frames go to the camera.
        let mut header: *mut Header = std::ptr::null_mut();
        let t1 = Instant::now();
        while header.is_null() && t1.elapsed() < Duration::from_secs(15) {
            if let Ok(h) = OpenFileMappingW(
                (FILE_MAP_READ | FILE_MAP_WRITE).0,
                false,
                w!("Local\\CamStudioVCam_Frames_v2"),
            ) {
                header =
                    MapViewOfFile(h, FILE_MAP_READ | FILE_MAP_WRITE, 0, 0, 0).Value as *mut Header;
            } else {
                std::thread::sleep(Duration::from_millis(100));
            }
        }
        if !header.is_null() {
            (*header).reader_active += 1;
        }
        let (mut last_latest, mut frames, mut cpu0, mut sec) = (0i64, 0u64, cpu_seconds(), 0u64);
        let t = Instant::now();
        let mut rows: Vec<(f64, f64, f64, u64, f64)> = Vec::new();
        let mut last_tick = Instant::now();
        while t.elapsed() < Duration::from_secs(seconds) {
            if !header.is_null() {
                let latest = std::ptr::read_volatile(&raw const (*header).latest);
                if latest != 0 && latest >> 2 != last_latest >> 2 {
                    frames += 1;
                    last_latest = latest;
                }
            }
            if last_tick.elapsed() >= Duration::from_secs(1) {
                last_tick = Instant::now();
                sec += 1;
                let mut st: CsStatus = std::mem::zeroed();
                status(&mut st);
                let cpu = cpu_seconds();
                let cpu_pct = (cpu - cpu0) * 100.0;
                cpu0 = cpu;
                println!(
                    "t={sec:>2}s state={} {}x{} camera={}x{} engine_fps={:>5.1} reader_fps={frames:>3} lag={:>4.0}ms dropped={} skipped={} pipeline={:.1}ms cpu={cpu_pct:>4.0}% err={}",
                    st.state, st.width, st.height, st.camera_width, st.camera_height, st.fps, st.lag_ms, st.frames_dropped, st.frames_skipped, st.process_ms, wstr(&st.error)
                );
                if sec > 4 {
                    rows.push((st.fps, frames as f64, st.lag_ms, st.frames_skipped, cpu_pct));
                }
                frames = 0;
            }
            std::thread::sleep(Duration::from_millis(2));
        }
        if !rows.is_empty() {
            let n = rows.len() as f64;
            let avg =
                |f: fn(&(f64, f64, f64, u64, f64)) -> f64| rows.iter().map(f).sum::<f64>() / n;
            let worst_lag = rows.iter().map(|r| r.2).fold(0.0, f64::max);
            println!(
                "SUMMARY (after 4 s): engine fps {:.1}, reader fps {:.1}, worst lag {:.0} ms, skipped {}, cpu {:.0} % of a core",
                avg(|r| r.0), avg(|r| r.1), worst_lag, rows.last().unwrap().3, avg(|r| r.4)
            );
        }
        if !header.is_null() {
            (*header).reader_active -= 1;
        }
        stop();
        shutdown();
    }
}
