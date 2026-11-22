//! A stand-in for the Windows Frame Server, to compare a media-source DLL with another one
//! without registering anything (no admin, no COM registration, the real camera is untouched).
//!
//! `fs_sim <dll> <report-file> <timing-file>` loads the DLL with `LoadLibraryW`, calls its
//! `DllGetClassObject` -> `IClassFactory` -> `IMFActivate` -> `ActivateObject` -> `IMFMediaSource`
//! and drives the source the way Frame Server does: characteristics, presentation descriptor,
//! media types, `Start` with each format, events, `RequestSample`, `Stop`, `Shutdown`, while a
//! `niwcam_shm::FrameWriter` (on the names in `NIWCAM_TEST_SUFFIX`) feeds it frames.
//!
//! The report file holds only deterministic lines (HRESULTs, attributes, media types, sample
//! hashes); run it for the C++ DLL and the Rust DLL and diff the two files. Lines starting with `#`
//! are informational and are not diffed. The timing file holds the pacing statistics (`T` lines).
//! The scenarios also check what they can against the expected picture (the shared-memory frame,
//! fitted by `niwcam-core`) and say `FAIL` for anything that is wrong on its own.

// Test tooling, not shipped: COM calls in bulk (safety comments per scenario block), and a few
// framework-typed lints that do not matter here.
#![allow(
    clippy::too_many_arguments,
    clippy::missing_transmute_annotations,
    clippy::arc_with_non_send_sync,
    clippy::undocumented_unsafe_blocks,
    clippy::redundant_closure
)]

use std::ffi::c_void;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use niwcam_core::nv12::nv12_fit_into;
use niwcam_shm::{draw_no_signal, mf_system_time, FrameWriter, Names};
use windows::core::{implement, Interface, Ref, GUID, HRESULT, PCWSTR};
use windows::Win32::Foundation::{E_NOINTERFACE, HANDLE, HMODULE, S_FALSE, S_OK};
use windows::Win32::Media::KernelStreaming::KSIDENTIFIER;
use windows::Win32::Media::MediaFoundation::*;
use windows::Win32::System::Com::StructuredStorage::PROPVARIANT;
use windows::Win32::System::Com::{CoInitializeEx, IClassFactory, COINIT_MULTITHREADED};
use windows::Win32::System::LibraryLoader::{GetProcAddress, LoadLibraryW};
use windows::Win32::System::Memory::{
    MapViewOfFile, OpenFileMappingW, UnmapViewOfFile, FILE_MAP_READ, FILE_MAP_WRITE,
    MEMORY_MAPPED_VIEW_ADDRESS,
};
use windows::Win32::System::Threading::{CreateEventW, SetEvent, WaitForSingleObject};

// ---------------------------------------------------------------- report plumbing

struct Rep {
    lines: Vec<String>,
    fails: Vec<String>,
}

impl Rep {
    fn line(&mut self, s: impl Into<String>) {
        self.lines.push(s.into());
    }
    fn info(&mut self, s: impl AsRef<str>) {
        self.lines.push(format!("# {}", s.as_ref()));
    }
    fn check(&mut self, ok: bool, what: impl AsRef<str>) {
        if !ok {
            let m = format!("FAIL: {}", what.as_ref());
            self.fails.push(m.clone());
            self.lines.push(m);
        }
    }
}

fn hr<T>(r: &windows::core::Result<T>) -> String {
    match r {
        Ok(_) => "S_OK".to_string(),
        Err(e) => format!("0x{:08X}", e.code().0 as u32),
    }
}

fn hrv(h: HRESULT) -> String {
    format!("0x{:08X}", h.0 as u32)
}

fn gs(g: &GUID) -> String {
    format!("{g:?}")
}

fn fnv(b: &[u8]) -> u64 {
    b.iter().fold(0xcbf2_9ce4_8422_2325u64, |h, &x| {
        (h ^ x as u64).wrapping_mul(0x100_0000_01b3)
    })
}

fn ev_name(t: u32) -> String {
    let n = match t as i32 {
        x if x == MEError.0 => "MEError",
        x if x == MESourceStarted.0 => "MESourceStarted",
        x if x == MESourceStopped.0 => "MESourceStopped",
        x if x == MENewStream.0 => "MENewStream",
        x if x == MEUpdatedStream.0 => "MEUpdatedStream",
        x if x == MEStreamStarted.0 => "MEStreamStarted",
        x if x == MEStreamStopped.0 => "MEStreamStopped",
        x if x == MEMediaSample.0 => "MEMediaSample",
        _ => "?",
    };
    format!("{n}({t})")
}

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

// ---------------------------------------------------------------- attributes dump

fn dump_attrs(attrs: &IMFAttributes) -> Vec<String> {
    let mut out = Vec::new();
    // SAFETY: valid attribute store; out parameters are locals.
    unsafe {
        let n = attrs.GetCount().unwrap_or(0);
        for i in 0..n {
            let mut key = GUID::zeroed();
            let mut pv = PROPVARIANT::default();
            if attrs.GetItemByIndex(i, &mut key, Some(&mut pv)).is_err() {
                continue;
            }
            let ty = attrs.GetItemType(&key).map(|t| t.0).unwrap_or(-1);
            let val = match ty {
                x if x == MF_ATTRIBUTE_UINT32.0 => {
                    format!("u32 {}", attrs.GetUINT32(&key).unwrap_or(0))
                }
                x if x == MF_ATTRIBUTE_UINT64.0 => {
                    let v = attrs.GetUINT64(&key).unwrap_or(0);
                    format!("u64 {} ({}x{})", v, v >> 32, v & 0xFFFF_FFFF)
                }
                x if x == MF_ATTRIBUTE_GUID.0 => {
                    format!("guid {}", gs(&attrs.GetGUID(&key).unwrap_or_default()))
                }
                x if x == MF_ATTRIBUTE_DOUBLE.0 => {
                    format!("f64 {}", attrs.GetDouble(&key).unwrap_or(0.0))
                }
                x if x == MF_ATTRIBUTE_STRING.0 => "string".to_string(),
                x if x == MF_ATTRIBUTE_BLOB.0 => {
                    format!("blob {}", attrs.GetBlobSize(&key).unwrap_or(0))
                }
                x if x == MF_ATTRIBUTE_IUNKNOWN.0 => "iunknown".to_string(),
                _ => format!("type {ty}"),
            };
            out.push(format!("  {} = {}", gs(&key), val));
        }
    }
    out.sort();
    out
}

// ---------------------------------------------------------------- the DLL

type GetClassObject =
    unsafe extern "system" fn(*const GUID, *const GUID, *mut *mut c_void) -> HRESULT;
type CanUnload = unsafe extern "system" fn() -> HRESULT;

struct Dll {
    get_class_object: GetClassObject,
    can_unload: CanUnload,
    has_register: bool,
    has_unregister: bool,
}

fn load(path: &str) -> Dll {
    let wide: Vec<u16> = path.encode_utf16().chain(Some(0)).collect();
    // SAFETY: loading the DLL under test; NUL-terminated path.
    let lib: HMODULE = unsafe { LoadLibraryW(PCWSTR(wide.as_ptr())) }.expect("LoadLibraryW");
    let sym = |n: &str| {
        let c = format!("{n}\0");
        // SAFETY: valid module, NUL-terminated name.
        unsafe { GetProcAddress(lib, windows::core::PCSTR(c.as_ptr())) }
    };
    let gco = sym("DllGetClassObject").expect("DllGetClassObject export");
    let cun = sym("DllCanUnloadNow").expect("DllCanUnloadNow export");
    Dll {
        // SAFETY: the exports have exactly these signatures.
        get_class_object: unsafe { std::mem::transmute::<_, GetClassObject>(gco) },
        // SAFETY: as above.
        can_unload: unsafe { std::mem::transmute::<_, CanUnload>(cun) },
        has_register: sym("DllRegisterServer").is_some(),
        has_unregister: sym("DllUnregisterServer").is_some(),
    }
}

const CLSID_VCAM: GUID = GUID::from_u128(0x72f5b2fd_c48e_449b_95ce_e8f13bf200cb);
const ATTR_NATIVE_SIZE: GUID = GUID::from_u128(0x7274f18f_32c3_4935_a7df_ae15ffc58fdc);
const ATTR_NATIVE_FPS: GUID = GUID::from_u128(0xa653111f_e52f_433f_828f_0ac165d965f2);

fn factory(dll: &Dll) -> IClassFactory {
    let mut raw: *mut c_void = std::ptr::null_mut();
    // SAFETY: valid out pointer; the DLL returns a counted IClassFactory.
    let h = unsafe { (dll.get_class_object)(&CLSID_VCAM, &IClassFactory::IID, &mut raw) };
    assert!(h.is_ok(), "DllGetClassObject {h:?}");
    // SAFETY: a counted IClassFactory pointer.
    unsafe { IClassFactory::from_raw(raw) }
}

fn set_native(act: &IMFActivate, w: u32, h: u32, num: u32, den: u32) {
    // SAFETY: valid attribute store.
    unsafe {
        act.SetUINT64(&ATTR_NATIVE_SIZE, ((w as u64) << 32) | h as u64)
            .unwrap();
        act.SetUINT64(&ATTR_NATIVE_FPS, ((num as u64) << 32) | den as u64)
            .unwrap();
    }
}

fn new_activate(dll: &Dll, w: u32, h: u32, num: u32, den: u32) -> IMFActivate {
    let f = factory(dll);
    // SAFETY: valid factory.
    let act: IMFActivate =
        unsafe { f.CreateInstance(None::<&windows::core::IUnknown>) }.expect("CreateInstance");
    set_native(&act, w, h, num, den);
    act
}

// ---------------------------------------------------------------- events

fn wait_event(g: &IMFMediaEventGenerator, ms: u64) -> Option<IMFMediaEvent> {
    let t = Instant::now();
    loop {
        // SAFETY: valid generator.
        match unsafe { g.GetEvent(MF_EVENT_FLAG_NO_WAIT) } {
            Ok(e) => return Some(e),
            Err(e) if e.code() == MF_E_NO_EVENTS_AVAILABLE => {
                if t.elapsed() > Duration::from_millis(ms) {
                    return None;
                }
                std::thread::sleep(Duration::from_millis(1));
            }
            Err(_) => return None,
        }
    }
}

fn describe(e: &IMFMediaEvent) -> String {
    // SAFETY: valid event.
    unsafe {
        let t = e.GetType().unwrap_or(0);
        let st = e.GetStatus().map(|h| hrv(h)).unwrap_or_else(|_| "?".into());
        let pv = e.GetValue().unwrap_or_default();
        format!("{} status={} vt={}", ev_name(t), st, pv.vt().0)
    }
}

/// Reads every queued event (none are waited for).
fn drain(g: &IMFMediaEventGenerator) -> Vec<(u32, IMFMediaEvent)> {
    let mut v = Vec::new();
    // SAFETY: valid generator.
    while let Ok(e) = unsafe { g.GetEvent(MF_EVENT_FLAG_NO_WAIT) } {
        let t = unsafe { e.GetType() }.unwrap_or(0);
        v.push((t, e));
    }
    v
}

// ---------------------------------------------------------------- samples

struct SampleInfo {
    time: i64,
    duration: i64,
    tight: Vec<u8>,
    pitch: i32,
    cur_len: u32,
    buffers: u32,
    token_ok: bool,
}

fn read_sample(
    ev: &IMFMediaEvent,
    w: u32,
    h: u32,
    token: &windows::core::IUnknown,
) -> Option<SampleInfo> {
    // SAFETY: valid event/sample/buffer; the locked buffer holds pitch * h * 3 / 2 bytes.
    unsafe {
        if ev.GetType().ok()? != MEMediaSample.0 as u32 {
            return None;
        }
        let pv = ev.GetValue().ok()?;
        let unk = windows::core::IUnknown::try_from(&pv).ok()?;
        let sample: IMFSample = unk.cast().ok()?;
        let time = sample.GetSampleTime().ok()?;
        let duration = sample.GetSampleDuration().ok()?;
        let buffers = sample.GetBufferCount().ok()?;
        let buf = sample.GetBufferByIndex(0).ok()?;
        let cur_len = buf.GetCurrentLength().ok()?;
        let b2: IMF2DBuffer2 = buf.cast().ok()?;
        let (mut p0, mut pitch) = (std::ptr::null_mut::<u8>(), 0i32);
        b2.Lock2D(&mut p0, &mut pitch).ok()?;
        let mut tight = Vec::with_capacity((w * h * 3 / 2) as usize);
        for y in 0..h as usize {
            tight.extend_from_slice(std::slice::from_raw_parts(
                p0.offset(y as isize * pitch as isize),
                w as usize,
            ));
        }
        let uv = p0.offset(h as isize * pitch as isize);
        for y in 0..h as usize / 2 {
            tight.extend_from_slice(std::slice::from_raw_parts(
                uv.offset(y as isize * pitch as isize),
                w as usize,
            ));
        }
        let _ = b2.Unlock2D();
        let got: windows::core::Result<windows::core::IUnknown> =
            sample.GetUnknown(&MFSampleExtension_Token);
        let token_ok = got.map(|t| t.as_raw() == token.as_raw()).unwrap_or(false);
        Some(SampleInfo {
            time,
            duration,
            tight,
            pitch,
            cur_len,
            buffers,
            token_ok,
        })
    }
}

/// Requests one sample on `stream` and waits for it.
fn request_one(
    stream: &IMFMediaStream2,
    w: u32,
    h: u32,
    token: &windows::core::IUnknown,
) -> windows::core::Result<Option<SampleInfo>> {
    // SAFETY: valid stream.
    unsafe { stream.RequestSample(token) }?;
    let gen: IMFMediaEventGenerator = stream.cast()?;
    let deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < deadline {
        if let Some(e) = wait_event(&gen, 200) {
            if let Some(s) = read_sample(&e, w, h, token) {
                return Ok(Some(s));
            }
        }
    }
    Ok(None)
}

// ---------------------------------------------------------------- IMFAsyncCallback for BeginGetEvent

#[implement(IMFAsyncCallback)]
struct Callback {
    done: HANDLE,
    result: Arc<Mutex<Option<IMFAsyncResult>>>,
}

impl IMFAsyncCallback_Impl for Callback_Impl {
    fn GetParameters(&self, _flags: *mut u32, _queue: *mut u32) -> windows::core::Result<()> {
        Err(windows::Win32::Foundation::E_NOTIMPL.into())
    }
    fn Invoke(&self, result: Ref<IMFAsyncResult>) -> windows::core::Result<()> {
        *self.result.lock().unwrap() = result.cloned();
        // SAFETY: valid event handle.
        unsafe { SetEvent(self.done) }
    }
}

// ---------------------------------------------------------------- scenarios

/// Driver state shared by the scenarios.
struct Ctx {
    dll: Dll,
    names: Names,
}

fn formats_expected(w: u32, h: u32, num: u32, den: u32) -> usize {
    // only to size loops; the formats themselves are compared through the report
    let _ = (w, h, num, den);
    0
}

fn start_with_format(
    source: &IMFMediaSource,
    pd: &IMFPresentationDescriptor,
    index: u32,
) -> windows::core::Result<(IMFMediaType, windows::core::Result<()>)> {
    // SAFETY: valid descriptors; the type handler belongs to our clone of the descriptor.
    unsafe {
        let mut selected = windows::core::BOOL(0);
        let mut sd: Option<IMFStreamDescriptor> = None;
        pd.GetStreamDescriptorByIndex(0, &mut selected, &mut sd)?;
        let sd = sd.unwrap();
        let handler = sd.GetMediaTypeHandler()?;
        let ty = handler.GetMediaTypeByIndex(index)?;
        handler.SetCurrentMediaType(&ty)?;
        pd.SelectStream(0)?;
        let r = source.Start(pd, std::ptr::null(), &PROPVARIANT::default());
        Ok((ty, r))
    }
}

fn type_size(ty: &IMFMediaType) -> (u32, u32, u32, u32) {
    // SAFETY: valid type.
    unsafe {
        let a: IMFAttributes = ty.cast().unwrap();
        let s = a.GetUINT64(&MF_MT_FRAME_SIZE).unwrap();
        let r = a.GetUINT64(&MF_MT_FRAME_RATE).unwrap();
        ((s >> 32) as u32, s as u32, (r >> 32) as u32, r as u32)
    }
}

/// The main scenario: everything a Frame Server does, with a live writer.
fn scenario_main(ctx: &Ctx, rep: &mut Rep) {
    let (nw, nh, fnum, fden) = (1280u32, 720u32, 60u32, 1u32);
    rep.line("== main scenario: native 1280x720@60");
    let dll = &ctx.dll;

    // ---- exports and class object
    rep.line(format!(
        "exports: DllRegisterServer={} DllUnregisterServer={}",
        dll.has_register, dll.has_unregister
    ));
    // SAFETY: calling the DLL's exports with valid (or deliberately null) arguments.
    unsafe {
        rep.line(format!("DllCanUnloadNow = {}", hrv((dll.can_unload)())));
        let mut raw: *mut c_void = std::ptr::null_mut();
        rep.line(format!(
            "DllGetClassObject(ppv=null) = {}",
            hrv((dll.get_class_object)(
                &CLSID_VCAM,
                &IClassFactory::IID,
                std::ptr::null_mut()
            ))
        ));
        let bad = GUID::from_u128(0x12345678_1234_1234_1234_123456789abc);
        rep.line(format!(
            "DllGetClassObject(wrong clsid) = {}",
            hrv((dll.get_class_object)(&bad, &IClassFactory::IID, &mut raw))
        ));
        rep.line(format!("  ppv after wrong clsid null = {}", raw.is_null()));
        let h = (dll.get_class_object)(&CLSID_VCAM, &IMFAttributes::IID, &mut raw);
        rep.line(format!(
            "DllGetClassObject(riid=IMFAttributes) = {}",
            hrv(h)
        ));
        rep.line(format!("  ppv null = {}", raw.is_null()));
        let h = (dll.get_class_object)(&CLSID_VCAM, &windows::core::IUnknown::IID, &mut raw);
        rep.line(format!("DllGetClassObject(riid=IUnknown) = {}", hrv(h)));
        if h.is_ok() {
            drop(windows::core::IUnknown::from_raw(raw));
        }
    }

    // ---- class factory
    let f = factory(dll);
    // SAFETY: valid factory; deliberately odd arguments go through the raw vtable.
    unsafe {
        let outer: windows::core::IUnknown = f.cast().unwrap();
        let mut raw: *mut c_void = std::ptr::null_mut();
        let vt = Interface::vtable(&f);
        rep.line(format!(
            "CreateInstance(outer) = {}",
            hrv((vt.CreateInstance)(
                f.as_raw(),
                outer.as_raw(),
                &IMFActivate::IID,
                &mut raw
            ))
        ));
        rep.line(format!("  ppv null = {}", raw.is_null()));
        rep.line(format!(
            "CreateInstance(ppv=null) = {}",
            hrv((vt.CreateInstance)(
                f.as_raw(),
                std::ptr::null_mut(),
                &IMFActivate::IID,
                std::ptr::null_mut()
            ))
        ));
        let h = (vt.CreateInstance)(
            f.as_raw(),
            std::ptr::null_mut(),
            &IMFMediaSource::IID,
            &mut raw,
        );
        rep.line(format!("CreateInstance(riid=IMFMediaSource) = {}", hrv(h)));
        rep.line(format!("LockServer(true) = {}", hr(&f.LockServer(true))));
        rep.line(format!("LockServer(false) = {}", hr(&f.LockServer(false))));
    }

    // ---- the activation object and its attribute store
    let act = new_activate(dll, nw, nh, fnum, fden);
    // SAFETY: valid activation object.
    unsafe {
        for (name, iid) in [
            ("IMFAttributes", IMFAttributes::IID),
            ("IMFActivate", IMFActivate::IID),
            ("IUnknown", windows::core::IUnknown::IID),
            ("IMFMediaSource", IMFMediaSource::IID),
            ("IMFMediaEventGenerator", IMFMediaEventGenerator::IID),
        ] {
            let mut raw: *mut c_void = std::ptr::null_mut();
            let h = act.query(&iid, &mut raw);
            rep.line(format!("Activate QI {name} = {}", hrv(h)));
            if h.is_ok() {
                drop(windows::core::IUnknown::from_raw(raw));
            }
        }
        // agile / inspectable are an artefact of the framework, not of our code: informational
        for (name, iid) in [
            ("IInspectable", windows::core::IInspectable::IID),
            (
                "IAgileObject",
                GUID::from_u128(0x94ea2b94_e9cc_49e0_c0ff_ee64ca8f5b90),
            ),
        ] {
            let mut raw: *mut c_void = std::ptr::null_mut();
            let h = act.query(&iid, &mut raw);
            rep.info(format!("Activate QI {name} = {}", hrv(h)));
            if h.is_ok() {
                drop(windows::core::IUnknown::from_raw(raw));
            }
        }
        let a: IMFAttributes = act.cast().unwrap();
        rep.line(format!(
            "Activate attrs GetCount = {:?}",
            a.GetCount().map_err(|e| hrv(e.code()))
        ));
        for l in dump_attrs(&a) {
            rep.line(format!("Activate attr{l}"));
        }
        let missing = GUID::from_u128(0xdeadbeef_0000_0000_0000_000000000001);
        rep.line(format!(
            "GetUINT32(missing) = {}",
            hr(&a.GetUINT32(&missing))
        ));
        rep.line(format!(
            "GetItemType(missing) = {}",
            hr(&a.GetItemType(&missing))
        ));
        // every attribute type through the delegation
        let k = |n: u128| GUID::from_u128(0xabcdef00_0000_0000_0000_000000000000 | n);
        a.SetUINT32(&k(1), 7).unwrap();
        a.SetDouble(&k(2), 2.5).unwrap();
        a.SetGUID(&k(3), &CLSID_VCAM).unwrap();
        a.SetString(&k(4), windows::core::w!("hello")).unwrap();
        a.SetBlob(&k(5), &[1, 2, 3, 4, 5]).unwrap();
        a.SetUnknown(&k(6), &a).unwrap();
        rep.line(format!(
            "after sets: GetCount = {:?}",
            a.GetCount().map_err(|e| hrv(e.code()))
        ));
        rep.line(format!(
            "GetUINT32 = {:?}",
            a.GetUINT32(&k(1)).map_err(|e| hrv(e.code()))
        ));
        rep.line(format!(
            "GetDouble = {:?}",
            a.GetDouble(&k(2)).map_err(|e| hrv(e.code()))
        ));
        rep.line(format!(
            "GetGUID = {:?}",
            a.GetGUID(&k(3)).map(|g| gs(&g)).map_err(|e| hrv(e.code()))
        ));
        rep.line(format!(
            "GetStringLength = {:?}",
            a.GetStringLength(&k(4)).map_err(|e| hrv(e.code()))
        ));
        let mut buf = [0u16; 16];
        let mut len = 0u32;
        let r = a.GetString(&k(4), &mut buf, Some(&mut len));
        rep.line(format!(
            "GetString = {} len={} text={}",
            hr(&r),
            len,
            String::from_utf16_lossy(&buf[..len as usize])
        ));
        rep.line(format!(
            "GetBlobSize = {:?}",
            a.GetBlobSize(&k(5)).map_err(|e| hrv(e.code()))
        ));
        let mut blob = [0u8; 8];
        let mut blen = 0u32;
        let r = a.GetBlob(&k(5), &mut blob, Some(&mut blen));
        rep.line(format!("GetBlob = {} {:?}", hr(&r), &blob[..blen as usize]));
        let got: windows::core::Result<IMFAttributes> = a.GetUnknown(&k(6));
        rep.line(format!("GetUnknown = {}", hr(&got)));
        let other = {
            let mut o: Option<IMFAttributes> = None;
            MFCreateAttributes(&mut o, 4).unwrap();
            o.unwrap()
        };
        rep.line(format!("CopyAllItems = {}", hr(&a.CopyAllItems(&other))));
        rep.line(format!(
            "  copy count = {:?}",
            other.GetCount().map_err(|e| hrv(e.code()))
        ));
        rep.line(format!(
            "Compare(self copy, MATCH_ALL_ITEMS) = {:?}",
            a.Compare(&other, MF_ATTRIBUTES_MATCH_ALL_ITEMS)
                .map(|b| b.as_bool())
                .map_err(|e| hrv(e.code()))
        ));
        rep.line(format!(
            "CompareItem = {:?}",
            a.CompareItem(&k(1), &PROPVARIANT::from(7u32))
                .map(|b| b.as_bool())
                .map_err(|e| hrv(e.code()))
        ));
        rep.line(format!(
            "LockStore/UnlockStore = {} {}",
            hr(&a.LockStore()),
            hr(&a.UnlockStore())
        ));
        rep.line(format!("DeleteItem = {}", hr(&a.DeleteItem(&k(1)))));
        rep.line(format!("DeleteItem again = {}", hr(&a.DeleteItem(&k(1)))));
        for n in 2..=6 {
            let _ = a.DeleteItem(&k(n));
        }
    }

    // ---- activate the media source
    let mut writer = FrameWriter::with_names(ctx.names.clone());
    // SAFETY: valid activation object.
    let source: IMFMediaSource = unsafe { act.ActivateObject() }.expect("ActivateObject");
    rep.line("ActivateObject(IMFMediaSource) = S_OK");
    // SAFETY: valid source.
    unsafe {
        let again: IMFMediaSource = act.ActivateObject().unwrap();
        rep.line(format!(
            "ActivateObject twice returns the same object = {}",
            again.as_raw() == source.as_raw()
        ));
        let mut ppv: *mut c_void = std::ptr::null_mut();
        let vt = Interface::vtable(&act);
        rep.line(format!(
            "ActivateObject(ppv=null) = {}",
            hrv((vt.ActivateObject)(
                act.as_raw(),
                &IMFMediaSource::IID,
                std::ptr::null_mut()
            ))
        ));
        let h = (vt.ActivateObject)(act.as_raw(), &IMFAttributes::IID, &mut ppv);
        rep.line(format!("ActivateObject(riid=IMFAttributes) = {}", hrv(h)));
        for (name, iid) in [
            ("IMFMediaSource", IMFMediaSource::IID),
            ("IMFMediaSourceEx", IMFMediaSourceEx::IID),
            ("IMFMediaEventGenerator", IMFMediaEventGenerator::IID),
            ("IMFGetService", IMFGetService::IID),
            (
                "IKsControl",
                windows::Win32::Media::KernelStreaming::IKsControl::IID,
            ),
            ("IMFSampleAllocatorControl", IMFSampleAllocatorControl::IID),
            ("IMFRealTimeClientEx", IMFRealTimeClientEx::IID),
            ("IMFMediaStream", IMFMediaStream::IID),
            ("IMFAttributes", IMFAttributes::IID),
            ("IMFActivate", IMFActivate::IID),
            ("IUnknown", windows::core::IUnknown::IID),
        ] {
            let mut raw: *mut c_void = std::ptr::null_mut();
            let h = source.query(&iid, &mut raw);
            rep.line(format!("Source QI {name} = {}", hrv(h)));
            if h.is_ok() {
                drop(windows::core::IUnknown::from_raw(raw));
            }
        }
        for (name, iid) in [
            ("IInspectable", windows::core::IInspectable::IID),
            (
                "IAgileObject",
                GUID::from_u128(0x94ea2b94_e9cc_49e0_c0ff_ee64ca8f5b90),
            ),
        ] {
            let mut raw: *mut c_void = std::ptr::null_mut();
            let h = source.query(&iid, &mut raw);
            rep.info(format!("Source QI {name} = {}", hrv(h)));
            if h.is_ok() {
                drop(windows::core::IUnknown::from_raw(raw));
            }
        }
    }
    let ex: IMFMediaSourceEx = source.cast().unwrap();
    let source_gen: IMFMediaEventGenerator = source.cast().unwrap();

    // the writer finds the mapping the source created (global channel, 1280x720)
    writer.use_local(false);
    let mut opened = false;
    for _ in 0..40 {
        writer.try_open();
        if writer.is_open() {
            opened = true;
            break;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    rep.check(
        opened,
        "writer could not open the mapping the source created",
    );
    rep.line(format!(
        "writer sees mapping {}x{} consumer_active={}",
        writer.width(),
        writer.height(),
        writer.consumer_active()
    ));

    // SAFETY: valid source and interfaces throughout.
    unsafe {
        // ---- characteristics, presentation descriptor, media types
        rep.line(format!(
            "GetCharacteristics = {:?}",
            source.GetCharacteristics().map_err(|e| hrv(e.code()))
        ));
        let pd = source
            .CreatePresentationDescriptor()
            .expect("CreatePresentationDescriptor");
        let count = pd.GetStreamDescriptorCount().unwrap();
        rep.line(format!("pd stream count = {count}"));
        let mut selected = windows::core::BOOL(0);
        let mut sd: Option<IMFStreamDescriptor> = None;
        pd.GetStreamDescriptorByIndex(0, &mut selected, &mut sd)
            .unwrap();
        let sd = sd.unwrap();
        rep.line(format!(
            "stream 0 selected by default = {}",
            selected.as_bool()
        ));
        rep.line(format!(
            "stream id = {:?}",
            sd.GetStreamIdentifier().map_err(|e| hrv(e.code()))
        ));
        let sda: IMFAttributes = sd.cast().unwrap();
        rep.line("stream descriptor attributes:");
        for l in dump_attrs(&sda) {
            rep.line(l);
        }
        let handler = sd.GetMediaTypeHandler().unwrap();
        let n_types = handler.GetMediaTypeCount().unwrap();
        rep.line(format!("media type count = {n_types}"));
        for i in 0..n_types {
            let t = handler.GetMediaTypeByIndex(i).unwrap();
            let (w, h, num, den) = type_size(&t);
            rep.line(format!("media type {i}: {w}x{h} @ {num}/{den}"));
            let ta: IMFAttributes = t.cast().unwrap();
            for l in dump_attrs(&ta) {
                rep.line(l);
            }
        }
        let cur = handler.GetCurrentMediaType().unwrap();
        rep.line(format!("current type = {:?}", type_size(&cur)));
        rep.line(format!(
            "IsMediaTypeSupported(type 0) = {}",
            hr(&handler.IsMediaTypeSupported(&handler.GetMediaTypeByIndex(0).unwrap(), None))
        ));
        rep.line(format!(
            "major type = {}",
            gs(&handler.GetMajorType().unwrap())
        ));

        // ---- source attributes
        let sa = ex.GetSourceAttributes().unwrap();
        rep.line("source attributes:");
        for l in dump_attrs(&sa) {
            rep.line(l);
        }
        let sa2 = ex.GetSourceAttributes().unwrap();
        rep.line(format!(
            "GetSourceAttributes returns the same store = {}",
            sa.as_raw() == sa2.as_raw()
        ));
        let st = ex.GetStreamAttributes(0);
        rep.line(format!("GetStreamAttributes(0) = {}", hr(&st)));
        if let Ok(a) = &st {
            for l in dump_attrs(a) {
                rep.line(l);
            }
        }
        rep.line(format!(
            "GetStreamAttributes(1) = {}",
            hr(&ex.GetStreamAttributes(1))
        ));
        rep.line(format!(
            "SetD3DManager(null) = {}",
            hr(&ex.SetD3DManager(None::<&windows::core::IUnknown>))
        ));
        let svc = source.cast::<IMFGetService>().unwrap();
        rep.line(format!(
            "GetService = {}",
            hr(&svc.GetService::<IMFAttributes>(&MF_DEVICESTREAM_ATTRIBUTE_FRAMESOURCE_TYPES))
        ));
        let ac = source.cast::<IMFSampleAllocatorControl>().unwrap();
        let (mut inp, mut usage) = (99u32, MFSampleAllocatorUsage::default());
        rep.line(format!(
            "GetAllocatorUsage(0) = {} input={} usage={}",
            hr(&ac.GetAllocatorUsage(0, &mut inp, &mut usage)),
            inp,
            usage.0
        ));
        rep.line(format!(
            "GetAllocatorUsage(1) = {}",
            hr(&ac.GetAllocatorUsage(1, &mut inp, &mut usage))
        ));
        rep.line(format!(
            "SetDefaultAllocator(1) = {}",
            hr(&ac.SetDefaultAllocator(1, &source))
        ));
        // KS control
        let ks: windows::Win32::Media::KernelStreaming::IKsControl = source.cast().unwrap();
        let ident = KSIDENTIFIER::default();
        let mut bytes = 99u32;
        let sz = std::mem::size_of::<KSIDENTIFIER>() as u32;
        rep.line(format!(
            "KsProperty(valid) = {} bytes={}",
            hr(&ks.KsProperty(&ident, sz, std::ptr::null_mut(), 0, &mut bytes)),
            bytes
        ));
        bytes = 99;
        rep.line(format!(
            "KsProperty(short) = {} bytes={}",
            hr(&ks.KsProperty(&ident, sz - 1, std::ptr::null_mut(), 0, &mut bytes)),
            bytes
        ));
        rep.line(format!(
            "KsProperty(null) = {}",
            hr(&ks.KsProperty(std::ptr::null(), sz, std::ptr::null_mut(), 0, &mut bytes))
        ));
        bytes = 99;
        rep.line(format!(
            "KsMethod = {} bytes={}",
            hr(&ks.KsMethod(&ident, sz, std::ptr::null_mut(), 0, &mut bytes)),
            bytes
        ));
        bytes = 99;
        rep.line(format!(
            "KsEvent = {} bytes={}",
            hr(&ks.KsEvent(&ident, sz, std::ptr::null_mut(), 0, &mut bytes)),
            bytes
        ));

        // ---- events before Start, Pause, bad Start calls
        rep.line(format!(
            "GetEvent(no wait) before Start = {}",
            hr(&source_gen.GetEvent(MF_EVENT_FLAG_NO_WAIT))
        ));
        rep.line(format!("Pause = {}", hr(&source.Pause())));
        rep.line(format!("Stop before Start = {}", hr(&source.Stop())));
        rep.line(format!(
            "Start(pd=null) = {}",
            hr(&source.Start(
                None::<&IMFPresentationDescriptor>,
                std::ptr::null(),
                &PROPVARIANT::default()
            ))
        ));
        rep.line(format!(
            "Start(startPos=null) = {}",
            hr(&source.Start(&pd, std::ptr::null(), std::ptr::null()))
        ));
        rep.line(format!(
            "Start(time format) = {}",
            hr(&source.Start(&pd, &CLSID_VCAM, &PROPVARIANT::default()))
        ));
        let two = {
            let a = pd.GetStreamDescriptorByIndex(0, &mut selected, &mut Some(sd.clone()));
            let _ = a;
            let sds = [Some(sd.clone()), Some(sd.clone())];
            MFCreatePresentationDescriptor(Some(&sds)).unwrap()
        };
        rep.line(format!(
            "Start(two streams) = {}",
            hr(&source.Start(&two, std::ptr::null(), &PROPVARIANT::default()))
        ));
        // custom event through the source's queue (and the async pair)
        rep.line(format!(
            "QueueEvent(custom) = {}",
            hr(&source_gen.QueueEvent(
                MEUnknown.0 as u32,
                &GUID::zeroed(),
                S_OK,
                &PROPVARIANT::from(42u32)
            ))
        ));
        {
            let done = CreateEventW(None, false, false, PCWSTR::null()).unwrap();
            let res = Arc::new(Mutex::new(None));
            let cb: IMFAsyncCallback = Callback {
                done,
                result: res.clone(),
            }
            .into();
            rep.line(format!(
                "BeginGetEvent = {}",
                hr(&source_gen.BeginGetEvent(&cb, None::<&windows::core::IUnknown>))
            ));
            let w = WaitForSingleObject(done, 5000);
            rep.line(format!("BeginGetEvent callback invoked = {}", w.0 == 0));
            let r = res.lock().unwrap().take();
            if let Some(r) = r {
                let e = source_gen.EndGetEvent(&r);
                rep.line(format!(
                    "EndGetEvent = {} {}",
                    hr(&e),
                    e.as_ref().map(describe).unwrap_or_default()
                ));
            }
            let _ = windows::Win32::Foundation::CloseHandle(done);
        }

        // ---- Start with every format in turn, one sample per frame, lock-step
        let token: windows::core::IUnknown = {
            let mut o: Option<IMFAttributes> = None;
            MFCreateAttributes(&mut o, 1).unwrap();
            o.unwrap().cast().unwrap()
        };
        let mut stream: Option<IMFMediaStream2> = None;
        let mut frame_no = 0u32;
        for fi in 0..n_types {
            let (ty, r) = start_with_format(&source, &pd, fi).unwrap();
            let (fw, fh, fnum_, fden_) = type_size(&ty);
            rep.line(format!(
                "Start(format {fi}: {fw}x{fh}@{fnum_}/{fden_}) = {}",
                hr(&r)
            ));
            let evs = drain(&source_gen);
            for (_, e) in &evs {
                rep.line(format!("  source event: {}", describe(e)));
                if stream.is_none() {
                    if let Ok(pv) = e.GetValue() {
                        if let Ok(u) = windows::core::IUnknown::try_from(&pv) {
                            if let Ok(s) = u.cast::<IMFMediaStream2>() {
                                stream = Some(s);
                            }
                        }
                    }
                }
            }
            let stream = stream.as_ref().expect("stream from MENewStream");
            let sgen: IMFMediaEventGenerator = stream.cast().unwrap();
            for (_, e) in drain(&sgen) {
                rep.line(format!("  stream event: {}", describe(&e)));
            }
            rep.check(
                writer.consumer_active(),
                format!("reader not active while streaming (format {fi})"),
            );
            for _ in 0..3 {
                frame_no += 1;
                let src = pattern(frame_no, nw, nh);
                writer.write(&src, nw as usize, nw, nh);
                let t0 = mf_system_time();
                let s = request_one(stream, fw, fh, &token).unwrap();
                let t1 = mf_system_time();
                match s {
                    None => rep.check(false, format!("no sample for format {fi} frame {frame_no}")),
                    Some(s) => {
                        let expect = if (fw, fh) == (nw, nh) {
                            src.clone()
                        } else {
                            fit(&src, nw, nh, fw, fh)
                        };
                        rep.line(format!(
                            "  sample f{frame_no}: hash={:016x} expected={:016x} duration={} buffers={} cur_len={} pitch={} token={} time_in_window={}",
                            fnv(&s.tight),
                            fnv(&expect),
                            s.duration,
                            s.buffers,
                            s.cur_len,
                            s.pitch,
                            s.token_ok,
                            s.time >= t0 && s.time <= t1,
                        ));
                        rep.check(
                            s.tight == expect,
                            format!("sample content differs (format {fi}, frame {frame_no})"),
                        );
                        let want = 10_000_000i64 * fden_ as i64 / fnum_ as i64;
                        rep.check(
                            s.duration == want,
                            format!("duration {} != {} (format {fi})", s.duration, want),
                        );
                        rep.check(s.token_ok, "token not attached");
                        rep.check(
                            s.time >= t0 && s.time <= t1,
                            "sample time outside the request window",
                        );
                    }
                }
            }
        }
        let stream = stream.expect("stream");
        let sgen: IMFMediaEventGenerator = stream.cast().unwrap();

        // ---- stream interface
        let ps = stream.GetMediaSource().unwrap();
        rep.line(format!(
            "stream.GetMediaSource is the source = {}",
            ps.as_raw() == source.as_raw()
        ));
        let ssd = stream.GetStreamDescriptor().unwrap();
        rep.line(format!(
            "stream.GetStreamDescriptor id = {:?}",
            ssd.GetStreamIdentifier().map_err(|e| hrv(e.code()))
        ));
        rep.line(format!(
            "GetStreamState = {:?}",
            stream
                .GetStreamState()
                .map(|s| s.0)
                .map_err(|e| hrv(e.code()))
        ));
        rep.line(format!(
            "SetStreamState(RUNNING) same = {}",
            hr(&stream.SetStreamState(MF_STREAM_STATE_RUNNING))
        ));
        rep.line(format!(
            "SetStreamState(PAUSED) = {}",
            hr(&stream.SetStreamState(MF_STREAM_STATE_PAUSED))
        ));
        rep.line(format!(
            "GetStreamState paused = {:?}",
            stream
                .GetStreamState()
                .map(|s| s.0)
                .map_err(|e| hrv(e.code()))
        ));
        rep.line(format!(
            "RequestSample while paused = {}",
            hr(&stream.RequestSample(&token))
        ));
        rep.line(format!(
            "SetStreamState(RUNNING) after pause = {}",
            hr(&stream.SetStreamState(MF_STREAM_STATE_RUNNING))
        ));
        rep.line(format!(
            "SetStreamState(STOPPED) = {}",
            hr(&stream.SetStreamState(MF_STREAM_STATE_STOPPED))
        ));
        rep.line(format!(
            "  consumer_active after stream stop = {}",
            writer.consumer_active()
        ));
        rep.line(format!(
            "RequestSample while stopped = {}",
            hr(&stream.RequestSample(&token))
        ));
        rep.line(format!(
            "SetStreamState(PAUSED) from stopped = {}",
            hr(&stream.SetStreamState(MF_STREAM_STATE_PAUSED))
        ));
        rep.line(format!(
            "SetStreamState(RUNNING) from stopped = {}",
            hr(&stream.SetStreamState(MF_STREAM_STATE_RUNNING))
        ));
        rep.line(format!(
            "  consumer_active after restart = {}",
            writer.consumer_active()
        ));
        rep.line(format!(
            "stream QueueEvent(custom) = {}",
            hr(&sgen.QueueEvent(
                MEUnknown.0 as u32,
                &GUID::zeroed(),
                S_OK,
                &PROPVARIANT::from(9u32)
            ))
        ));
        for (_, e) in drain(&sgen) {
            rep.line(format!("  stream event: {}", describe(&e)));
        }
        // the last frame still on the camera, delivered after a restart at the last format
        let (fw, fh) = (
            type_size(
                &ssd.GetMediaTypeHandler()
                    .unwrap()
                    .GetCurrentMediaType()
                    .unwrap(),
            )
            .0,
            type_size(
                &ssd.GetMediaTypeHandler()
                    .unwrap()
                    .GetCurrentMediaType()
                    .unwrap(),
            )
            .1,
        );
        let src = pattern(frame_no, nw, nh);
        let s = request_one(&stream, fw, fh, &token).unwrap();
        rep.check(
            s.as_ref()
                .is_some_and(|s| s.tight == fit(&src, nw, nh, fw, fh)),
            "sample after a stream restart is the latest frame",
        );

        // ---- allocator can not change while running
        let alloc: IMFVideoSampleAllocator = {
            let mut raw: *mut c_void = std::ptr::null_mut();
            MFCreateVideoSampleAllocatorEx(&IMFVideoSampleAllocator::IID, &mut raw).unwrap();
            IMFVideoSampleAllocator::from_raw(raw)
        };
        rep.line(format!(
            "SetDefaultAllocator(0) while running = {}",
            hr(&ac.SetDefaultAllocator(0, &alloc))
        ));
        rep.line(format!(
            "SetDefaultAllocator(0, not an allocator) = {}",
            hr(&ac.SetDefaultAllocator(0, &token))
        ));

        // ---- staleness: no frame for 2.3 s -> "no signal" with the sample's own time
        std::thread::sleep(Duration::from_millis(2300));
        if let Some(s) = request_one(&stream, fw, fh, &token).unwrap() {
            let mut ns = vec![0xEEu8; (fw * fh * 3 / 2) as usize];
            draw_no_signal(&mut ns, fw as usize, fw, fh, s.time);
            rep.line(format!(
                "stale writer: sample is the no-signal picture of its own time = {}",
                s.tight == ns
            ));
            rep.check(
                s.tight == ns,
                "stale writer: sample is not the no-signal picture",
            );
        } else {
            rep.check(false, "stale writer: no sample");
        }
        // a fresh frame brings the picture back; HoldFor keeps it alive past 2 s
        frame_no += 1;
        let src = pattern(frame_no, nw, nh);
        writer.write(&src, nw as usize, nw, nh);
        writer.hold_for(6000);
        std::thread::sleep(Duration::from_millis(2300));
        let s = request_one(&stream, fw, fh, &token).unwrap();
        rep.line(format!(
            "held frame after 2.3 s is live = {}",
            s.as_ref()
                .is_some_and(|s| s.tight == fit(&src, nw, nh, fw, fh))
        ));
        rep.check(
            s.as_ref()
                .is_some_and(|s| s.tight == fit(&src, nw, nh, fw, fh)),
            "held frame went stale",
        );
        writer.release_hold();

        // ---- Stop / events / restart / Shutdown
        rep.line(format!("Stop = {}", hr(&source.Stop())));
        for (_, e) in drain(&source_gen) {
            rep.line(format!("  source event: {}", describe(&e)));
        }
        for (_, e) in drain(&sgen) {
            rep.line(format!("  stream event: {}", describe(&e)));
        }
        rep.line(format!(
            "  consumer_active after Stop = {}",
            writer.consumer_active()
        ));
        rep.line(format!("Stop again = {}", hr(&source.Stop())));
        rep.line(format!(
            "GetStreamState after Stop = {:?}",
            stream
                .GetStreamState()
                .map(|s| s.0)
                .map_err(|e| hrv(e.code()))
        ));
        // Start with the stream deselected (pd copy with the stream not selected)
        let pd2 = source.CreatePresentationDescriptor().unwrap();
        let mut sel2 = windows::core::BOOL(1);
        let mut sd2: Option<IMFStreamDescriptor> = None;
        pd2.GetStreamDescriptorByIndex(0, &mut sel2, &mut sd2)
            .unwrap();
        rep.line(format!("new pd: stream selected = {}", sel2.as_bool()));
        rep.line(format!(
            "Start(stream deselected) = {}",
            hr(&source.Start(&pd2, std::ptr::null(), &PROPVARIANT::default()))
        ));
        for (_, e) in drain(&source_gen) {
            rep.line(format!("  source event: {}", describe(&e)));
        }
        rep.line(format!(
            "  consumer_active after Start = {}",
            writer.consumer_active()
        ));
        rep.line(format!("Stop = {}", hr(&source.Stop())));
        drain(&source_gen);

        // Frame Server hands the source its own allocator while the stream is stopped
        // (GetAllocatorUsage says "uses provided allocator"): every sample then comes from it.
        let own_alloc: IMFVideoSampleAllocator = {
            let mut raw: *mut c_void = std::ptr::null_mut();
            MFCreateVideoSampleAllocatorEx(&IMFVideoSampleAllocator::IID, &mut raw).unwrap();
            IMFVideoSampleAllocator::from_raw(raw)
        };
        rep.line(format!(
            "SetDefaultAllocator(0) while stopped = {}",
            hr(&ac.SetDefaultAllocator(0, &own_alloc))
        ));
        frame_no += 1;
        let src_a = pattern(frame_no, nw, nh);
        writer.write(&src_a, nw as usize, nw, nh);
        pd2.SelectStream(0).unwrap();
        rep.line(format!(
            "Start(stream selected, provided allocator) = {}",
            hr(&source.Start(&pd2, std::ptr::null(), &PROPVARIANT::default()))
        ));
        for (_, e) in drain(&source_gen) {
            rep.line(format!("  source event: {}", describe(&e)));
        }
        let cur = type_size(
            &sd2.as_ref()
                .unwrap()
                .GetMediaTypeHandler()
                .unwrap()
                .GetCurrentMediaType()
                .unwrap(),
        );
        let s = request_one(&stream, cur.0, cur.1, &token).unwrap();
        let ok = s
            .as_ref()
            .is_some_and(|s| s.tight == fit(&src_a, nw, nh, cur.0, cur.1));
        rep.line(format!(
            "sample through the allocator Frame Server provided = {ok} ({}x{})",
            cur.0, cur.1
        ));
        rep.check(ok, "sample from the provided allocator differs");
        // The stream (re)initialised the allocator with 10 samples of the current type.
        rep.line(format!(
            "provided allocator can hand out a sample = {}",
            hr(&own_alloc.AllocateSample())
        ));
        rep.line(format!("Stop = {}", hr(&source.Stop())));
        drain(&source_gen);

        // ---- Shutdown
        rep.line(format!("source.Shutdown = {}", hr(&source.Shutdown())));
        rep.line(format!(
            "source.Shutdown again = {}",
            hr(&source.Shutdown())
        ));
        rep.line(format!(
            "after Shutdown: GetCharacteristics = {}",
            hr(&source.GetCharacteristics())
        ));
        rep.line(format!(
            "after Shutdown: CreatePresentationDescriptor = {}",
            hr(&source.CreatePresentationDescriptor())
        ));
        rep.line(format!(
            "after Shutdown: GetEvent = {}",
            hr(&source_gen.GetEvent(MF_EVENT_FLAG_NO_WAIT))
        ));
        rep.line(format!(
            "after Shutdown: Start = {}",
            hr(&source.Start(&pd2, std::ptr::null(), &PROPVARIANT::default()))
        ));
        rep.line(format!("after Shutdown: Stop = {}", hr(&source.Stop())));
        rep.line(format!(
            "after Shutdown: GetSourceAttributes = {}",
            hr(&ex.GetSourceAttributes())
        ));
        rep.line(format!(
            "after Shutdown: GetStreamAttributes = {}",
            hr(&ex.GetStreamAttributes(0))
        ));
        rep.line(format!(
            "after Shutdown: GetAllocatorUsage = {}",
            hr(&ac.GetAllocatorUsage(0, &mut inp, &mut usage))
        ));
        rep.line(format!(
            "after Shutdown: stream.RequestSample = {}",
            hr(&stream.RequestSample(&token))
        ));
        rep.line(format!(
            "after Shutdown: stream.GetStreamState = {}",
            hr(&stream.GetStreamState())
        ));
        rep.line(format!(
            "after Shutdown: stream.GetMediaSource = {}",
            hr(&stream.GetMediaSource())
        ));
        rep.line(format!(
            "after Shutdown: stream.GetEvent = {}",
            hr(&sgen.GetEvent(MF_EVENT_FLAG_NO_WAIT))
        ));
        rep.line(format!(
            "  consumer_active after Shutdown = {}",
            writer.consumer_active()
        ));

        // ---- activating again after Shutdown gives a new, working source (the writer lets go of
        // the old mapping first: a normal user cannot reopen it with full access, see xcheck/run.ps1)
        writer.close();
        let fresh: IMFMediaSource = act.ActivateObject().unwrap();
        rep.line(format!(
            "ActivateObject after source.Shutdown is a new object = {}",
            fresh.as_raw() != source.as_raw()
        ));
        rep.line(format!(
            "  new source GetCharacteristics = {}",
            hr(&fresh.GetCharacteristics())
        ));
        rep.line(format!("ShutdownObject = {}", hr(&act.ShutdownObject())));
        rep.line(format!(
            "  after ShutdownObject: new source GetCharacteristics = {}",
            hr(&fresh.GetCharacteristics())
        ));
        let third: IMFMediaSource = act.ActivateObject().unwrap();
        rep.line(format!(
            "ActivateObject after ShutdownObject = new object {}",
            third.as_raw() != fresh.as_raw()
        ));
        rep.line(format!("DetachObject = {}", hr(&act.DetachObject())));
        rep.line(format!(
            "  after DetachObject: source still alive GetCharacteristics = {}",
            hr(&third.GetCharacteristics())
        ));
        let _ = third.Shutdown();
    }
    drop(writer);
}

/// A source with no writer at all: "no signal" frames; then a size mismatch between the mapping
/// that already exists (kept alive by the writer) and the new source's native size.
fn scenario_signal(ctx: &Ctx, rep: &mut Rep) {
    let dll = &ctx.dll;
    rep.line("== scenario: no writer, then a writer with another size");
    // A: never written
    let act = new_activate(dll, 640, 360, 30, 1);
    // SAFETY: valid activation object; valid source and stream afterwards.
    unsafe {
        let source: IMFMediaSource = act.ActivateObject().unwrap();
        let pd = source.CreatePresentationDescriptor().unwrap();
        let (ty, r) = start_with_format(&source, &pd, 0).unwrap();
        let (fw, fh, _, _) = type_size(&ty);
        rep.line(format!("Start 640x360@30 = {}", hr(&r)));
        let sgen: IMFMediaEventGenerator = source.cast().unwrap();
        let mut stream: Option<IMFMediaStream2> = None;
        for (_, e) in drain(&sgen) {
            if let Ok(pv) = e.GetValue() {
                if let Ok(u) = windows::core::IUnknown::try_from(&pv) {
                    if let Ok(s) = u.cast::<IMFMediaStream2>() {
                        stream = Some(s);
                    }
                }
            }
        }
        let stream = stream.unwrap();
        let token: windows::core::IUnknown = {
            let mut o: Option<IMFAttributes> = None;
            MFCreateAttributes(&mut o, 1).unwrap();
            o.unwrap().cast().unwrap()
        };
        for i in 0..3 {
            let s = request_one(&stream, fw, fh, &token)
                .unwrap()
                .expect("sample");
            let mut ns = vec![0xEEu8; (fw * fh * 3 / 2) as usize];
            draw_no_signal(&mut ns, fw as usize, fw, fh, s.time);
            rep.line(format!(
                "never written: sample {i} is the no-signal picture = {}",
                s.tight == ns
            ));
            rep.check(s.tight == ns, "never written: not the no-signal picture");
        }
        // B: the writer opens the mapping; frames of its own size come through
        let mut writer = FrameWriter::with_names(ctx.names.clone());
        writer.use_local(false);
        for _ in 0..40 {
            writer.try_open();
            if writer.is_open() {
                break;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        let src = pattern(5, 640, 360);
        writer.write(&src, 640, 640, 360);
        let s = request_one(&stream, fw, fh, &token)
            .unwrap()
            .expect("sample");
        rep.line(format!("written 640x360: live = {}", s.tight == src));
        rep.check(s.tight == src, "written frame not delivered");

        // C: the mapping's header says another shape than the format (what a source finds when it
        // meets a mapping left over from an earlier size): the reader fits the picture it holds
        // into the format with black bars. The test reshapes the header the way the writer's
        // `OpenLocal` does for the local channel (360x640 has the bytes of 640x360).
        let raw = RawHeader::open(&ctx.names.global.mapping_string());
        raw.hdr().width.store(360, Ordering::SeqCst);
        raw.hdr().height.store(640, Ordering::SeqCst);
        raw.hdr().stride.store(360, Ordering::SeqCst);
        let src = pattern(6, 360, 640);
        writer.write(&src, 360, 360, 640);
        let s = request_one(&stream, fw, fh, &token)
            .unwrap()
            .expect("sample");
        let expect = fit(&src, 360, 640, fw, fh);
        rep.line(format!(
            "360x640 picture in a 640x360 format with bars = {} hash={:016x}",
            s.tight == expect,
            fnv(&s.tight)
        ));
        rep.check(s.tight == expect, "reshaped header: fitted picture differs");
        // A picture larger than the slot (header says more than frameBytes holds) is refused:
        raw.hdr().width.store(1280, Ordering::SeqCst);
        raw.hdr().height.store(720, Ordering::SeqCst);
        raw.hdr().stride.store(1280, Ordering::SeqCst);
        let s = request_one(&stream, fw, fh, &token)
            .unwrap()
            .expect("sample");
        let mut ns = vec![0xEEu8; (fw * fh * 3 / 2) as usize];
        draw_no_signal(&mut ns, fw as usize, fw, fh, s.time);
        rep.line(format!(
            "header larger than the slot -> no signal = {}",
            s.tight == ns
        ));
        rep.check(s.tight == ns, "oversized header: not the no-signal picture");
        raw.hdr().width.store(640, Ordering::SeqCst);
        raw.hdr().height.store(360, Ordering::SeqCst);
        raw.hdr().stride.store(640, Ordering::SeqCst);
        drop(raw);
        let _ = source.Stop();
        let _ = source.Shutdown();
        let _ = act.ShutdownObject();
        writer.close();
    }
}

/// A view of the global mapping's header, to poke at it the way the other side could.
struct RawHeader {
    h: *mut niwcam_shm::SharedFrameHeader,
    handle: HANDLE,
}

impl Drop for RawHeader {
    fn drop(&mut self) {
        // SAFETY: the view and the handle were created by `open` and are released once.
        unsafe {
            let _ = UnmapViewOfFile(MEMORY_MAPPED_VIEW_ADDRESS {
                Value: self.h as *mut c_void,
            });
            let _ = windows::Win32::Foundation::CloseHandle(self.handle);
        }
    }
}

impl RawHeader {
    fn open(name: &str) -> RawHeader {
        let wide: Vec<u16> = name.encode_utf16().chain(Some(0)).collect();
        // SAFETY: NUL-terminated name; whole-mapping view with the access the DACL grants.
        unsafe {
            let handle = OpenFileMappingW(
                FILE_MAP_READ.0 | FILE_MAP_WRITE.0,
                false,
                PCWSTR(wide.as_ptr()),
            )
            .expect("raw OpenFileMapping");
            let v = MapViewOfFile(handle, FILE_MAP_READ | FILE_MAP_WRITE, 0, 0, 0);
            assert!(!v.Value.is_null());
            RawHeader {
                h: v.Value as *mut niwcam_shm::SharedFrameHeader,
                handle,
            }
        }
    }
    fn hdr(&self) -> &niwcam_shm::SharedFrameHeader {
        // SAFETY: mapped for the life of the process, all fields atomic.
        unsafe { &*self.h }
    }
}

/// Pacing: how many samples per second the source hands out and how evenly, for a few writer rates.
fn timing(ctx: &Ctx, out: &mut Vec<String>) {
    let dll = &ctx.dll;
    // (label, native w, h, fps, format index, writer fps)
    let runs: [(&str, u32, u32, u32, u32, u32); 4] = [
        ("native30_writer30", 1280, 720, 30, 0, 30),
        ("native30_writer100", 1280, 720, 30, 0, 100),
        ("native60_writer60", 1280, 720, 60, 0, 60),
        ("native60_fmt30_writer15", 1280, 720, 60, 1, 15),
    ];
    for (label, w, h, fps, fmt, wfps) in runs {
        let act = new_activate(dll, w, h, fps, 1);
        // SAFETY: valid objects throughout; the writer thread only uses its own FrameWriter.
        unsafe {
            let source: IMFMediaSource = act.ActivateObject().unwrap();
            let pd = source.CreatePresentationDescriptor().unwrap();
            let (ty, _) = start_with_format(&source, &pd, fmt).unwrap();
            let (fw, fh, _, _) = type_size(&ty);
            let sgen: IMFMediaEventGenerator = source.cast().unwrap();
            let mut stream: Option<IMFMediaStream2> = None;
            for (_, e) in drain(&sgen) {
                if let Ok(pv) = e.GetValue() {
                    if let Ok(u) = windows::core::IUnknown::try_from(&pv) {
                        if let Ok(s) = u.cast::<IMFMediaStream2>() {
                            stream = Some(s);
                        }
                    }
                }
            }
            let stream = stream.unwrap();
            let token: windows::core::IUnknown = {
                let mut o: Option<IMFAttributes> = None;
                MFCreateAttributes(&mut o, 1).unwrap();
                o.unwrap().cast().unwrap()
            };

            let stop = Arc::new(AtomicBool::new(false));
            let stop_w = stop.clone();
            let names = ctx.names.clone();
            let writer_thread = std::thread::spawn(move || {
                let mut wr = FrameWriter::with_names(names);
                wr.use_local(false);
                for _ in 0..40 {
                    wr.try_open();
                    if wr.is_open() {
                        break;
                    }
                    std::thread::sleep(Duration::from_millis(100));
                }
                if !wr.is_open() {
                    eprintln!("fs_sim: timing writer could not open the mapping");
                }
                let mut frame = vec![0u8; (w * h * 3 / 2) as usize];
                let iv = Duration::from_secs_f64(1.0 / wfps as f64);
                let mut next = Instant::now();
                let mut k = 0u32;
                while !stop_w.load(Ordering::Relaxed) {
                    k += 1;
                    frame.fill(((k % 40) * 2 + 1) as u8); // odd: never 0x30 / 0xB0 (the no-signal picture)
                    wr.write(&frame, w as usize, w, h);
                    next += iv;
                    while Instant::now() < next {
                        std::thread::yield_now();
                    }
                }
            });
            std::thread::sleep(Duration::from_millis(500));

            // Frame Server keeps a few requests outstanding.
            let sgen: IMFMediaEventGenerator = stream.cast().unwrap();
            let start = Instant::now();
            let mut outstanding = 0;
            let mut times: Vec<i64> = Vec::new();
            let mut firsts: Vec<u8> = Vec::new();
            let mut nosignal = 0u32;
            while start.elapsed() < Duration::from_secs(4) {
                while outstanding < 3 {
                    stream.RequestSample(&token).unwrap();
                    outstanding += 1;
                }
                match sgen.GetEvent(MF_EVENT_FLAG_NO_WAIT) {
                    Ok(e) => {
                        if let Some(s) = read_sample(&e, fw, fh, &token) {
                            outstanding -= 1;
                            times.push(s.time);
                            firsts.push(s.tight[0]);
                            if s.tight[0] == 0x30 || s.tight[0] == 0xB0 {
                                nosignal += 1;
                            }
                        }
                    }
                    Err(_) => std::thread::sleep(Duration::from_micros(500)),
                }
            }
            stop.store(true, Ordering::Relaxed);
            writer_thread.join().unwrap();
            let _ = source.Stop();
            let _ = source.Shutdown();
            let _ = act.ShutdownObject();

            let n = times.len();
            let gaps: Vec<f64> = times
                .windows(2)
                .map(|p| (p[1] - p[0]) as f64 / 10_000.0)
                .collect();
            let mean = gaps.iter().sum::<f64>() / gaps.len().max(1) as f64;
            let max = gaps.iter().cloned().fold(0.0, f64::max);
            let var = gaps.iter().map(|g| (g - mean) * (g - mean)).sum::<f64>()
                / gaps.len().max(1) as f64;
            let unique = {
                let mut u = 1;
                for p in firsts.windows(2) {
                    if p[0] != p[1] {
                        u += 1;
                    }
                }
                if firsts.is_empty() {
                    0
                } else {
                    u
                }
            };
            out.push(format!(
                "T {label} samples={n} mean_ms={mean:.2} stddev_ms={:.2} max_ms={max:.2} unique_frames={unique} nosignal={nosignal}",
                var.sqrt()
            ));
        }
    }
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 4 {
        eprintln!("usage: fs_sim <dll> <report-file> <timing-file> [skip-timing]");
        std::process::exit(2);
    }
    let suffix = std::env::var("NIWCAM_TEST_SUFFIX").expect("NIWCAM_TEST_SUFFIX must be set");
    // Safety net: never hang the build.
    std::thread::spawn(|| {
        std::thread::sleep(Duration::from_secs(240));
        eprintln!("fs_sim: watchdog expired");
        std::process::exit(3);
    });
    // SAFETY: plain COM / MF start-up on this thread.
    unsafe {
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
        MFStartup(MF_VERSION, MFSTARTUP_FULL).expect("MFStartup");
    }
    let ctx = Ctx {
        dll: load(&args[1]),
        names: Names::for_test(&suffix),
    };
    let _ = formats_expected(0, 0, 0, 0);
    let _ = (E_NOINTERFACE, S_FALSE);

    let mut rep = Rep {
        lines: Vec::new(),
        fails: Vec::new(),
    };
    scenario_main(&ctx, &mut rep);
    scenario_signal(&ctx, &mut rep);
    std::fs::write(&args[2], rep.lines.join("\n") + "\n").unwrap();

    let mut t = Vec::new();
    if args.get(4).map(String::as_str) != Some("skip-timing") {
        timing(&ctx, &mut t);
    }
    std::fs::write(&args[3], t.join("\n") + "\n").unwrap();

    if rep.fails.is_empty() {
        println!(
            "fs_sim {}: no failures, {} report lines",
            args[1],
            rep.lines.len()
        );
    } else {
        for f in &rep.fails {
            println!("{f}");
        }
        println!("fs_sim {}: {} FAILURES", args[1], rep.fails.len());
        std::process::exit(1);
    }
}
