//! The output pin of the filter and its streaming thread. Ports `OutputPin` of `src/dshow/SourceFilter.{h,cpp}`.

use crate::com::{
    IAMStreamConfigX, IAMStreamConfigX_Impl, IKsPropertySetX, IKsPropertySetX_Impl, IPinX,
    IPinX_Impl, IQualityControlX, IQualityControlX_Impl,
};
use crate::media_type::{alloc_media_type, copy_media_type_fields, free_media_type_fields};
use crate::reader::FrameReader;
use crate::{guard, lock, FilterShared};
use core::ffi::c_void;
use core::mem::{size_of, ManuallyDrop};
use core::ptr;
use core::sync::atomic::{AtomicBool, Ordering::SeqCst};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use windows::core::{implement, IUnknownImpl, Interface, BOOL, GUID, HRESULT, PWSTR};
use windows::Win32::Foundation::{
    CloseHandle, E_FAIL, E_NOTIMPL, E_OUTOFMEMORY, E_POINTER, E_UNEXPECTED, HANDLE, SIZE, S_FALSE,
    S_OK,
};
use windows::Win32::Graphics::Gdi::BITMAPINFOHEADER;
use windows::Win32::Media::DirectShow::{
    IBaseFilter, IFilterGraph, IFilterGraph2, IMediaSample, IMemAllocator, IMemInputPin, IPin,
    Quality, State_Stopped, ALLOCATOR_PROPERTIES, AMPROPERTY_PIN_CATEGORY, E_PROP_ID_UNSUPPORTED,
    E_PROP_SET_UNSUPPORTED, PINDIR_OUTPUT, PIN_INFO, VFW_E_ALREADY_CONNECTED,
    VFW_E_INVALIDMEDIATYPE, VFW_E_NOT_CONNECTED, VFW_E_NOT_STOPPED, VFW_E_NO_ACCEPTABLE_TYPES,
    VFW_E_TYPE_NOT_ACCEPTED, VIDEO_STREAM_CONFIG_CAPS,
};
use windows::Win32::Media::IReferenceClock;
use windows::Win32::Media::MediaFoundation::{
    AMPROPSETID_Pin, CLSID_MemoryAllocator, FORMAT_VideoInfo, FORMAT_VideoInfo2, MEDIATYPE_Video,
    MFGetSystemTime, AM_MEDIA_TYPE, PIN_CATEGORY_CAPTURE, VIDEOINFOHEADER, VIDEOINFOHEADER2,
};
use windows::Win32::System::Com::{CoCreateInstance, CoTaskMemAlloc, CLSCTX_INPROC_SERVER};
use windows::Win32::System::Threading::{
    CancelWaitableTimer, CreateWaitableTimerW, GetCurrentThread, SetThreadPriority,
    SetWaitableTimer, Sleep, WaitForMultipleObjects, THREAD_PRIORITY_ABOVE_NORMAL,
};

// KSPROPERTY_SUPPORT_GET (ks.h), the only support bit the category property has.
const KSPROPERTY_SUPPORT_GET: u32 = 1;

pub const PIN_NAME: &str = "Capture";
const MIN_INTERVAL: i64 = 166667; // 60 fps
const MAX_INTERVAL: i64 = 2000000; // 5 fps
const DEFAULT_INTERVAL: i64 = 333333; // 30 fps

pub const FOURCC_NV12: u32 = 0x3231564E; // 'NV12'
pub const FOURCC_YUY2: u32 = 0x32595559; // 'YUY2'
pub const FOURCC_I420: u32 = 0x30323449; // 'I420'
const FOURCCS: [u32; 3] = [FOURCC_NV12, FOURCC_YUY2, FOURCC_I420];
const STANDARD_SIZES: [(u32, u32); 6] = [
    (1920, 1080),
    (1280, 720),
    (960, 540),
    (640, 480),
    (640, 360),
    (320, 240),
];

/// `FourccSubtype`: FOURCC subtypes (uuids.h has some of them under names that vary between SDKs).
pub const fn fourcc_subtype(fourcc: u32) -> GUID {
    GUID {
        data1: fourcc,
        data2: 0x0000,
        data3: 0x0010,
        data4: [0x80, 0x00, 0x00, 0xAA, 0x00, 0x38, 0x9B, 0x71],
    }
}

/// `PinFormat`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PinFormat {
    pub fourcc: u32,
    pub width: u32,
    pub height: u32,
}

impl Default for PinFormat {
    fn default() -> Self {
        Self {
            fourcc: FOURCC_NV12,
            width: 1280,
            height: 720,
        }
    }
}

impl PinFormat {
    pub fn sample_bytes(&self) -> u32 {
        if self.fourcc == FOURCC_YUY2 {
            self.width * self.height * 2
        } else {
            self.width * self.height * 3 / 2
        }
    }
    pub fn bits_per_pixel(&self) -> u32 {
        if self.fourcc == FOURCC_YUY2 {
            16
        } else {
            12
        }
    }
}

fn pin_name_wide() -> [u16; 8] {
    let mut out = [0u16; 8];
    for (o, c) in out.iter_mut().zip(PIN_NAME.encode_utf16()) {
        *o = c;
    }
    out
}

fn is_partial(mt: Option<&AM_MEDIA_TYPE>) -> bool {
    match mt {
        None => true,
        Some(mt) => {
            mt.majortype == GUID::zeroed()
                || mt.subtype == GUID::zeroed()
                || mt.formattype == GUID::zeroed()
        }
    }
}

/// NV12 (contiguous, pitch w) -> YUY2 (pitch 2w). Chroma rows are shared by two luma rows.
/// (Written with chunk iterators so the per-pixel loop has no bounds checks: the C++ has none.)
pub fn nv12_to_yuy2(nv12: &[u8], w: u32, h: u32, dst: &mut [u8]) {
    let (w, h) = (w as usize, h as usize);
    let (luma, uv_plane) = nv12.split_at(w * h);
    for (y, d) in dst.chunks_exact_mut(w * 2).take(h).enumerate() {
        let yr = &luma[y * w..(y + 1) * w];
        let uv = &uv_plane[(y / 2) * w..(y / 2 + 1) * w];
        let (d4, _) = d.as_chunks_mut::<4>();
        let (y2, _) = yr.as_chunks::<2>();
        let (c2, _) = uv.as_chunks::<2>();
        for ((o, yy), cc) in d4.iter_mut().zip(y2).zip(c2) {
            *o = [yy[0], cc[0], yy[1], cc[1]];
        }
    }
}

/// NV12 -> I420 (Y, then U, then V, each plane packed).
pub fn nv12_to_i420(nv12: &[u8], w: u32, h: u32, dst: &mut [u8]) {
    let luma = w as usize * h as usize;
    dst[..luma].copy_from_slice(&nv12[..luma]);
    let (pairs, _) = nv12[luma..].as_chunks::<2>();
    let (u, v) = dst[luma..].split_at_mut(luma / 4);
    for ((u, v), p) in u.iter_mut().zip(v.iter_mut()).zip(pairs) {
        *u = p[0];
        *v = p[1];
    }
}

/// `OutputPin::Parse`: a media type this pin accepts -> its format and frame interval.
///
/// # Safety
/// `mt.pbFormat` must be readable for `mt.cbFormat` bytes.
pub unsafe fn parse(mt: &AM_MEDIA_TYPE, f: &mut PinFormat, interval: &mut i64) -> bool {
    if mt.majortype != MEDIATYPE_Video {
        return false;
    }
    let mut fourcc = 0;
    for c in FOURCCS {
        if mt.subtype == fourcc_subtype(c) {
            fourcc = c;
        }
    }
    if fourcc == 0 {
        return false;
    }
    let (bmi, avg): (BITMAPINFOHEADER, i64);
    // SAFETY: cbFormat bytes are readable at pbFormat (precondition); read unaligned as the block is a caller's.
    unsafe {
        if mt.formattype == FORMAT_VideoInfo
            && mt.cbFormat as usize >= size_of::<VIDEOINFOHEADER>()
            && !mt.pbFormat.is_null()
        {
            let vih = ptr::read_unaligned(mt.pbFormat as *const VIDEOINFOHEADER);
            bmi = vih.bmiHeader;
            avg = vih.AvgTimePerFrame;
        } else if mt.formattype == FORMAT_VideoInfo2
            && mt.cbFormat as usize >= size_of::<VIDEOINFOHEADER2>()
            && !mt.pbFormat.is_null()
        {
            let vih = ptr::read_unaligned(mt.pbFormat as *const VIDEOINFOHEADER2);
            bmi = vih.bmiHeader;
            avg = vih.AvgTimePerFrame;
        } else {
            return false;
        }
    }
    let w = bmi.biWidth;
    let h = if bmi.biHeight < 0 {
        -bmi.biHeight
    } else {
        bmi.biHeight
    };
    if w < 16 || h < 16 || (w & 1) != 0 || (h & 1) != 0 || w > 8192 || h > 8192 {
        return false;
    }
    if bmi.biCompression != fourcc {
        return false;
    }
    f.fourcc = fourcc;
    f.width = w as u32;
    f.height = h as u32;
    *interval = if avg > 0 {
        avg.clamp(MIN_INTERVAL, MAX_INTERVAL)
    } else {
        DEFAULT_INTERVAL
    };
    true
}

/// `OutputPin::Build`: the media type of a format (a CoTaskMem VIDEOINFOHEADER block).
pub fn build(f: &PinFormat, interval: i64, mt: &mut AM_MEDIA_TYPE) -> HRESULT {
    *mt = AM_MEDIA_TYPE::default();
    // SAFETY: the block is allocated here, checked, fully initialised before use, and owned by `mt`.
    unsafe {
        let vih = CoTaskMemAlloc(size_of::<VIDEOINFOHEADER>()) as *mut VIDEOINFOHEADER;
        if vih.is_null() {
            return E_OUTOFMEMORY;
        }
        ptr::write_bytes(vih as *mut u8, 0, size_of::<VIDEOINFOHEADER>());
        (*vih).AvgTimePerFrame = interval;
        (*vih).dwBitRate = (u32::MAX as u64)
            .min(f.sample_bytes() as u64 * 8 * 10_000_000 / interval as u64)
            as u32;
        (*vih).bmiHeader.biSize = size_of::<BITMAPINFOHEADER>() as u32;
        (*vih).bmiHeader.biWidth = f.width as i32;
        (*vih).bmiHeader.biHeight = f.height as i32;
        (*vih).bmiHeader.biPlanes = 1;
        (*vih).bmiHeader.biBitCount = f.bits_per_pixel() as u16;
        (*vih).bmiHeader.biCompression = f.fourcc;
        (*vih).bmiHeader.biSizeImage = f.sample_bytes();
        mt.pbFormat = vih as *mut u8;
    }
    mt.majortype = MEDIATYPE_Video;
    mt.subtype = fourcc_subtype(f.fourcc);
    mt.bFixedSizeSamples = BOOL(1);
    mt.bTemporalCompression = BOOL(0);
    mt.lSampleSize = f.sample_bytes();
    mt.formattype = FORMAT_VideoInfo;
    mt.cbFormat = size_of::<VIDEOINFOHEADER>() as u32;
    S_OK
}

/// What the filter (the owner of the pin) lets the pin know: the graph and the state live in `FilterShared`,
/// `iface` is the filter's `IBaseFilter` (not counted: no reference cycle, as the C++ raw `m_filter`).
struct FilterLink {
    iface: *mut c_void,
    shared: Arc<FilterShared>,
}

/// Everything under `m_lock`: connection and format.
struct PinState {
    connected: Option<IPin>,
    input: Option<IMemInputPin>,
    allocator: Option<IMemAllocator>,
    format: PinFormat,
    interval: i64,
    format_set: bool,
}

struct ClockState {
    clock: Option<IReferenceClock>,
    start: i64,
}

/// The pin's data, shared with the streaming thread.
pub struct PinInner {
    state: Mutex<PinState>,
    link: Mutex<Option<FilterLink>>,
    reader: FrameReader,
    thread: Mutex<Option<JoinHandle<()>>>,
    quit: AtomicBool,
    running: AtomicBool,
    timer: HANDLE,
    clock: Mutex<ClockState>,
}

// SAFETY: DirectShow objects are free-threaded (the class is registered ThreadingModel=Both); the C++ passes
// the same raw interface pointers to its worker thread. Every mutable member is behind a mutex or an atomic.
unsafe impl Send for PinInner {}
// SAFETY: see above.
unsafe impl Sync for PinInner {}

impl PinInner {
    fn filter_state(&self) -> i32 {
        lock(&self.link)
            .as_ref()
            .map_or(State_Stopped.0, |l| l.shared.state.load(SeqCst))
    }

    fn filter_graph(&self) -> *mut c_void {
        lock(&self.link)
            .as_ref()
            .map_or(ptr::null_mut(), |l| l.shared.graph.load(SeqCst))
    }

    pub fn connected(&self) -> bool {
        lock(&self.state).connected.is_some()
    }

    /// Everything offered, in order of preference: the host's current picture size first.
    fn offered(&self) -> Vec<PinFormat> {
        let mut sizes: Vec<(u32, u32)> = Vec::new();
        let mut add = |w: u32, h: u32| {
            if !sizes.contains(&(w, h)) {
                sizes.push((w, h));
            }
        };
        // The host's picture first (its own size is the best quality), when the host runs.
        self.reader.open_existing();
        if self.reader.width() >= 16 && self.reader.height() >= 16 {
            add(self.reader.width() & !1, self.reader.height() & !1);
        }
        for (w, h) in STANDARD_SIZES {
            add(w, h);
        }
        let mut out = Vec::new();
        for (w, h) in sizes {
            for fourcc in FOURCCS {
                out.push(PinFormat {
                    fourcc,
                    width: w,
                    height: h,
                });
            }
        }
        out
    }

    fn negotiate_allocator(
        input: &IMemInputPin,
        sample_bytes: u32,
    ) -> Result<IMemAllocator, HRESULT> {
        // SAFETY: plain COM call; the requirements are optional, so a failure leaves them zero.
        let wanted = unsafe { input.GetAllocatorRequirements() }.unwrap_or_default();
        // SAFETY: plain COM calls on live interfaces with valid arguments.
        unsafe {
            let allocator = match input.GetAllocator() {
                Ok(a) => a,
                Err(_) => CoCreateInstance::<_, IMemAllocator>(
                    &CLSID_MemoryAllocator,
                    None,
                    CLSCTX_INPROC_SERVER,
                )
                .map_err(|e| e.code())?,
            };
            let props = ALLOCATOR_PROPERTIES {
                cBuffers: wanted.cBuffers.max(4),
                cbBuffer: wanted.cbBuffer.max(sample_bytes as i32),
                cbAlign: wanted.cbAlign.max(1),
                cbPrefix: wanted.cbPrefix,
            };
            let actual = allocator.SetProperties(&props).map_err(|e| e.code())?;
            if actual.cbBuffer < sample_bytes as i32 {
                return Err(E_FAIL);
            }
            input
                .NotifyAllocator(&allocator, false)
                .map_err(|e| e.code())?;
            Ok(allocator)
        }
    }

    /// `IPin::Connect`.
    ///
    /// # Safety
    /// `pmt` is null or a valid media type.
    pub unsafe fn connect(
        &self,
        this: &IPin,
        receiver: &IPin,
        pmt: *const AM_MEDIA_TYPE,
    ) -> HRESULT {
        // Build the candidate list under the lock, then call into the receiver WITHOUT it. A
        // receiver may call back into this pin during ReceiveConnection (ConnectionMediaType,
        // EnumMediaTypes, GetFormat); the mutex is not recursive, so holding it across the call
        // would self-deadlock and hang the host app.
        let mut candidates: Vec<AM_MEDIA_TYPE> = Vec::new();
        {
            let st = lock(&self.state);
            if st.connected.is_some() {
                return VFW_E_ALREADY_CONNECTED;
            }
            if self.filter_state() != State_Stopped.0 {
                return VFW_E_NOT_STOPPED;
            }
            // SAFETY: precondition: null or valid.
            let pmt_ref = unsafe { pmt.as_ref() };

            let push = |f: &PinFormat, interval: i64, candidates: &mut Vec<AM_MEDIA_TYPE>| {
                if let Some(pmt) = pmt_ref {
                    if pmt.majortype != GUID::zeroed() && pmt.majortype != MEDIATYPE_Video {
                        return;
                    }
                    if pmt.subtype != GUID::zeroed() && pmt.subtype != fourcc_subtype(f.fourcc) {
                        return;
                    }
                    if pmt.formattype != GUID::zeroed() && pmt.formattype != FORMAT_VideoInfo {
                        return;
                    }
                }
                let mut mt = AM_MEDIA_TYPE::default();
                if build(f, interval, &mut mt).is_ok() {
                    candidates.push(mt);
                }
            };
            if !is_partial(pmt_ref) {
                let mut f = PinFormat::default();
                let mut interval = 0;
                if let Some(p) = pmt_ref {
                    // SAFETY: precondition on the format block.
                    if !unsafe { parse(p, &mut f, &mut interval) } {
                        return VFW_E_TYPE_NOT_ACCEPTED;
                    }
                    let mut mt = AM_MEDIA_TYPE::default();
                    // SAFETY: mt is a valid destination, p a valid source.
                    if unsafe { copy_media_type_fields(&mut mt, p) }.is_ok() {
                        candidates.push(mt);
                    }
                }
            } else {
                if st.format_set {
                    push(&st.format, st.interval, &mut candidates);
                }
                for f in self.offered() {
                    push(
                        &f,
                        if st.format_set {
                            st.interval
                        } else {
                            DEFAULT_INTERVAL
                        },
                        &mut candidates,
                    );
                }
            }
        }

        let mut hr = VFW_E_NO_ACCEPTABLE_TYPES;
        for mut mt in candidates {
            let mut f = PinFormat::default();
            let mut interval = 0;
            // SAFETY: candidates were built here or copied from a valid media type.
            if hr != S_OK && unsafe { parse(&mt, &mut f, &mut interval) } && {
                // SAFETY: raw vtable call: the HRESULT is compared with S_OK exactly, as the C++ does (a typed
                // call would turn every success code into Ok).
                let r = unsafe {
                    (Interface::vtable(receiver).ReceiveConnection)(
                        receiver.as_raw(),
                        this.as_raw(),
                        &mt,
                    )
                };
                r == S_OK
            } {
                let input = receiver.cast::<IMemInputPin>().ok();
                let mut allocator = None;
                let mut ok = false;
                if let Some(input) = &input {
                    if let Ok(a) = Self::negotiate_allocator(input, f.sample_bytes()) {
                        allocator = Some(a);
                        ok = true;
                    }
                }
                if ok {
                    let mut st = lock(&self.state);
                    if st.connected.is_none() {
                        st.input = input;
                        st.allocator = allocator;
                        st.connected = Some(receiver.clone());
                        st.format = f;
                        st.interval = interval;
                        st.format_set = true;
                        hr = S_OK;
                    } else {
                        ok = false; // something connected while we were unlocked
                    }
                }
                if !ok {
                    // SAFETY: plain COM call; the result is ignored as in the C++.
                    let _ = unsafe { receiver.Disconnect() };
                }
            }
            // SAFETY: mt was built or copied above.
            unsafe { free_media_type_fields(&mut mt) };
        }
        hr
    }

    pub fn disconnect(&self) -> HRESULT {
        let mut st = lock(&self.state);
        if self.filter_state() != State_Stopped.0 {
            return VFW_E_NOT_STOPPED;
        }
        if st.connected.is_none() {
            return S_FALSE;
        }
        st.input = None;
        st.allocator = None;
        st.connected = None;
        S_OK
    }

    /// # Safety
    /// `out` must be valid for writing a pointer.
    pub unsafe fn connected_to(&self, out: *mut *mut c_void) -> HRESULT {
        if out.is_null() {
            return E_POINTER;
        }
        let st = lock(&self.state);
        // SAFETY: out is valid (precondition).
        unsafe { *out = ptr::null_mut() };
        match &st.connected {
            None => VFW_E_NOT_CONNECTED,
            Some(p) => {
                // SAFETY: as above; `clone` is the AddRef the caller owns.
                unsafe { *out = p.clone().into_raw() };
                S_OK
            }
        }
    }

    /// # Safety
    /// `pmt` must be valid for writing a media type.
    pub unsafe fn connection_media_type(&self, pmt: *mut AM_MEDIA_TYPE) -> HRESULT {
        if pmt.is_null() {
            return E_POINTER;
        }
        let st = lock(&self.state);
        // SAFETY: pmt is writable (precondition); a zeroed AM_MEDIA_TYPE is the C++ `*pmt = {}`.
        unsafe {
            ptr::write(pmt, AM_MEDIA_TYPE::default());
            if st.connected.is_none() {
                return VFW_E_NOT_CONNECTED;
            }
            build(&st.format, st.interval, &mut *pmt)
        }
    }

    /// # Safety
    /// `info` must be valid for writing a `PIN_INFO`.
    pub unsafe fn query_pin_info(&self, info: *mut PIN_INFO) -> HRESULT {
        if info.is_null() {
            return E_POINTER;
        }
        let filter = lock(&self.link).as_ref().map(|l| l.iface);
        let p = filter.and_then(|raw| {
            // SAFETY: the link is cleared before the filter dies (Orphan), so `raw` is a live IBaseFilter.
            unsafe { IBaseFilter::from_raw_borrowed(&raw) }.cloned()
        });
        let name = pin_name_wide();
        // SAFETY: info is writable (precondition). Like wcscpy_s, only the string and its NUL are written.
        unsafe {
            ptr::write(&raw mut (*info).pFilter, ManuallyDrop::new(p));
            (*info).dir = PINDIR_OUTPUT;
            ptr::copy_nonoverlapping(
                name.as_ptr(),
                (*info).achName.as_mut_ptr(),
                PIN_NAME.len() + 1,
            );
        }
        S_OK
    }

    /// # Safety
    /// `id` must be valid for writing a pointer.
    pub unsafe fn query_id(&self, id: *mut PWSTR) -> HRESULT {
        if id.is_null() {
            return E_POINTER;
        }
        let name = pin_name_wide();
        let bytes = (PIN_NAME.len() + 1) * 2;
        // SAFETY: id is writable (precondition); the block is checked and large enough.
        unsafe {
            let p = CoTaskMemAlloc(bytes) as *mut u16;
            *id = PWSTR(p);
            if p.is_null() {
                return E_OUTOFMEMORY;
            }
            ptr::copy_nonoverlapping(name.as_ptr(), p, PIN_NAME.len() + 1);
        }
        S_OK
    }

    /// # Safety
    /// `pmt` is null or a valid media type.
    pub unsafe fn query_accept(&self, pmt: *const AM_MEDIA_TYPE) -> HRESULT {
        // SAFETY: precondition.
        let Some(pmt) = (unsafe { pmt.as_ref() }) else {
            return E_POINTER;
        };
        let mut f = PinFormat::default();
        let mut interval = 0;
        // SAFETY: precondition on the format block.
        if unsafe { parse(pmt, &mut f, &mut interval) } {
            S_OK
        } else {
            S_FALSE
        }
    }

    /// Builds the media types of `IPin::EnumMediaTypes`.
    pub fn enum_media_types(&self) -> Vec<AM_MEDIA_TYPE> {
        let mut types = Vec::new();
        let st = lock(&self.state);
        if st.format_set {
            let mut mt = AM_MEDIA_TYPE::default();
            if build(&st.format, st.interval, &mut mt).is_ok() {
                types.push(mt);
            }
        }
        for f in self.offered() {
            if st.format_set && f == st.format {
                continue;
            }
            let mut mt = AM_MEDIA_TYPE::default();
            if build(
                &f,
                if st.format_set {
                    st.interval
                } else {
                    DEFAULT_INTERVAL
                },
                &mut mt,
            )
            .is_ok()
            {
                types.push(mt);
            }
        }
        types
    }

    // ---- IAMStreamConfig ----

    /// # Safety
    /// `pmt` is null or a valid media type.
    pub unsafe fn set_format(&self, this: &IPin, pmt: *mut AM_MEDIA_TYPE) -> HRESULT {
        // SAFETY: precondition.
        let Some(pmt_ref) = (unsafe { pmt.as_ref() }) else {
            return E_POINTER;
        };
        let mut f = PinFormat::default();
        let mut interval = 0;
        // SAFETY: precondition on the format block.
        if !unsafe { parse(pmt_ref, &mut f, &mut interval) } {
            return VFW_E_INVALIDMEDIATYPE;
        }
        let connected;
        {
            let mut st = lock(&self.state);
            if let Some(c) = &st.connected {
                if self.filter_state() != State_Stopped.0 {
                    return VFW_E_NOT_STOPPED;
                }
                // SAFETY: plain COM call; the result is compared with S_OK exactly as the C++ does.
                if unsafe { c.QueryAccept(pmt) } != S_OK {
                    return VFW_E_INVALIDMEDIATYPE;
                }
                connected = true;
            } else {
                connected = false;
            }
            st.format = f;
            st.interval = interval;
            st.format_set = true;
        }
        if connected {
            // Connected already: the graph connects us again with the new type.
            let graph = self.filter_graph();
            // SAFETY: the graph pointer is the one JoinFilterGraph stored (not counted, per the DirectShow
            // rules); it is valid while the filter is in the graph, as the C++ relies on too.
            if let Some(g) = unsafe { IFilterGraph::from_raw_borrowed(&graph) } {
                if let Ok(g2) = g.cast::<IFilterGraph2>() {
                    // SAFETY: raw vtable call so the exact HRESULT is returned, as `return g2->ReconnectEx(...)`.
                    return unsafe {
                        (Interface::vtable(&g2).ReconnectEx)(g2.as_raw(), this.as_raw(), pmt)
                    };
                }
                // SAFETY: as above, for IFilterGraph::Reconnect.
                return unsafe { (Interface::vtable(g).Reconnect)(g.as_raw(), this.as_raw()) };
            }
        }
        S_OK
    }

    /// # Safety
    /// `ppmt` must be valid for writing a pointer.
    pub unsafe fn get_format(&self, ppmt: *mut *mut AM_MEDIA_TYPE) -> HRESULT {
        if ppmt.is_null() {
            return E_POINTER;
        }
        let mut mt = AM_MEDIA_TYPE::default();
        {
            let st = lock(&self.state);
            let mut f = st.format;
            let mut interval = st.interval;
            if !st.format_set {
                let offered = self.offered();
                if let Some(first) = offered.first() {
                    f = *first;
                }
                interval = DEFAULT_INTERVAL;
            }
            let hr = build(&f, interval, &mut mt);
            if hr.is_err() {
                return hr;
            }
        }
        // SAFETY: ppmt is writable (precondition); mt was just built.
        unsafe {
            *ppmt = alloc_media_type(&mt);
            free_media_type_fields(&mut mt);
            if (*ppmt).is_null() {
                E_OUTOFMEMORY
            } else {
                S_OK
            }
        }
    }

    /// # Safety
    /// `count` and `size` must be valid for writing.
    pub unsafe fn get_number_of_capabilities(&self, count: *mut i32, size: *mut i32) -> HRESULT {
        if count.is_null() || size.is_null() {
            return E_POINTER;
        }
        let _st = lock(&self.state);
        // SAFETY: both writable (precondition).
        unsafe {
            *count = self.offered().len() as i32;
            *size = size_of::<VIDEO_STREAM_CONFIG_CAPS>() as i32;
        }
        S_OK
    }

    /// # Safety
    /// `ppmt` writable pointer, `caps` writable for `sizeof(VIDEO_STREAM_CONFIG_CAPS)` bytes.
    pub unsafe fn get_stream_caps(
        &self,
        index: i32,
        ppmt: *mut *mut AM_MEDIA_TYPE,
        caps: *mut u8,
    ) -> HRESULT {
        if ppmt.is_null() || caps.is_null() {
            return E_POINTER;
        }
        let _st = lock(&self.state);
        let offered = self.offered();
        if index < 0 || index as usize >= offered.len() {
            return S_FALSE;
        }
        let f = offered[index as usize];
        let mut mt = AM_MEDIA_TYPE::default();
        let hr = build(&f, DEFAULT_INTERVAL, &mut mt);
        if hr.is_err() {
            return hr;
        }
        // SAFETY: ppmt and caps are writable (precondition); mt was just built.
        unsafe {
            *ppmt = alloc_media_type(&mt);
            free_media_type_fields(&mut mt);
            if (*ppmt).is_null() {
                return E_OUTOFMEMORY;
            }
        }
        let input = SIZE {
            cx: f.width as i32,
            cy: f.height as i32,
        };
        let sb = f.sample_bytes() as u64 * 8 * 10_000_000;
        let c = VIDEO_STREAM_CONFIG_CAPS {
            guid: FORMAT_VideoInfo,
            VideoStandard: 0,
            InputSize: input,
            MinCroppingSize: input,
            MaxCroppingSize: input,
            CropGranularityX: 1,
            CropGranularityY: 1,
            CropAlignX: 1,
            CropAlignY: 1,
            MinOutputSize: input,
            MaxOutputSize: input,
            OutputGranularityX: 1,
            OutputGranularityY: 1,
            StretchTapsX: 0,
            StretchTapsY: 0,
            ShrinkTapsX: 0,
            ShrinkTapsY: 0,
            MinFrameInterval: MIN_INTERVAL,
            MaxFrameInterval: MAX_INTERVAL,
            MinBitsPerSecond: (i32::MAX as u64).min(sb / MAX_INTERVAL as u64) as i32,
            MaxBitsPerSecond: (i32::MAX as u64).min(sb / MIN_INTERVAL as u64) as i32,
        };
        // SAFETY: caps is writable for the whole structure (precondition); it may be unaligned.
        unsafe { ptr::write_unaligned(caps as *mut VIDEO_STREAM_CONFIG_CAPS, c) };
        S_OK
    }

    // ---- Streaming ----

    /// `Activate`: the worker starts in Pause (a live source delivers while paused; the renderer holds the
    /// sample until Run) and ends in Stop.
    pub fn activate(self: &Arc<Self>) -> HRESULT {
        let st = lock(&self.state);
        let Some(allocator) = st.allocator.as_ref() else {
            return VFW_E_NOT_CONNECTED;
        };
        if st.connected.is_none() {
            return VFW_E_NOT_CONNECTED;
        }
        let mut thread = lock(&self.thread);
        if thread.is_some() {
            return S_OK;
        }
        let hr = self.reader.open(st.format.width, st.format.height);
        if hr.is_err() {
            return hr;
        }
        // SAFETY: plain COM call on a live allocator.
        if let Err(e) = unsafe { allocator.Commit() } {
            return e.code();
        }
        self.quit.store(false, SeqCst);
        self.reader.set_active(true); // wakes the host: it opens its source for us
        let me = Arc::clone(self);
        match std::thread::Builder::new().spawn(move || {
            // A panic must end this thread quietly, not the app.
            let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| worker(&me)));
        }) {
            Ok(h) => {
                *thread = Some(h);
                S_OK
            }
            Err(_) => {
                self.reader.set_active(false);
                E_FAIL
            }
        }
    }

    pub fn run(&self, start: i64, clock: Option<IReferenceClock>) {
        {
            let mut c = lock(&self.clock);
            c.start = start;
            c.clock = clock;
        }
        self.running.store(true, SeqCst);
    }

    fn stop_worker(&self) {
        self.quit.store(true, SeqCst);
        self.running.store(false, SeqCst);
        let allocator = lock(&self.state).allocator.clone();
        if let Some(a) = allocator {
            // SAFETY: plain COM call. GetBuffer returns; Receive is not waited for.
            let _ = unsafe { a.Decommit() };
        }
        if !self.timer.is_invalid() {
            // SAFETY: the timer handle is ours and open until the pin is dropped.
            let _ = unsafe { CancelWaitableTimer(self.timer) };
        }
        let handle = lock(&self.thread).take();
        if let Some(h) = handle {
            // A thread cannot wait for itself (the last release of a pin on its own worker).
            if h.thread().id() != std::thread::current().id() {
                let _ = h.join();
            }
        }
        self.reader.set_active(false);
    }

    pub fn inactive(&self) {
        self.stop_worker();
        lock(&self.clock).clock = None;
    }

    /// The filter is going away while something still holds the pin.
    pub fn orphan(&self) {
        self.stop_worker();
        *lock(&self.link) = None;
    }
}

/// `OutputPin::Worker`.
fn worker(inner: &Arc<PinInner>) {
    // SAFETY: plain Win32 call on this thread.
    let _ = unsafe { SetThreadPriority(GetCurrentThread(), THREAD_PRIORITY_ABOVE_NORMAL) };
    let (f, interval, allocator, input) = {
        let st = lock(&inner.state);
        (
            st.format,
            st.interval,
            st.allocator.clone(),
            st.input.clone(),
        )
    };
    let (Some(allocator), Some(input)) = (allocator, input) else {
        return;
    };
    let nv12_bytes = (f.width * f.height * 3 / 2) as usize;
    let mut nv12 = if f.fourcc != FOURCC_NV12 {
        vec![0u8; nv12_bytes]
    } else {
        Vec::new()
    };

    let wait_until = |target: i64, extra: Option<HANDLE>| {
        // SAFETY: plain Win32 calls; the timer handle outlives the thread (closed after the join).
        unsafe {
            let now = MFGetSystemTime();
            if target <= now {
                return;
            }
            let due = -(target - now);
            let _ = SetWaitableTimer(inner.timer, &due, 0, None, None, false);
            let handles = [inner.timer, extra.unwrap_or_default()];
            WaitForMultipleObjects(&handles[..if extra.is_some() { 2 } else { 1 }], false, 200);
        }
    };
    let event = || {
        let e = inner.reader.frame_event();
        if e.is_invalid() {
            None
        } else {
            Some(e)
        }
    };

    let mut due: i64 = 0;
    let mut first = true;
    while !inner.quit.load(SeqCst) {
        // Paced like the Windows 11 media source: one frame per interval at the app's rate; a
        // source slower than that is waited for up to half an interval instead of repeated.
        // SAFETY: plain Win32 call.
        let now = unsafe { MFGetSystemTime() };
        if due == 0 || now - due > interval {
            due = now;
        } else {
            wait_until(due, None);
            if inner.reader.latest_frame() == inner.reader.delivered_frame() {
                wait_until(due + interval / 2, event());
            }
        }
        due += interval;
        if inner.quit.load(SeqCst) {
            break;
        }

        let mut sample: Option<IMediaSample> = None;
        // SAFETY: plain COM call; `sample` is a valid out slot.
        let got = unsafe { allocator.GetBuffer(&mut sample, None, None, 0) };
        let Some(sample) = sample.filter(|_| got.is_ok()) else {
            if inner.quit.load(SeqCst) {
                break;
            }
            // SAFETY: Sleep has no preconditions.
            unsafe { Sleep(5) };
            continue;
        };
        // SAFETY: sample is a live buffer; its pointer stays valid while we hold the sample.
        let (data, size) = unsafe {
            match sample.GetPointer() {
                Ok(p) => (p, sample.GetSize()),
                Err(_) => (ptr::null_mut(), 0),
            }
        };
        if data.is_null() || size < f.sample_bytes() as i32 {
            // SAFETY: Sleep has no preconditions.
            unsafe { Sleep(5) };
            continue;
        }
        // SAFETY: plain Win32 call.
        let stamp = unsafe { MFGetSystemTime() };
        // SAFETY: the sample buffer holds at least sample_bytes() (checked above) and nobody else writes it.
        let out = unsafe { core::slice::from_raw_parts_mut(data, f.sample_bytes() as usize) };
        if f.fourcc == FOURCC_NV12 {
            inner
                .reader
                .fill(out, f.width as usize, f.width, f.height, stamp);
        } else {
            inner
                .reader
                .fill(&mut nv12, f.width as usize, f.width, f.height, stamp);
            if f.fourcc == FOURCC_YUY2 {
                nv12_to_yuy2(&nv12, f.width, f.height, out);
            } else {
                nv12_to_i420(&nv12, f.width, f.height, out);
            }
        }
        // SAFETY: plain COM calls on the live sample.
        unsafe {
            let _ = sample.SetActualDataLength(f.sample_bytes() as i32);
            let _ = sample.SetSyncPoint(true);
            let _ = sample.SetPreroll(false);
            let _ = sample.SetDiscontinuity(first);
            let _ = sample.SetMediaTime(None, None);
        }
        {
            // Stream time (the graph clock minus the run start): the renderer shows the frame at
            // once since it is "now". None while paused or without a clock.
            let c = lock(&inner.clock);
            let mut t: i64 = 0;
            let timed = match (&c.clock, inner.running.load(SeqCst)) {
                // SAFETY: plain COM call on the clock.
                (Some(clock), true) => unsafe { clock.GetTime() }.map(|v| t = v).is_ok(),
                _ => false,
            };
            // SAFETY: plain COM calls on the live sample; the pointers are valid for the call.
            unsafe {
                if timed {
                    let start = t - c.start;
                    let end = start + interval;
                    let _ = sample.SetTime(Some(&start), Some(&end));
                } else {
                    let _ = sample.SetTime(None, None);
                }
            }
        }
        // SAFETY: raw vtable call so S_FALSE (flushing / stopping) stays distinguishable from S_OK.
        let hr = unsafe { (Interface::vtable(&input).Receive)(input.as_raw(), sample.as_raw()) };
        drop(sample);
        if hr == S_OK {
            first = false;
        } else {
            // S_FALSE: the app is flushing or stopping, and asks for no more for now.
            if inner.quit.load(SeqCst) {
                break;
            }
            // SAFETY: Sleep has no preconditions.
            unsafe { Sleep(10) };
        }
    }
}

// ---- the COM object ---------------------------------------------------------------------------

/// `OutputPin`: IPin, IAMStreamConfig, IKsPropertySet, IQualityControl.
#[implement(
    IPinX,
    IAMStreamConfigX,
    IKsPropertySetX,
    IQualityControlX,
    Agile = false
)]
pub struct OutputPin {
    pub(crate) inner: Arc<PinInner>,
}

impl OutputPin {
    /// `RuntimeClassInitialize`: a waitable timer for the pacing, or its error.
    pub fn new() -> windows::core::Result<Self> {
        // SAFETY: plain Win32 call.
        let timer = unsafe { CreateWaitableTimerW(None, true, None) }?;
        Ok(Self {
            inner: Arc::new(PinInner {
                state: Mutex::new(PinState {
                    connected: None,
                    input: None,
                    allocator: None,
                    format: PinFormat::default(),
                    interval: DEFAULT_INTERVAL,
                    format_set: false,
                }),
                link: Mutex::new(None),
                reader: FrameReader::new(),
                thread: Mutex::new(None),
                quit: AtomicBool::new(false),
                running: AtomicBool::new(false),
                timer,
                clock: Mutex::new(ClockState {
                    clock: None,
                    start: 0,
                }),
            }),
        })
    }

    /// Lets the pin know its filter (not counted).
    pub(crate) fn set_filter(&self, iface: *mut c_void, shared: Arc<FilterShared>) {
        *lock(&self.inner.link) = Some(FilterLink { iface, shared });
    }
}

impl Drop for OutputPin {
    fn drop(&mut self) {
        self.inner.stop_worker();
        if !self.inner.timer.is_invalid() {
            // SAFETY: the timer is ours; the worker is joined.
            let _ = unsafe { CloseHandle(self.inner.timer) };
        }
    }
}

impl OutputPin_Impl {
    fn this_pin(&self) -> Option<IPin> {
        self.to_interface::<IPinX>().cast::<IPin>().ok()
    }
}

// SAFETY (all methods of the four impls below): COM calls the methods with the pointers the interface
// contract allows (null or valid); each one is wrapped in `guard` so no panic crosses the boundary.
impl IPinX_Impl for OutputPin_Impl {
    unsafe fn Connect(&self, receiver: *mut c_void, pmt: *const AM_MEDIA_TYPE) -> HRESULT {
        guard(|| {
            // SAFETY: see the comment above the impl.
            let Some(receiver) = (unsafe { IPin::from_raw_borrowed(&receiver) }) else {
                return E_POINTER;
            };
            let Some(this) = self.this_pin() else {
                return E_FAIL;
            };
            // SAFETY: see above.
            unsafe { self.inner.connect(&this, receiver, pmt) }
        })
    }
    unsafe fn ReceiveConnection(
        &self,
        _connector: *mut c_void,
        _pmt: *const AM_MEDIA_TYPE,
    ) -> HRESULT {
        E_UNEXPECTED
    }
    unsafe fn Disconnect(&self) -> HRESULT {
        guard(|| self.inner.disconnect())
    }
    unsafe fn ConnectedTo(&self, pin: *mut *mut c_void) -> HRESULT {
        // SAFETY: see above.
        guard(|| unsafe { self.inner.connected_to(pin) })
    }
    unsafe fn ConnectionMediaType(&self, pmt: *mut AM_MEDIA_TYPE) -> HRESULT {
        // SAFETY: see above.
        guard(|| unsafe { self.inner.connection_media_type(pmt) })
    }
    unsafe fn QueryPinInfo(&self, info: *mut PIN_INFO) -> HRESULT {
        // SAFETY: see above.
        guard(|| unsafe { self.inner.query_pin_info(info) })
    }
    unsafe fn QueryDirection(&self, dir: *mut i32) -> HRESULT {
        if dir.is_null() {
            return E_POINTER;
        }
        // SAFETY: dir is writable (non-null, contract).
        unsafe { *dir = PINDIR_OUTPUT.0 };
        S_OK
    }
    unsafe fn QueryId(&self, id: *mut PWSTR) -> HRESULT {
        // SAFETY: see above.
        guard(|| unsafe { self.inner.query_id(id) })
    }
    unsafe fn QueryAccept(&self, pmt: *const AM_MEDIA_TYPE) -> HRESULT {
        // SAFETY: see above.
        guard(|| unsafe { self.inner.query_accept(pmt) })
    }
    unsafe fn EnumMediaTypes(&self, out: *mut *mut c_void) -> HRESULT {
        guard(|| {
            if out.is_null() {
                return E_POINTER;
            }
            let types = self.inner.enum_media_types();
            // SAFETY: out is non-null and writable (checked above).
            unsafe { crate::filter::new_media_type_enumerator(types, 0, out) }
        })
    }
    unsafe fn QueryInternalConnections(
        &self,
        _pins: *mut *mut c_void,
        _count: *mut u32,
    ) -> HRESULT {
        E_NOTIMPL
    }
    unsafe fn EndOfStream(&self) -> HRESULT {
        E_UNEXPECTED
    }
    unsafe fn BeginFlush(&self) -> HRESULT {
        E_UNEXPECTED
    }
    unsafe fn EndFlush(&self) -> HRESULT {
        E_UNEXPECTED
    }
    unsafe fn NewSegment(&self, _start: i64, _stop: i64, _rate: f64) -> HRESULT {
        E_UNEXPECTED
    }
}

impl IAMStreamConfigX_Impl for OutputPin_Impl {
    unsafe fn SetFormat(&self, pmt: *mut AM_MEDIA_TYPE) -> HRESULT {
        guard(|| {
            let Some(this) = self.this_pin() else {
                return E_FAIL;
            };
            // SAFETY: see above.
            unsafe { self.inner.set_format(&this, pmt) }
        })
    }
    unsafe fn GetFormat(&self, pmt: *mut *mut AM_MEDIA_TYPE) -> HRESULT {
        // SAFETY: see above.
        guard(|| unsafe { self.inner.get_format(pmt) })
    }
    unsafe fn GetNumberOfCapabilities(&self, count: *mut i32, size: *mut i32) -> HRESULT {
        // SAFETY: see above.
        guard(|| unsafe { self.inner.get_number_of_capabilities(count, size) })
    }
    unsafe fn GetStreamCaps(
        &self,
        index: i32,
        pmt: *mut *mut AM_MEDIA_TYPE,
        caps: *mut u8,
    ) -> HRESULT {
        // SAFETY: see above.
        guard(|| unsafe { self.inner.get_stream_caps(index, pmt, caps) })
    }
}

/// The pin category, which capture graph builders look for.
impl IKsPropertySetX_Impl for OutputPin_Impl {
    unsafe fn Set(
        &self,
        _set: *const GUID,
        _id: u32,
        _instance: *mut c_void,
        _instance_bytes: u32,
        _data: *mut c_void,
        _data_bytes: u32,
    ) -> HRESULT {
        E_NOTIMPL
    }
    unsafe fn Get(
        &self,
        set: *const GUID,
        id: u32,
        _instance: *mut c_void,
        _instance_bytes: u32,
        data: *mut c_void,
        data_bytes: u32,
        returned: *mut u32,
    ) -> HRESULT {
        // SAFETY: set is a valid GUID reference (contract), data/returned null or writable.
        unsafe {
            if set.is_null() {
                return E_POINTER;
            }
            if *set != AMPROPSETID_Pin {
                return E_PROP_SET_UNSUPPORTED;
            }
            if id != AMPROPERTY_PIN_CATEGORY.0 as u32 {
                return E_PROP_ID_UNSUPPORTED;
            }
            if !returned.is_null() {
                *returned = size_of::<GUID>() as u32;
            }
            if data.is_null() {
                return S_OK;
            }
            if (data_bytes as usize) < size_of::<GUID>() {
                return E_UNEXPECTED;
            }
            ptr::write_unaligned(data as *mut GUID, PIN_CATEGORY_CAPTURE);
        }
        S_OK
    }
    unsafe fn QuerySupported(&self, set: *const GUID, id: u32, support: *mut u32) -> HRESULT {
        // SAFETY: as in Get.
        unsafe {
            if set.is_null() {
                return E_POINTER;
            }
            if *set != AMPROPSETID_Pin {
                return E_PROP_SET_UNSUPPORTED;
            }
            if id != AMPROPERTY_PIN_CATEGORY.0 as u32 {
                return E_PROP_ID_UNSUPPORTED;
            }
            if !support.is_null() {
                *support = KSPROPERTY_SUPPORT_GET;
            }
        }
        S_OK
    }
}

impl IQualityControlX_Impl for OutputPin_Impl {
    unsafe fn Notify(&self, _filter: *mut c_void, _q: Quality) -> HRESULT {
        S_OK
    }
    unsafe fn SetSink(&self, _sink: *mut c_void) -> HRESULT {
        S_OK
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sizes_of_the_windows_structs() {
        // The C++ values: VIDEOINFOHEADER 88, VIDEOINFOHEADER2 112, AM_MEDIA_TYPE 72 (x86) / 88 (x64).
        assert_eq!(size_of::<VIDEOINFOHEADER>(), 88);
        assert_eq!(size_of::<VIDEOINFOHEADER2>(), 112);
        assert_eq!(
            size_of::<AM_MEDIA_TYPE>(),
            if cfg!(target_pointer_width = "64") {
                88
            } else {
                72
            }
        );
        assert_eq!(size_of::<VIDEO_STREAM_CONFIG_CAPS>(), 128);
    }

    #[test]
    fn build_then_parse_round_trips() {
        for fourcc in FOURCCS {
            let f = PinFormat {
                fourcc,
                width: 1280,
                height: 720,
            };
            let mut mt = AM_MEDIA_TYPE::default();
            assert_eq!(build(&f, 333333, &mut mt), S_OK);
            let (mut g, mut iv) = (PinFormat::default(), 0);
            // SAFETY: mt was built by `build`.
            assert!(unsafe { parse(&mt, &mut g, &mut iv) });
            assert_eq!((g, iv), (f, 333333));
            // SAFETY: as above.
            unsafe { free_media_type_fields(&mut mt) };
        }
    }

    #[test]
    fn conversions() {
        // 4x2 NV12: Y 0..8, UV (u0 v0 u1 v1) = 100 101 102 103
        let nv12: Vec<u8> = (0..8).chain([100, 101, 102, 103]).collect();
        let mut yuy2 = vec![0u8; 16];
        nv12_to_yuy2(&nv12, 4, 2, &mut yuy2);
        assert_eq!(
            yuy2,
            [0, 100, 1, 101, 2, 102, 3, 103, 4, 100, 5, 101, 6, 102, 7, 103]
        );
        let mut i420 = vec![0u8; 12];
        nv12_to_i420(&nv12, 4, 2, &mut i420);
        assert_eq!(i420, [0, 1, 2, 3, 4, 5, 6, 7, 100, 102, 101, 103]);
    }
}
