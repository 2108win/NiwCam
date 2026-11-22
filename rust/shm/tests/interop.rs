//! Cross-check of niwcam-shm against the C++ `FrameWriter` / `FrameReader` / `SharedFrameHeader`.
//!
//! The C++ code lives in `cpp_shm.dll` (built by `xcheck/run.ps1` from a patched *copy* of the
//! headers whose four kernel-object names are unique per test). This test loads it and drives the
//! real C++ writer and reader and the Rust ones on the same names, in both directions, comparing
//! the frames byte for byte. `NIWCAM_SHM_CPP_DLL` names the DLL; without it the tests that need it
//! are skipped (plain `cargo test` still runs the pure-Rust unit tests).
//!
//! Run with `--test-threads=1`: the C++ DLL holds one global name suffix.

#![allow(clippy::too_many_arguments)]
// The function types of the C++ DLL are declared once, in `Cpp`; the macro infers them.
#![allow(clippy::missing_transmute_annotations)]

use std::ffi::c_void;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use niwcam_core::nv12::{nv12_fit_into, nv12_scale};
use niwcam_shm::*;
use windows::core::PCWSTR;
use windows::Win32::Foundation::{HANDLE, INVALID_HANDLE_VALUE};
use windows::Win32::System::LibraryLoader::{GetProcAddress, LoadLibraryW};
use windows::Win32::System::Memory::{
    CreateFileMappingW, MapViewOfFile, OpenFileMappingW, FILE_MAP_ALL_ACCESS, FILE_MAP_READ,
    FILE_MAP_WRITE, PAGE_READWRITE,
};

// ---------------------------------------------------------------- the C++ DLL

struct Cpp {
    set_suffix: unsafe extern "C" fn(*const u16),
    layout: unsafe extern "C" fn(*mut u8, i32) -> i32,
    constants: unsafe extern "C" fn(*mut u8, i32) -> i32,
    clamp: unsafe extern "C" fn(*mut u32, *mut u32),
    cw_new: unsafe extern "C" fn() -> *mut c_void,
    cw_free: unsafe extern "C" fn(*mut c_void),
    cw_use_local: unsafe extern "C" fn(*mut c_void, i32),
    cw_try_open: unsafe extern "C" fn(*mut c_void),
    cw_is_open: unsafe extern "C" fn(*mut c_void) -> i32,
    cw_is_local: unsafe extern "C" fn(*mut c_void) -> i32,
    cw_consumer_active: unsafe extern "C" fn(*mut c_void) -> i32,
    cw_width: unsafe extern "C" fn(*mut c_void) -> u32,
    cw_height: unsafe extern "C" fn(*mut c_void) -> u32,
    cw_open_local: unsafe extern "C" fn(*mut c_void, u32, u32, *mut u32, *mut u32) -> i32,
    cw_close: unsafe extern "C" fn(*mut c_void),
    cw_write: unsafe extern "C" fn(*mut c_void, *const u8, i32, u32, u32),
    cw_copy_latest: unsafe extern "C" fn(*mut c_void, *mut u8, i32, *mut u32, *mut u32, i32) -> i32,
    cw_hold_for: unsafe extern "C" fn(*mut c_void, u32),
    cw_release_hold: unsafe extern "C" fn(*mut c_void),
    cr_new: unsafe extern "C" fn() -> *mut c_void,
    cr_free: unsafe extern "C" fn(*mut c_void),
    cr_open: unsafe extern "C" fn(*mut c_void, u32, u32, i32) -> i32,
    cr_open_existing: unsafe extern "C" fn(*mut c_void, i32) -> i32,
    cr_width: unsafe extern "C" fn(*mut c_void) -> u32,
    cr_height: unsafe extern "C" fn(*mut c_void) -> u32,
    cr_close: unsafe extern "C" fn(*mut c_void),
    cr_latest_frame: unsafe extern "C" fn(*mut c_void) -> i64,
    cr_delivered_frame: unsafe extern "C" fn(*mut c_void) -> i64,
    cr_set_active: unsafe extern "C" fn(*mut c_void, i32),
    cr_fill: unsafe extern "C" fn(*mut c_void, *mut u8, i32, u32, u32, i64) -> i32,
}

fn cpp() -> Option<&'static Cpp> {
    static CPP: OnceLock<Option<Cpp>> = OnceLock::new();
    CPP.get_or_init(|| {
        let path = std::env::var("NIWCAM_SHM_CPP_DLL").ok()?;
        let wide: Vec<u16> = path.encode_utf16().chain(Some(0)).collect();
        // SAFETY: loading the test DLL we built; the path is NUL-terminated.
        let lib = unsafe { LoadLibraryW(PCWSTR(wide.as_ptr())) }.expect("LoadLibrary cpp_shm.dll");
        macro_rules! sym {
            ($n:literal) => {{
                let name = concat!($n, "\0");
                // SAFETY: the symbol exists with the signature declared in `Cpp`.
                unsafe {
                    std::mem::transmute(
                        GetProcAddress(lib, windows::core::PCSTR(name.as_ptr()))
                            .unwrap_or_else(|| panic!("missing export {}", $n)),
                    )
                }
            }};
        }
        Some(Cpp {
            set_suffix: sym!("cpp_set_suffix"),
            layout: sym!("cpp_layout"),
            constants: sym!("cpp_constants"),
            clamp: sym!("cpp_clamp"),
            cw_new: sym!("cw_new"),
            cw_free: sym!("cw_free"),
            cw_use_local: sym!("cw_use_local"),
            cw_try_open: sym!("cw_try_open"),
            cw_is_open: sym!("cw_is_open"),
            cw_is_local: sym!("cw_is_local"),
            cw_consumer_active: sym!("cw_consumer_active"),
            cw_width: sym!("cw_width"),
            cw_height: sym!("cw_height"),
            cw_open_local: sym!("cw_open_local"),
            cw_close: sym!("cw_close"),
            cw_write: sym!("cw_write"),
            cw_copy_latest: sym!("cw_copy_latest"),
            cw_hold_for: sym!("cw_hold_for"),
            cw_release_hold: sym!("cw_release_hold"),
            cr_new: sym!("cr_new"),
            cr_free: sym!("cr_free"),
            cr_open: sym!("cr_open"),
            cr_open_existing: sym!("cr_open_existing"),
            cr_width: sym!("cr_width"),
            cr_height: sym!("cr_height"),
            cr_close: sym!("cr_close"),
            cr_latest_frame: sym!("cr_latest_frame"),
            cr_delivered_frame: sym!("cr_delivered_frame"),
            cr_set_active: sym!("cr_set_active"),
            cr_fill: sym!("cr_fill"),
        })
    })
    .as_ref()
}

macro_rules! need_cpp {
    () => {
        match cpp() {
            Some(c) => c,
            None => {
                eprintln!("SKIPPED: NIWCAM_SHM_CPP_DLL not set (run xcheck/run.ps1)");
                return;
            }
        }
    };
}

// ---------------------------------------------------------------- one name set per scenario

static SERIAL: Mutex<()> = Mutex::new(());
static COUNTER: AtomicU32 = AtomicU32::new(0);

/// Takes the global lock (the C++ DLL has one suffix) and makes a fresh unique name set for both
/// the Rust code and the C++ DLL.
fn scenario(c: &Cpp) -> (std::sync::MutexGuard<'static, ()>, Names, String) {
    let guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let n = COUNTER.fetch_add(1, Ordering::SeqCst);
    let suffix = format!(
        "{}_{}_{}",
        std::process::id(),
        n,
        Instant::now().elapsed().as_nanos() % 1000
    );
    let wide: Vec<u16> = suffix.encode_utf16().chain(Some(0)).collect();
    // SAFETY: NUL-terminated wide string, copied by the callee.
    unsafe { (c.set_suffix)(wide.as_ptr()) };
    (guard, Names::for_test(&suffix), suffix)
}

// ---------------------------------------------------------------- Rust/C++ wrappers

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Impl {
    Rust,
    Cpp,
}

impl Impl {
    fn other(self) -> Impl {
        match self {
            Impl::Rust => Impl::Cpp,
            Impl::Cpp => Impl::Rust,
        }
    }
}

enum W {
    Rust(FrameWriter),
    Cpp(&'static Cpp, *mut c_void),
}

// SAFETY: the C++ writer is used from one thread at a time in these tests.
unsafe impl Send for W {}

impl W {
    fn new(imp: Impl, c: &'static Cpp, names: &Names) -> W {
        match imp {
            Impl::Rust => W::Rust(FrameWriter::with_names(names.clone())),
            // SAFETY: plain constructor.
            Impl::Cpp => W::Cpp(c, unsafe { (c.cw_new)() }),
        }
    }
    fn use_local(&mut self, l: bool) {
        match self {
            W::Rust(w) => w.use_local(l),
            // SAFETY: valid object.
            W::Cpp(c, p) => unsafe { (c.cw_use_local)(*p, l as i32) },
        }
    }
    fn try_open(&mut self) {
        match self {
            W::Rust(w) => w.try_open(),
            // SAFETY: valid object.
            W::Cpp(c, p) => unsafe { (c.cw_try_open)(*p) },
        }
    }
    /// `try_open` until open (it is rate limited to one attempt per 500 ms).
    fn wait_open(&mut self) -> bool {
        for _ in 0..50 {
            self.try_open();
            if self.is_open() {
                return true;
            }
            std::thread::sleep(Duration::from_millis(120));
        }
        false
    }
    fn is_open(&self) -> bool {
        match self {
            W::Rust(w) => w.is_open(),
            // SAFETY: valid object.
            W::Cpp(c, p) => unsafe { (c.cw_is_open)(*p) != 0 },
        }
    }
    fn is_local(&self) -> bool {
        match self {
            W::Rust(w) => w.is_local(),
            // SAFETY: valid object.
            W::Cpp(c, p) => unsafe { (c.cw_is_local)(*p) != 0 },
        }
    }
    fn consumer_active(&self) -> bool {
        match self {
            W::Rust(w) => w.consumer_active(),
            // SAFETY: valid object.
            W::Cpp(c, p) => unsafe { (c.cw_consumer_active)(*p) != 0 },
        }
    }
    fn size(&self) -> (u32, u32) {
        match self {
            W::Rust(w) => (w.width(), w.height()),
            // SAFETY: valid object.
            W::Cpp(c, p) => unsafe { ((c.cw_width)(*p), (c.cw_height)(*p)) },
        }
    }
    /// Ok((w, h)) or Err(hresult).
    fn open_local(&mut self, w: u32, h: u32) -> Result<(u32, u32), i32> {
        match self {
            W::Rust(wr) => wr.open_local(w, h).map_err(|e| e.code().0),
            W::Cpp(c, p) => {
                let (mut ow, mut oh) = (0u32, 0u32);
                // SAFETY: valid object and out pointers.
                let hr = unsafe { (c.cw_open_local)(*p, w, h, &mut ow, &mut oh) };
                if hr < 0 {
                    Err(hr)
                } else {
                    Ok((ow, oh))
                }
            }
        }
    }
    fn close(&mut self) {
        match self {
            W::Rust(w) => w.close(),
            // SAFETY: valid object.
            W::Cpp(c, p) => unsafe { (c.cw_close)(*p) },
        }
    }
    fn write(&mut self, src: &[u8], pitch: usize, w: u32, h: u32) {
        match self {
            W::Rust(wr) => wr.write(src, pitch, w, h),
            // SAFETY: `src` holds a w x h NV12 picture.
            W::Cpp(c, p) => unsafe { (c.cw_write)(*p, src.as_ptr(), pitch as i32, w, h) },
        }
    }
    fn copy_latest(&self, fresh: bool) -> Option<(Vec<u8>, u32, u32)> {
        match self {
            W::Rust(w) => {
                let mut v = Vec::new();
                let r = if fresh {
                    w.copy_latest_fresh(&mut v)
                } else {
                    w.copy_latest(&mut v)
                };
                r.map(|(a, b)| (v, a, b))
            }
            W::Cpp(c, p) => {
                let mut buf = vec![0u8; 4096 * 2304 * 3 / 2];
                let (mut w, mut h) = (0u32, 0u32);
                // SAFETY: valid object, buffer of the stated capacity.
                let n = unsafe {
                    (c.cw_copy_latest)(
                        *p,
                        buf.as_mut_ptr(),
                        buf.len() as i32,
                        &mut w,
                        &mut h,
                        fresh as i32,
                    )
                };
                if n < 0 {
                    None
                } else {
                    buf.truncate(n as usize);
                    Some((buf, w, h))
                }
            }
        }
    }
    fn hold_for(&mut self, ms: u32) {
        match self {
            W::Rust(w) => w.hold_for(ms),
            // SAFETY: valid object.
            W::Cpp(c, p) => unsafe { (c.cw_hold_for)(*p, ms) },
        }
    }
    fn release_hold(&mut self) {
        match self {
            W::Rust(w) => w.release_hold(),
            // SAFETY: valid object.
            W::Cpp(c, p) => unsafe { (c.cw_release_hold)(*p) },
        }
    }
}

impl Drop for W {
    fn drop(&mut self) {
        if let W::Cpp(c, p) = self {
            // SAFETY: freed once.
            unsafe { (c.cw_free)(*p) };
        }
    }
}

enum R {
    Rust(FrameReader),
    Cpp(&'static Cpp, *mut c_void),
}

// SAFETY: the C++ reader is used from one thread at a time in these tests.
unsafe impl Send for R {}

impl R {
    fn new(imp: Impl, c: &'static Cpp, names: &Names) -> R {
        match imp {
            Impl::Rust => R::Rust(FrameReader::with_names(names.clone())),
            // SAFETY: plain constructor.
            Impl::Cpp => R::Cpp(c, unsafe { (c.cr_new)() }),
        }
    }
    /// HRESULT (0 = S_OK).
    fn open(&mut self, w: u32, h: u32, local: bool) -> i32 {
        match self {
            R::Rust(r) => r.open(w, h, local).map_or_else(|e| e.code().0, |_| 0),
            // SAFETY: valid object.
            R::Cpp(c, p) => unsafe { (c.cr_open)(*p, w, h, local as i32) },
        }
    }
    fn open_existing(&mut self, local: bool) -> bool {
        match self {
            R::Rust(r) => r.open_existing(local),
            // SAFETY: valid object.
            R::Cpp(c, p) => unsafe { (c.cr_open_existing)(*p, local as i32) != 0 },
        }
    }
    fn size(&self) -> (u32, u32) {
        match self {
            R::Rust(r) => (r.width(), r.height()),
            // SAFETY: valid object.
            R::Cpp(c, p) => unsafe { ((c.cr_width)(*p), (c.cr_height)(*p)) },
        }
    }
    fn close(&mut self) {
        match self {
            R::Rust(r) => r.close(),
            // SAFETY: valid object.
            R::Cpp(c, p) => unsafe { (c.cr_close)(*p) },
        }
    }
    fn latest_frame(&self) -> i64 {
        match self {
            R::Rust(r) => r.latest_frame(),
            // SAFETY: valid object.
            R::Cpp(c, p) => unsafe { (c.cr_latest_frame)(*p) },
        }
    }
    fn delivered_frame(&self) -> i64 {
        match self {
            R::Rust(r) => r.delivered_frame(),
            // SAFETY: valid object.
            R::Cpp(c, p) => unsafe { (c.cr_delivered_frame)(*p) },
        }
    }
    fn set_active(&mut self, a: bool) {
        match self {
            R::Rust(r) => r.set_active(a),
            // SAFETY: valid object.
            R::Cpp(c, p) => unsafe { (c.cr_set_active)(*p, a as i32) },
        }
    }
    fn fill(&mut self, dst: &mut [u8], pitch: usize, w: u32, h: u32, now: i64) -> bool {
        match self {
            R::Rust(r) => r.fill(dst, pitch, w, h, now),
            // SAFETY: `dst` holds pitch * h * 3 / 2 bytes (checked by the callers).
            R::Cpp(c, p) => unsafe {
                (c.cr_fill)(*p, dst.as_mut_ptr(), pitch as i32, w, h, now) != 0
            },
        }
    }
}

impl Drop for R {
    fn drop(&mut self) {
        if let R::Cpp(c, p) = self {
            // SAFETY: freed once.
            unsafe { (c.cr_free)(*p) };
        }
    }
}

// ---------------------------------------------------------------- helpers

/// A distinct NV12 picture per frame number.
fn pattern(k: u32, w: u32, h: u32) -> Vec<u8> {
    (0..(w * h * 3 / 2) as usize)
        .map(|i| ((i * 7 + k as usize * 13 + (i >> 8)) & 0xFF) as u8)
        .collect()
}

fn fit(src: &[u8], sw: u32, sh: u32, dw: u32, dh: u32) -> Vec<u8> {
    let mut d = vec![0xEEu8; (dw * dh * 3 / 2) as usize];
    nv12_fit_into(src, sw as usize, sw, sh, &mut d, dw as usize, dw, dh);
    d
}

fn no_signal(w: u32, h: u32, now: i64) -> Vec<u8> {
    let mut d = vec![0xEEu8; (w * h * 3 / 2) as usize];
    draw_no_signal(&mut d, w as usize, w, h, now);
    d
}

fn fnv(b: &[u8]) -> u64 {
    b.iter().fold(0xcbf2_9ce4_8422_2325u64, |h, &x| {
        (h ^ x as u64).wrapping_mul(0x100_0000_01b3)
    })
}

fn same(what: &str, a: &[u8], b: &[u8]) {
    if a != b {
        let i = a.iter().zip(b).position(|(x, y)| x != y);
        panic!(
            "{what}: differ (len {} vs {}, first diff at {:?}, hash {:016x} vs {:016x})",
            a.len(),
            b.len(),
            i,
            fnv(a),
            fnv(b)
        );
    }
}

/// Fill into a fresh 0xEE buffer.
fn do_fill(r: &mut R, w: u32, h: u32, now: i64) -> (bool, Vec<u8>) {
    let mut d = vec![0xEEu8; (w * h * 3 / 2) as usize];
    let ok = r.fill(&mut d, w as usize, w, h, now);
    (ok, d)
}

fn now() -> i64 {
    mf_system_time()
}

/// A raw view of a channel's mapping, to poke at the header the way the other side could.
struct Raw {
    h: *mut SharedFrameHeader,
    _handle: HANDLE,
}

impl Raw {
    fn open(name: &str) -> Raw {
        let wide: Vec<u16> = name.encode_utf16().chain(Some(0)).collect();
        // SAFETY: NUL-terminated name; whole-mapping view.
        unsafe {
            let handle = OpenFileMappingW(
                FILE_MAP_READ.0 | FILE_MAP_WRITE.0,
                false,
                PCWSTR(wide.as_ptr()),
            )
            .expect("raw OpenFileMapping");
            let v = MapViewOfFile(handle, FILE_MAP_READ | FILE_MAP_WRITE, 0, 0, 0);
            assert!(!v.Value.is_null());
            Raw {
                h: v.Value as *mut SharedFrameHeader,
                _handle: handle,
            }
        }
    }
    fn create(name: &str, bytes: u64) -> Raw {
        let wide: Vec<u16> = name.encode_utf16().chain(Some(0)).collect();
        // SAFETY: NUL-terminated name; whole-mapping view.
        unsafe {
            let handle = CreateFileMappingW(
                INVALID_HANDLE_VALUE,
                None,
                PAGE_READWRITE,
                (bytes >> 32) as u32,
                bytes as u32,
                PCWSTR(wide.as_ptr()),
            )
            .expect("raw CreateFileMapping");
            let v = MapViewOfFile(handle, FILE_MAP_ALL_ACCESS, 0, 0, 0);
            assert!(!v.Value.is_null());
            Raw {
                h: v.Value as *mut SharedFrameHeader,
                _handle: handle,
            }
        }
    }
    fn hdr(&self) -> &SharedFrameHeader {
        // SAFETY: mapped for the lifetime of `self`, all fields atomic.
        unsafe { &*self.h }
    }
}

const IMPLS: [Impl; 2] = [Impl::Rust, Impl::Cpp];

// ---------------------------------------------------------------- tests

/// sizeof/offsetof of `SharedFrameHeader` and every constant, from C++ and Rust.
#[test]
fn layout_and_constants_equal() {
    let c = need_cpp!();
    let mut buf = vec![0u8; 4096];
    // SAFETY: buffer of the stated capacity.
    let n = unsafe { (c.layout)(buf.as_mut_ptr(), buf.len() as i32) };
    assert!(n > 0);
    let cpp_lines: Vec<String> = String::from_utf8_lossy(&buf[..n as usize])
        .lines()
        .map(str::to_string)
        .collect();
    let rust_lines = layout_lines();
    println!(
        "--- C++ ---\n{}\n--- Rust ---\n{}",
        cpp_lines.join("\n"),
        rust_lines.join("\n")
    );
    assert_eq!(cpp_lines, rust_lines, "SharedFrameHeader layout differs");

    // SAFETY: as above.
    let n = unsafe { (c.constants)(buf.as_mut_ptr(), buf.len() as i32) };
    let cpp_c = String::from_utf8_lossy(&buf[..n as usize]).to_string();
    let rust_c = format!(
        "MAGIC {}\nVERSION {}\nSLOTS {}\nSTALE {}\nLOCAL_MAX_PIXELS {}\nLOCAL_SLOT_BYTES {}\nLOCAL_MAPPING_BYTES {}\nMAPPING_BYTES_1920x1080 {}\nNV12_BYTES_1921x1081 {}\n",
        CAMSTUDIO_MAGIC,
        CAMSTUDIO_VERSION,
        CAMSTUDIO_SLOTS,
        CAMSTUDIO_STALE_100NS,
        CAMSTUDIO_LOCAL_MAX_PIXELS,
        local_slot_bytes(),
        local_mapping_bytes(),
        mapping_bytes(1920, 1080),
        nv12_bytes(1921, 1081)
    );
    println!("{cpp_c}");
    assert_eq!(cpp_c, rust_c, "constants differ");

    // CamStudioClampLocalSize
    for (w, h) in [
        (1920u32, 1080u32),
        (4096, 2304),
        (4097, 2304),
        (7680, 4320),
        (3840, 2160),
        (8192, 8192),
        (0, 5000),
        (5000, 0),
        (10001, 3),
        (6000, 4000),
    ] {
        let (mut a, mut b) = (w, h);
        // SAFETY: valid pointers.
        unsafe { (c.clamp)(&mut a, &mut b) };
        let (mut x, mut y) = (w, h);
        clamp_local_size(&mut x, &mut y);
        assert_eq!((a, b), (x, y), "clamp {w}x{h}");
    }
}

/// The global channel (Windows 11): the reader (media source) creates the mapping, the host
/// writer opens it. All four writer x creator combinations; a reader of the other kind joins.
fn global_scenario(c: &'static Cpp, wi: Impl, ri: Impl) {
    let (_g, names, _s) = scenario(c);
    let tag = format!("global writer={wi:?} reader={ri:?}");
    let (nw, nh) = (64u32, 48u32);
    let mut ra = R::new(ri, c, &names);
    assert_eq!(ra.open(nw, nh, false), 0, "{tag}: RA open");
    // The second reader (other implementation) finds the mapping: the "existing" path.
    let mut rb = R::new(ri.other(), c, &names);
    let rb_open = rb.open_existing(false);
    println!("{tag}: second reader open_existing(global) = {rb_open} (needs an elevated process: the DACL grants only generic read/write to normal users)");
    let mut readers: Vec<&mut R> = vec![&mut ra];
    if rb_open {
        readers.push(&mut rb);
    }

    // The writer cannot open a mapping that is not there yet (checked first on another name below).
    let mut wr = W::new(wi, c, &names);
    wr.use_local(false);
    assert!(!wr.is_open());
    assert!(wr.wait_open(), "{tag}: writer opens");
    assert!(!wr.is_local());
    assert_eq!(wr.size(), (nw, nh), "{tag}: writer sees native size");
    assert!(!wr.consumer_active());
    assert!(
        wr.copy_latest(false).is_none(),
        "{tag}: nothing written yet"
    );

    // readerActive counting.
    readers[0].set_active(true);
    assert!(wr.consumer_active());
    readers[0].set_active(true); // idempotent
    readers[0].set_active(false);
    assert!(!wr.consumer_active());
    readers[0].set_active(true);
    if readers.len() > 1 {
        readers[1].set_active(true);
        readers[0].set_active(false);
        assert!(wr.consumer_active(), "{tag}: two readers, one left");
        readers[0].set_active(true);
        readers[1].set_active(false);
    }

    // No signal (host never wrote): same pattern from every reader, for several destinations.
    for (dw, dh) in [(64u32, 48u32), (100, 60), (48, 64), (16, 16)] {
        let t = now();
        let expect = no_signal(dw, dh, t);
        for r in readers.iter_mut() {
            let (ok, d) = do_fill(r, dw, dh, t);
            assert!(!ok, "{tag}: never wrote -> no signal");
            same(&format!("{tag}: nosignal {dw}x{dh}"), &d, &expect);
        }
    }

    // Frames at the native size: byte-identical, through every destination shape.
    let mut slots_seen = [false; 3];
    for k in 1..=7u32 {
        let src = pattern(k, nw, nh);
        wr.write(&src, nw as usize, nw, nh);
        let t = now();
        for (dw, dh) in [(nw, nh), (100, 60), (48, 64), (32, 24)] {
            let expect = if (dw, dh) == (nw, nh) {
                src.clone()
            } else {
                fit(&src, nw, nh, dw, dh)
            };
            for r in readers.iter_mut() {
                let (ok, d) = do_fill(r, dw, dh, t);
                assert!(ok, "{tag}: frame {k} live");
                same(&format!("{tag}: frame {k} -> {dw}x{dh}"), &d, &expect);
                assert_eq!(r.latest_frame(), k as i64);
                assert_eq!(r.delivered_frame(), k as i64);
            }
        }
        let (copy, cw_, ch_) = wr.copy_latest(false).expect("copy_latest");
        assert_eq!((cw_, ch_), (nw, nh));
        same(&format!("{tag}: copy_latest {k}"), &copy, &src);
        let (copy, _, _) = wr.copy_latest(true).expect("copy_latest_fresh");
        same(&format!("{tag}: copy_latest_fresh {k}"), &copy, &src);
        // Which slot got the frame: record via the raw header.
        let raw = Raw::open(&names.global.mapping_string());
        slots_seen[(raw.hdr().latest.load(Ordering::SeqCst) & 3) as usize] = true;
    }
    assert!(
        slots_seen.iter().filter(|s| **s).count() >= 2,
        "{tag}: the writer rotates slots"
    );

    // A picture of another size is scaled to the mapping's size (nearest neighbour, like the C++).
    let src32 = pattern(100, 32, 24);
    wr.write(&src32, 32, 32, 24);
    let mut scaled = vec![0u8; (nw * nh * 3 / 2) as usize];
    nv12_scale(&src32, 32, 32, 24, &mut scaled, nw as usize, nw, nh);
    let t = now();
    for r in readers.iter_mut() {
        let (ok, d) = do_fill(r, nw, nh, t);
        assert!(ok);
        same(&format!("{tag}: 32x24 written into 64x48"), &d, &scaled);
    }
    // ... and a source with a bigger pitch than width.
    let wide_pitch = 80usize;
    let src = pattern(101, nw, nh);
    let mut padded = vec![0u8; wide_pitch * (nh as usize * 3 / 2)];
    for y in 0..(nh as usize * 3 / 2) {
        padded[y * wide_pitch..y * wide_pitch + nw as usize]
            .copy_from_slice(&src[y * nw as usize..(y + 1) * nw as usize]);
    }
    wr.write(&padded, wide_pitch, nw, nh);
    for r in readers.iter_mut() {
        let (ok, d) = do_fill(r, nw, nh, now());
        assert!(ok);
        same(&format!("{tag}: pitched source"), &d, &src);
    }

    // Staleness (2 s rule), HoldFor / ReleaseHold. The reader gets `now` as a parameter.
    let src = pattern(102, nw, nh);
    wr.write(&src, nw as usize, nw, nh);
    let t0 = now();
    for r in readers.iter_mut() {
        let (ok, d) = do_fill(r, nw, nh, t0 + 15_000_000);
        assert!(ok, "{tag}: 1.5 s old is live");
        same("live 1.5s", &d, &src);
        let (ok, d) = do_fill(r, nw, nh, t0 + 25_000_000);
        assert!(!ok, "{tag}: 2.5 s old is stale");
        same("stale nosignal", &d, &no_signal(nw, nh, t0 + 25_000_000));
    }
    wr.hold_for(10_000);
    for r in readers.iter_mut() {
        let (ok, d) = do_fill(r, nw, nh, now() + 50_000_000);
        assert!(ok, "{tag}: held frame is live 5 s on");
        same("held", &d, &src);
        let (ok, _) = do_fill(r, nw, nh, now() + 110_000_000);
        assert!(!ok, "{tag}: ... but not 11 s on");
    }
    wr.release_hold();
    for r in readers.iter_mut() {
        let (ok, _) = do_fill(r, nw, nh, now() + 30_000_000);
        assert!(!ok, "{tag}: released: stale in 3 s");
    }
    wr.hold_for(10_000); // after a release: nothing
    for r in readers.iter_mut() {
        let (ok, _) = do_fill(r, nw, nh, now() + 30_000_000);
        assert!(!ok, "{tag}: hold after release is ignored");
    }
    wr.write(&src, nw as usize, nw, nh); // a new frame clears `released`
    wr.hold_for(10_000);
    for r in readers.iter_mut() {
        let (ok, _) = do_fill(r, nw, nh, now() + 50_000_000);
        assert!(ok, "{tag}: hold works again after a write");
    }
    wr.release_hold();
    // copy_latest_fresh uses the real clock: fresh now, stale after 2.1 s.
    assert!(wr.copy_latest(true).is_some());
    std::thread::sleep(Duration::from_millis(2150));
    assert!(
        wr.copy_latest(true).is_none(),
        "{tag}: copy_latest_fresh after 2.15 s"
    );
    assert!(
        wr.copy_latest(false).is_some(),
        "{tag}: copy_latest does not care"
    );

    // A raced slot: odd sequence on the latest slot -> the reader tries 4 times, then delivers the
    // possibly torn copy ("beats a grey flash"). Same in both readers.
    let src = pattern(103, nw, nh);
    wr.write(&src, nw as usize, nw, nh);
    let raw = Raw::open(&names.global.mapping_string());
    let latest = raw.hdr().latest.load(Ordering::SeqCst);
    let slot = (latest & 3) as usize;
    raw.hdr().slot_seq[slot].fetch_add(1, Ordering::SeqCst); // odd: "being written"
    for r in readers.iter_mut() {
        let (ok, d) = do_fill(r, nw, nh, now());
        assert!(ok, "{tag}: raced slot still delivers");
        same("raced slot content", &d, &src);
        assert_eq!(r.delivered_frame(), latest >> 2);
        assert_eq!(
            raw.hdr().slot_readers[slot].load(Ordering::SeqCst),
            0,
            "slotReaders back to 0"
        );
    }
    raw.hdr().slot_seq[slot].fetch_add(1, Ordering::SeqCst);

    // The writer avoids the latest slot and slots with readers; with every other slot busy it
    // takes the first non-latest one.
    for _ in 0..3 {
        let latest = raw.hdr().latest.load(Ordering::SeqCst);
        let ls = (latest & 3) as usize;
        let others: Vec<usize> = (0..3).filter(|s| *s != ls).collect();
        raw.hdr().slot_readers[others[0]].store(1, Ordering::SeqCst);
        wr.write(&src, nw as usize, nw, nh);
        let got = (raw.hdr().latest.load(Ordering::SeqCst) & 3) as usize;
        assert_eq!(got, others[1], "{tag}: skips the slot a reader is on");
        raw.hdr().slot_readers[others[0]].store(0, Ordering::SeqCst);

        let latest = raw.hdr().latest.load(Ordering::SeqCst);
        let ls = (latest & 3) as usize;
        let others: Vec<usize> = (0..3).filter(|s| *s != ls).collect();
        raw.hdr().slot_readers[others[0]].store(1, Ordering::SeqCst);
        raw.hdr().slot_readers[others[1]].store(1, Ordering::SeqCst);
        wr.write(&src, nw as usize, nw, nh);
        let got = (raw.hdr().latest.load(Ordering::SeqCst) & 3) as usize;
        assert_eq!(got, others[0], "{tag}: every other slot busy -> first");
        raw.hdr().slot_readers[others[0]].store(0, Ordering::SeqCst);
        raw.hdr().slot_readers[others[1]].store(0, Ordering::SeqCst);
        // Each slot's sequence is even after a write.
        for s in 0..3 {
            assert_eq!(raw.hdr().slot_seq[s].load(Ordering::SeqCst) & 1, 0);
        }
    }

    // Close and reopen the writer.
    wr.close();
    assert!(!wr.is_open());
    wr.write(&src, nw as usize, nw, nh); // no-op
    assert!(wr.wait_open(), "{tag}: reopens");
    for r in readers.iter_mut() {
        r.close();
    }
    assert!(!wr.consumer_active(), "{tag}: readers closed -> not active");
    println!("{tag}: OK");
}

#[test]
fn global_channel_all_combinations() {
    let c = need_cpp!();
    for wi in IMPLS {
        for ri in IMPLS {
            global_scenario(c, wi, ri);
        }
    }
}

/// Windows 10 local channel: the host writer creates the mapping, readers open it.
fn local_scenario(c: &'static Cpp, wi: Impl, ri: Impl) {
    let (_g, names, _s) = scenario(c);
    let tag = format!("local writer={wi:?} reader={ri:?}");
    let (nw, nh) = (64u32, 48u32);

    let mut wr = W::new(wi, c, &names);
    wr.use_local(true);
    assert_eq!(
        wr.open_local(nw, nh).expect("open_local"),
        (nw, nh),
        "{tag}"
    );
    assert!(wr.is_open() && wr.is_local());
    assert_eq!(wr.size(), (nw, nh));

    let mut ra = R::new(ri, c, &names);
    assert!(
        ra.open_existing(true),
        "{tag}: reader finds the local mapping"
    );
    // A reader of the other kind that "creates": finds it existing, with a good magic.
    let mut rb = R::new(ri.other(), c, &names);
    assert_eq!(
        rb.open(1280, 720, true),
        0,
        "{tag}: Open on an existing local mapping"
    );
    assert_eq!(
        rb.size(),
        (nw, nh),
        "{tag}: existing header kept, not re-initialised"
    );

    // A second writer (same kind) joins through TryOpen (checks the local capacity).
    let mut w2 = W::new(wi, c, &names);
    w2.use_local(true);
    assert!(w2.wait_open());
    assert!(w2.is_local());
    // A global-channel writer does not see it.
    let mut wg = W::new(wi, c, &names);
    wg.use_local(false);
    wg.try_open();
    assert!(!wg.is_open());

    ra.set_active(true);
    assert!(wr.consumer_active());
    for k in 1..=5u32 {
        let src = pattern(k, nw, nh);
        wr.write(&src, nw as usize, nw, nh);
        let t = now();
        for r in [&mut ra, &mut rb] {
            for (dw, dh) in [(nw, nh), (120, 90), (48, 64)] {
                let expect = if (dw, dh) == (nw, nh) {
                    src.clone()
                } else {
                    fit(&src, nw, nh, dw, dh)
                };
                let (ok, d) = do_fill(r, dw, dh, t);
                assert!(ok);
                same(&format!("{tag}: frame {k} -> {dw}x{dh}"), &d, &expect);
            }
        }
        let (copy, _, _) = wr.copy_latest(false).unwrap();
        same(&format!("{tag}: copy_latest"), &copy, &src);
    }
    // The second writer's frame is seen too (frame numbers continue).
    let src = pattern(77, nw, nh);
    w2.write(&src, nw as usize, nw, nh);
    let (ok, d) = do_fill(&mut ra, nw, nh, now());
    assert!(ok);
    same(&format!("{tag}: second writer"), &d, &src);
    assert_eq!(ra.latest_frame(), 6);

    // Resize: refused while a reader streams, taken when none does (the picture is dropped).
    assert_eq!(
        wr.open_local(32, 24).unwrap(),
        (nw, nh),
        "{tag}: size kept while a reader streams"
    );
    ra.set_active(false);
    assert_eq!(
        wr.open_local(32, 24).unwrap(),
        (32, 24),
        "{tag}: size changes with no reader"
    );
    assert_eq!(ra.size(), (32, 24));
    assert_eq!(ra.latest_frame(), 0, "{tag}: latest reset by the resize");
    let t = now();
    let (ok, d) = do_fill(&mut ra, 32, 24, t);
    assert!(!ok);
    same("after resize: no signal", &d, &no_signal(32, 24, t));
    let src = pattern(200, 32, 24);
    wr.write(&src, 32, 32, 24);
    let (ok, d) = do_fill(&mut ra, 32, 24, now());
    assert!(ok);
    same(&format!("{tag}: after resize"), &d, &src);
    // A bigger picture than the new size is scaled down into it by the writer.
    let big = pattern(201, 64, 48);
    wr.write(&big, 64, 64, 48);
    let mut sc = vec![0u8; 32 * 24 * 3 / 2];
    nv12_scale(&big, 64, 64, 48, &mut sc, 32, 32, 24);
    let (_, d) = do_fill(&mut ra, 32, 24, now());
    same(&format!("{tag}: downscaled by the writer"), &d, &sc);

    // Oversize pictures are clamped (4K either way up fits).
    let got = wr.open_local(7680, 4320).unwrap();
    let (mut ex, mut ey) = (7680, 4320);
    clamp_local_size(&mut ex, &mut ey);
    assert_eq!(got, (ex, ey), "{tag}: clamped size");
    println!("{tag}: 7680x4320 -> {}x{}", got.0, got.1);
    let src = pattern(300, got.0, got.1);
    wr.write(&src, got.0 as usize, got.0, got.1);
    let (ok, d) = do_fill(&mut ra, 320, 180, now());
    assert!(ok);
    same(
        &format!("{tag}: clamped picture shown"),
        &d,
        &fit(&src, got.0, got.1, 320, 180),
    );
}

#[test]
fn local_channel_all_combinations() {
    let c = need_cpp!();
    for wi in IMPLS {
        for ri in IMPLS {
            local_scenario(c, wi, ri);
        }
    }
}

/// The reader creates the local mapping (header sized by the clamp), then the host adopts it.
#[test]
fn local_channel_reader_creates() {
    let c = need_cpp!();
    for ri in IMPLS {
        for wi in IMPLS {
            {
                let (_g, names, _s) = scenario(c);
                let tag = format!("local reader-creates reader={ri:?} writer={wi:?}");
                let mut r = R::new(ri, c, &names);
                assert_eq!(r.open(100, 60, true), 0);
                assert_eq!(r.size(), (100, 60));
                let mut w = W::new(wi, c, &names);
                w.use_local(true);
                assert_eq!(
                    w.open_local(64, 48).unwrap(),
                    (64, 48),
                    "{tag}: host resizes (no reader active)"
                );
                assert_eq!(r.size(), (64, 48));
                r.set_active(true);
                assert_eq!(
                    w.open_local(100, 60).unwrap(),
                    (64, 48),
                    "{tag}: not while streaming"
                );
            }
            {
                // An oversized native size is clamped by the creating reader.
                let (_g, names, _s) = scenario(c);
                let mut r2 = R::new(ri, c, &names);
                assert_eq!(r2.open(7680, 4320, true), 0);
                let (mut ex, mut ey) = (7680, 4320);
                clamp_local_size(&mut ex, &mut ey);
                assert_eq!(r2.size(), (ex, ey), "creator clamps");
            }
        }
    }
}

/// Mappings that are not ours: a bad magic, another version, another capacity.
#[test]
fn foreign_mappings_are_refused() {
    let c = need_cpp!();
    for wi in IMPLS {
        // TryOpen (global): bad magic, bad version, bad slot count, and for local a wrong capacity.
        for (what, local) in [
            ("bad magic", false),
            ("bad version", false),
            ("bad slots", false),
            ("bad magic local", true),
            ("wrong capacity", true),
            ("bad version local", true),
        ] {
            let (_g, names, _s) = scenario(c);
            let ch = names.channel(local);
            let raw = Raw::create(&ch.mapping_string(), local_mapping_bytes());
            let h = raw.hdr();
            h.init(
                64,
                48,
                if local {
                    local_slot_bytes()
                } else {
                    nv12_bytes(64, 48)
                },
            );
            match what {
                "bad magic" | "bad magic local" => h.magic.store(0x1234, Ordering::SeqCst),
                "bad version" | "bad version local" => h.version.store(99, Ordering::SeqCst),
                "bad slots" => h.slots.store(5, Ordering::SeqCst),
                "wrong capacity" => h.frame_bytes.store(1000, Ordering::SeqCst),
                _ => unreachable!(),
            }
            let mut w = W::new(wi, c, &names);
            w.use_local(local);
            w.try_open();
            assert!(!w.is_open(), "{wi:?}: TryOpen must refuse {what}");
            println!("{wi:?}: TryOpen refuses {what}");
            // OpenLocal on a mapping of another layout (not for a bad magic: it re-initialises it).
            if local && what != "bad magic local" {
                let mut w = W::new(wi, c, &names);
                let r = w.open_local(64, 48);
                assert_eq!(
                    r,
                    Err(0x8000FFFFu32 as i32),
                    "{wi:?}: OpenLocal on {what} -> E_UNEXPECTED"
                );
                println!("{wi:?}: OpenLocal on {what} -> E_UNEXPECTED");
            }
            // OpenExisting (reader) refuses bad magic / version / slots.
            for ri in IMPLS {
                if what == "wrong capacity" {
                    continue; // the reader does not check the capacity
                }
                let mut r = R::new(ri, c, &names);
                assert!(
                    !r.open_existing(local),
                    "{ri:?}: OpenExisting must refuse {what}"
                );
            }
        }
    }
}

/// `OpenLocal` finding a mapping whose magic never appears: after waiting it re-initialises it.
#[test]
fn open_local_reinitialises_uninitialised_mapping() {
    let c = need_cpp!();
    for wi in IMPLS {
        let (_g, names, _s) = scenario(c);
        let raw = Raw::create(&names.local.mapping_string(), local_mapping_bytes()); // all zero: magic 0
        let mut w = W::new(wi, c, &names);
        let t = Instant::now();
        assert_eq!(w.open_local(64, 48).unwrap(), (64, 48));
        println!(
            "{wi:?}: OpenLocal on a never-initialised mapping took {:?}",
            t.elapsed()
        );
        assert_eq!(raw.hdr().magic.load(Ordering::SeqCst), CAMSTUDIO_MAGIC);
        assert_eq!(
            raw.hdr().frame_bytes.load(Ordering::SeqCst),
            local_slot_bytes()
        );
    }
}

// ---------------------------------------------------------------- stress: the seqlock

struct StressResult {
    frames: u64,
    torn: u64,
    no_signal: u64,
    non_monotonic: u64,
}

/// A writer thread (`wi`) publishes 1280x720 frames of one repeated byte each, `fps` per second
/// (0 = as fast as it can) for `secs`; two reader threads (one per implementation) read as fast
/// as they can and count frames that are not uniform (a torn copy).
fn stress(
    c: &'static Cpp,
    wi: Impl,
    fps: u32,
    secs: u64,
    local: bool,
) -> Vec<(String, StressResult)> {
    let (_g, names, _s) = scenario(c);
    let (w, h) = (1280u32, 720u32);
    // Global: the reader creates the channel (as Frame Server does) and the host writer opens it
    // (one reader: a second one cannot open the DACL'd mapping without elevation). Local: the
    // host creates it and two readers, one of each kind, open it.
    let mut first = R::new(wi.other(), c, &names);
    let mut wr = W::new(wi, c, &names);
    if local {
        wr.use_local(true);
        assert_eq!(wr.open_local(w, h).unwrap(), (w, h));
        assert!(first.open_existing(true));
    } else {
        assert_eq!(first.open(w, h, false), 0);
        wr.use_local(false);
        assert!(wr.wait_open());
    }
    let mut second = R::new(wi, c, &names);
    let second_ok = local && second.open_existing(true);

    let stop = Arc::new(AtomicBool::new(false));
    let stop_w = stop.clone();
    let writer = std::thread::spawn(move || {
        let mut frame = vec![0u8; (w * h * 3 / 2) as usize];
        let mut k = 0u32;
        let interval = if fps > 0 {
            Some(Duration::from_secs_f64(1.0 / fps as f64))
        } else {
            None
        };
        let mut next = Instant::now();
        while !stop_w.load(Ordering::Relaxed) {
            k += 1;
            frame.fill((k % 250 + 1) as u8);
            wr.write(&frame, w as usize, w, h);
            if let Some(iv) = interval {
                next += iv;
                while Instant::now() < next {
                    std::thread::yield_now();
                }
            }
        }
        k
    });

    let mut readers = Vec::new();
    let mut rs: Vec<(String, R)> = vec![(format!("{:?}", wi.other()), first)];
    if second_ok {
        rs.push((format!("{wi:?}"), second));
    }
    for (name, mut r) in rs {
        let stop_r = stop.clone();
        readers.push(std::thread::spawn(move || {
            r.set_active(true);
            let mut res = StressResult {
                frames: 0,
                torn: 0,
                no_signal: 0,
                non_monotonic: 0,
            };
            let mut dst = vec![0u8; (w * h * 3 / 2) as usize];
            let mut last = 0i64;
            while !stop_r.load(Ordering::Relaxed) {
                if r.latest_frame() == r.delivered_frame() {
                    std::thread::yield_now();
                    continue;
                }
                let ok = r.fill(&mut dst, w as usize, w, h, mf_system_time());
                if !ok {
                    res.no_signal += 1;
                    continue;
                }
                res.frames += 1;
                let first = dst[0];
                if dst.iter().any(|b| *b != first) {
                    res.torn += 1;
                }
                let d = r.delivered_frame();
                if d < last {
                    res.non_monotonic += 1;
                }
                last = d;
            }
            r.set_active(false);
            (name, res)
        }));
    }
    std::thread::sleep(Duration::from_secs(secs));
    stop.store(true, Ordering::Relaxed);
    let written = writer.join().expect("writer thread");
    let out: Vec<(String, StressResult)> = readers
        .into_iter()
        .map(|t| t.join().expect("reader thread"))
        .collect();
    println!(
        "stress writer={wi:?} fps={fps} local={local}: wrote {written} frames, readers: {}",
        out.len()
    );
    out
}

#[test]
fn seqlock_stress_paced_100fps_both_directions() {
    let c = need_cpp!();
    for local in [false, true] {
        for wi in IMPLS {
            let res = stress(c, wi, 100, 3, local);
            assert_eq!(res.len(), if local { 2 } else { 1 });
            for (name, r) in &res {
                println!(
                    "  writer={wi:?} local={local} reader={name}: frames={} torn={} nosignal={} non-monotonic={}",
                    r.frames, r.torn, r.no_signal, r.non_monotonic
                );
                assert!(r.frames > 100, "reader {name} saw only {} frames", r.frames);
                assert_eq!(
                    r.torn, 0,
                    "torn frames seen by reader {name} with writer {wi:?} at 100 fps"
                );
                assert_eq!(r.no_signal, 0);
                assert_eq!(r.non_monotonic, 0);
            }
        }
    }
}

/// As fast as the writer can go (hundreds of frames per second of 1.4 MB): the C++ reader itself
/// may deliver a torn "last resort" copy here; the point is that Rust and C++ behave alike.
#[test]
fn seqlock_stress_unpaced_report() {
    let c = need_cpp!();
    for local in [false, true] {
        for wi in IMPLS {
            let res = stress(c, wi, 0, 3, local);
            for (name, r) in &res {
                println!(
                    "  UNPACED writer={wi:?} local={local} reader={name}: frames={} torn={} nosignal={} non-monotonic={}",
                    r.frames, r.torn, r.no_signal, r.non_monotonic
                );
                assert!(r.frames > 100);
                assert_eq!(r.non_monotonic, 0);
            }
        }
    }
}
