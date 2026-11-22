//! Port of `src/vcam/MediaStream.h` / `MediaStream.cpp`: the one video stream of the media source.
//!
//! Same structure as the C++: a worker thread delivers one sample per `RequestSample` token,
//! paced to the rate the app asked for (`pace_until_next_frame`); the state, the token queue and
//! the event queue sit behind one mutex (`m_lock`) with a condition variable.

use std::collections::VecDeque;
use std::ffi::c_void;
use std::sync::{Arc, Condvar, Mutex};
use std::thread::JoinHandle;

use niwcam_shm::{mf_system_time, nv12_bytes, FrameReader};
use windows::core::{implement, Error, Interface, Ref, GUID, HRESULT, PCWSTR};
use windows::Win32::Foundation::{
    CloseHandle, E_INVALIDARG, E_POINTER, E_UNEXPECTED, HANDLE, INVALID_HANDLE_VALUE,
};
use windows::Win32::Media::KernelStreaming::PINNAME_VIDEO_CAPTURE;
use windows::Win32::Media::MediaFoundation::*;
use windows::Win32::System::Com::StructuredStorage::PROPVARIANT;
use windows::Win32::System::Threading::{
    CreateWaitableTimerExW, CreateWaitableTimerW, GetCurrentThread, SetThreadPriority,
    SetWaitableTimer, Sleep, WaitForMultipleObjects, CREATE_WAITABLE_TIMER_HIGH_RESOLUTION,
    INFINITE, THREAD_PRIORITY_ABOVE_NORMAL, TIMER_ALL_ACCESS,
};

use crate::common::{check, guard, lock, vlog};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct VideoFormat {
    pub width: u32,
    pub height: u32,
    pub fps_num: u32,
    pub fps_den: u32,
}

const MF_E_SHUTDOWN_HR: HRESULT = MF_E_SHUTDOWN;

/// `MFGetAttributeSize` / `MFGetAttributeRatio`: two u32 packed in a u64.
fn get_packed(attrs: &IMFAttributes, key: &GUID) -> windows::core::Result<(u32, u32)> {
    // SAFETY: valid interface and key.
    let v = unsafe { attrs.GetUINT64(key) }?;
    Ok(((v >> 32) as u32, v as u32))
}

fn set_packed(attrs: &IMFAttributes, key: &GUID, hi: u32, lo: u32) -> windows::core::Result<()> {
    // SAFETY: valid interface and key.
    unsafe { attrs.SetUINT64(key, ((hi as u64) << 32) | lo as u64) }
}

fn create_nv12_type(f: &VideoFormat) -> windows::core::Result<IMFMediaType> {
    // SAFETY: plain MF calls on objects we own.
    unsafe {
        let ty = check!(MFCreateMediaType());
        check!(ty.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video));
        check!(ty.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_NV12));
        check!(ty.SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32));
        check!(ty.SetUINT32(&MF_MT_ALL_SAMPLES_INDEPENDENT, 1));
        check!(ty.SetUINT32(&MF_MT_FIXED_SIZE_SAMPLES, 1));
        check!(ty.SetUINT32(&MF_MT_DEFAULT_STRIDE, f.width));
        check!(ty.SetUINT32(&MF_MT_SAMPLE_SIZE, nv12_bytes(f.width, f.height) as u32));
        check!(set_packed(
            &ty.cast()?,
            &MF_MT_FRAME_SIZE,
            f.width,
            f.height
        ));
        check!(set_packed(
            &ty.cast()?,
            &MF_MT_FRAME_RATE,
            f.fps_num,
            f.fps_den
        ));
        check!(set_packed(&ty.cast()?, &MF_MT_PIXEL_ASPECT_RATIO, 1, 1));
        let bitrate = nv12_bytes(f.width, f.height) * 8 * f.fps_num as u64
            / if f.fps_den != 0 { f.fps_den as u64 } else { 1 };
        check!(ty.SetUINT32(
            &MF_MT_AVG_BITRATE,
            if bitrate > u32::MAX as u64 {
                u32::MAX
            } else {
                bitrate as u32
            }
        ));
        Ok(ty)
    }
}

fn set_stream_category(attrs: &IMFAttributes, stream_id: u32) -> windows::core::Result<()> {
    // SAFETY: valid interface and keys.
    unsafe {
        check!(attrs.SetGUID(&MF_DEVICESTREAM_STREAM_CATEGORY, &PINNAME_VIDEO_CAPTURE));
        check!(attrs.SetUINT32(&MF_DEVICESTREAM_STREAM_ID, stream_id));
        check!(attrs.SetUINT32(&MF_DEVICESTREAM_FRAMESERVER_SHARED, 1));
        check!(attrs.SetUINT32(
            &MF_DEVICESTREAM_ATTRIBUTE_FRAMESOURCE_TYPES,
            MFFrameSourceTypes_Color.0 as u32
        ));
    }
    Ok(())
}

/// A kernel handle closed on drop.
struct TimerHandle(HANDLE);

// SAFETY: a waitable-timer handle is a number usable from any thread.
unsafe impl Send for TimerHandle {}
// SAFETY: as above; only passed to thread-safe kernel calls.
unsafe impl Sync for TimerHandle {}

impl Drop for TimerHandle {
    fn drop(&mut self) {
        if !self.0.is_invalid() {
            // SAFETY: we own the handle and close it once.
            unsafe {
                let _ = CloseHandle(self.0);
            }
        }
    }
}

/// COM objects of Media Foundation are free-threaded (the C++ shares them between Frame Server's
/// threads and the worker with a plain mutex); this lets the state that holds them cross threads.
struct Com<T>(T);

// SAFETY: see above: the MF objects held here (event queue, descriptors, allocator, types,
// attributes, the parent source, tokens) are agile and are only used under `Inner::lock`.
unsafe impl<T> Send for Com<T> {}

struct State {
    pending_tokens: VecDeque<Option<Com<windows::core::IUnknown>>>, // nullptr tokens allowed
    quit: bool,
    parent: Option<Com<IMFMediaSource>>,
    event_queue: Option<Com<IMFMediaEventQueue>>,
    attributes: Option<Com<IMFAttributes>>,
    descriptor: Option<Com<IMFStreamDescriptor>>,
    allocator: Option<Com<IMFVideoSampleAllocator>>,
    current_type: Option<Com<IMFMediaType>>,
    shutdown: bool,
    state: MF_STREAM_STATE,
    width: u32,
    height: u32,
    interval_100ns: i64,
    last_delivery_100ns: i64,
    /// When the next sample is due (0: deliver at once).
    next_due_100ns: i64,
}

struct Inner {
    lock: Mutex<State>,
    cv: Condvar,
    reader: Arc<Mutex<FrameReader>>,
    timer: TimerHandle,
    stream_id: u32,
    worker: Mutex<Option<JoinHandle<()>>>,
}

#[implement(IMFMediaStream2, Agile = false)]
pub struct MediaStream {
    inner: Arc<Inner>,
}

impl MediaStream {
    /// `RuntimeClassInitialize`.
    pub fn create(
        source: IMFMediaSource,
        stream_id: u32,
        formats: &[VideoFormat],
        reader: Arc<Mutex<FrameReader>>,
    ) -> windows::core::Result<windows::core::ComObject<MediaStream>> {
        if formats.is_empty() {
            return Err(Error::from_hresult(E_INVALIDARG));
        }

        let mut types: Vec<Option<IMFMediaType>> = Vec::new();
        for f in formats {
            types.push(Some(create_nv12_type(f)?));
        }

        // SAFETY: plain MF calls on objects we own.
        let descriptor = unsafe {
            let descriptor = MFCreateStreamDescriptor(stream_id, &types);
            let descriptor = match descriptor {
                Ok(d) => {
                    let handler = d.GetMediaTypeHandler();
                    let r = handler.and_then(|h| h.SetCurrentMediaType(types[0].as_ref()));
                    match r {
                        Ok(()) => Ok(d),
                        Err(e) => Err(e),
                    }
                }
                Err(e) => Err(e),
            };
            check!(descriptor)
        };
        drop(types);

        check!(set_stream_category(&descriptor.cast()?, stream_id));
        // SAFETY: plain MF calls.
        let (attributes, event_queue) = unsafe {
            let mut attributes: Option<IMFAttributes> = None;
            check!(MFCreateAttributes(&mut attributes, 4));
            let attributes = attributes.ok_or_else(|| Error::from_hresult(E_UNEXPECTED))?;
            check!(set_stream_category(&attributes, stream_id));
            let event_queue = check!(MFCreateEventQueue());
            (attributes, event_queue)
        };

        // SAFETY: plain kernel calls; the handle is owned by `TimerHandle`.
        let timer = unsafe {
            let mut t = CreateWaitableTimerExW(
                None,
                PCWSTR::null(),
                CREATE_WAITABLE_TIMER_HIGH_RESOLUTION,
                TIMER_ALL_ACCESS.0,
            );
            if t.is_err() {
                t = CreateWaitableTimerW(None, false, PCWSTR::null());
            }
            t?
        };

        let inner = Arc::new(Inner {
            lock: Mutex::new(State {
                pending_tokens: VecDeque::new(),
                quit: false,
                parent: Some(Com(source)),
                event_queue: Some(Com(event_queue)),
                attributes: Some(Com(attributes)),
                descriptor: Some(Com(descriptor)),
                allocator: None,
                current_type: None,
                shutdown: false,
                state: MF_STREAM_STATE_STOPPED,
                width: 0,
                height: 0,
                interval_100ns: 333333,
                last_delivery_100ns: 0,
                next_due_100ns: 0,
            }),
            cv: Condvar::new(),
            reader,
            timer: TimerHandle(timer),
            stream_id,
            worker: Mutex::new(None),
        });

        let worker_inner = inner.clone();
        let handle = std::thread::Builder::new()
            .name("CamStudioVCam worker".into())
            .spawn(move || worker_loop(worker_inner))
            .map_err(|_| Error::from_hresult(E_UNEXPECTED))?;
        *lock(&inner.worker) = Some(handle);

        Ok(windows::core::ComObject::new(MediaStream { inner }))
    }

    pub fn id(&self) -> u32 {
        self.inner.stream_id
    }

    /// `Attributes()`: a reference to the stream's attribute store.
    pub fn attributes(&self) -> Option<IMFAttributes> {
        lock(&self.inner.lock)
            .attributes
            .as_ref()
            .map(|a| a.0.clone())
    }

    /// `Start(mediaType)`.
    pub fn start(&self, media_type: &IMFMediaType) -> windows::core::Result<()> {
        let mut st = lock(&self.inner.lock);
        check!(check_shutdown_locked(&st));
        start_locked(&self.inner, &mut st, Some(media_type), true)
    }

    /// `Stop(sendEvent)`.
    pub fn stop(&self, send_event: bool) -> windows::core::Result<()> {
        let mut st = lock(&self.inner.lock);
        check!(check_shutdown_locked(&st));
        stop_locked(&self.inner, &mut st, send_event)
    }

    /// `Shutdown()`.
    pub fn shutdown(&self) {
        shutdown_inner(&self.inner);
    }

    /// `SetSampleAllocator`.
    pub fn set_sample_allocator(
        &self,
        allocator: &IMFVideoSampleAllocator,
    ) -> windows::core::Result<()> {
        let mut st = lock(&self.inner.lock);
        check!(check_shutdown_locked(&st));
        if st.state == MF_STREAM_STATE_RUNNING {
            return Err(Error::from_hresult(MF_E_INVALIDREQUEST));
        }
        st.allocator = Some(Com(allocator.clone()));
        Ok(())
    }
}

impl Drop for MediaStream {
    fn drop(&mut self) {
        crate::common::guard_drop(|| shutdown_inner(&self.inner));
    }
}

fn shutdown_inner(inner: &Arc<Inner>) {
    {
        let mut st = lock(&inner.lock);
        if st.shutdown {
            return;
        }
        st.shutdown = true;
        st.quit = true;
        st.state = MF_STREAM_STATE_STOPPED;
        st.pending_tokens.clear();
        lock(&inner.reader).set_active(false);
        if let Some(q) = &st.event_queue {
            // SAFETY: valid event queue.
            unsafe {
                let _ = q.0.Shutdown();
            }
        }
    }
    inner.cv.notify_all();
    // Wake the worker if it is pacing.
    let due: i64 = -1;
    // SAFETY: valid timer handle owned by `inner`; `due` outlives the call.
    unsafe {
        let _ = SetWaitableTimer(inner.timer.0, &due, 0, None, None, false);
    }
    let handle = lock(&inner.worker).take();
    if let Some(h) = handle {
        if h.thread().id() != std::thread::current().id() {
            let _ = h.join();
        }
    }

    let mut st = lock(&inner.lock);
    st.event_queue = None;
    st.parent = None;
    st.allocator = None;
    st.current_type = None;
    st.descriptor = None;
    st.attributes = None;
    // The timer handle closes when the last reference to `inner` goes (the worker is gone).
}

fn check_shutdown_locked(st: &State) -> windows::core::Result<()> {
    if st.shutdown {
        return Err(Error::from_hresult(MF_E_SHUTDOWN_HR));
    }
    if st.event_queue.is_none() {
        return Err(Error::from_hresult(E_UNEXPECTED));
    }
    Ok(())
}

fn queue_of(st: &State) -> windows::core::Result<&IMFMediaEventQueue> {
    st.event_queue
        .as_ref()
        .map(|q| &q.0)
        .ok_or_else(|| Error::from_hresult(E_UNEXPECTED))
}

fn start_locked(
    inner: &Inner,
    st: &mut State,
    media_type: Option<&IMFMediaType>,
    send_event: bool,
) -> windows::core::Result<()> {
    let mut type_changed = false;
    if let Some(mt) = media_type {
        let mut same = windows::core::BOOL(0);
        if let Some(cur) = &st.current_type {
            // SAFETY: valid interfaces; the result is ignored like the C++.
            unsafe {
                let attrs: windows::core::Result<IMFAttributes> = mt.cast();
                if let Ok(a) = attrs {
                    if let Ok(b) = cur.0.Compare(&a, MF_ATTRIBUTES_MATCH_ALL_ITEMS) {
                        same = b;
                    }
                }
            }
        }
        if !same.as_bool() {
            st.current_type = Some(Com(mt.clone()));
            type_changed = true;
        }
    }
    if st.current_type.is_none() {
        let descriptor = st
            .descriptor
            .as_ref()
            .ok_or_else(|| Error::from_hresult(E_UNEXPECTED))?;
        // SAFETY: valid descriptor.
        let ty = unsafe {
            let handler = check!(descriptor.0.GetMediaTypeHandler());
            check!(handler.GetCurrentMediaType())
        };
        st.current_type = Some(Com(ty));
        type_changed = true;
    }

    if st.state != MF_STREAM_STATE_RUNNING || type_changed {
        if st.allocator.is_none() {
            let mut raw: *mut c_void = std::ptr::null_mut();
            // SAFETY: `raw` receives a new reference to the requested interface.
            let alloc = unsafe {
                check!(MFCreateVideoSampleAllocatorEx(
                    &IMFVideoSampleAllocator::IID,
                    &mut raw
                ));
                // SAFETY: MFCreateVideoSampleAllocatorEx returned a counted pointer of that IID.
                IMFVideoSampleAllocator::from_raw(raw)
            };
            st.allocator = Some(Com(alloc));
        }
        let cur = st.current_type.as_ref().map(|t| t.0.clone());
        let cur = cur.ok_or_else(|| Error::from_hresult(E_UNEXPECTED))?;
        let alloc = st.allocator.as_ref().map(|a| a.0.clone());
        let alloc = alloc.ok_or_else(|| Error::from_hresult(E_UNEXPECTED))?;
        // SAFETY: valid allocator and media type.
        unsafe {
            let _ = alloc.UninitializeSampleAllocator();
            check!(alloc.InitializeSampleAllocator(10, &cur));
        }

        let attrs: IMFAttributes = cur.cast()?;
        let (w, h) = check!(get_packed(&attrs, &MF_MT_FRAME_SIZE));
        st.width = w;
        st.height = h;
        // MFGetAttributeRatio leaves num/den alone on failure: 30/1.
        let (num, den) = get_packed(&attrs, &MF_MT_FRAME_RATE).unwrap_or((30, 1));
        st.interval_100ns = if num > 0 {
            10_000_000i64 * (if den != 0 { den } else { 1 }) as i64 / num as i64
        } else {
            333333
        };
        st.last_delivery_100ns = 0;
        st.next_due_100ns = 0;
        vlog!("Stream start {}x{} @ {}/{}", w, h, num, den);
    }

    if send_event {
        let q = queue_of(st)?;
        // SAFETY: valid queue; a null value is allowed.
        check!(unsafe {
            q.QueueEventParamVar(
                MEStreamStarted.0 as u32,
                &GUID::zeroed(),
                HRESULT(0),
                std::ptr::null(),
            )
        });
    }
    st.state = MF_STREAM_STATE_RUNNING;
    lock(&inner.reader).set_active(true);
    inner.cv.notify_one();
    Ok(())
}

fn stop_locked(inner: &Inner, st: &mut State, send_event: bool) -> windows::core::Result<()> {
    st.state = MF_STREAM_STATE_STOPPED;
    st.pending_tokens.clear();
    lock(&inner.reader).set_active(false);
    if send_event {
        let q = queue_of(st)?;
        // SAFETY: valid queue; a null value is allowed.
        check!(unsafe {
            q.QueueEventParamVar(
                MEStreamStopped.0 as u32,
                &GUID::zeroed(),
                HRESULT(0),
                std::ptr::null(),
            )
        });
    }
    Ok(())
}

// ---- IMFMediaEventGenerator ------------------------------------------------

impl IMFMediaEventGenerator_Impl for MediaStream_Impl {
    fn GetEvent(
        &self,
        flags: MEDIA_EVENT_GENERATOR_GET_EVENT_FLAGS,
    ) -> windows::core::Result<IMFMediaEvent> {
        guard(|| {
            // GetEvent may block, so do not hold the lock while waiting.
            let queue = {
                let st = lock(&self.inner.lock);
                check!(check_shutdown_locked(&st));
                queue_of(&st)?.clone()
            };
            // SAFETY: valid queue.
            unsafe { queue.GetEvent(flags.0) }
        })
    }

    fn BeginGetEvent(
        &self,
        callback: Ref<IMFAsyncCallback>,
        state: Ref<windows::core::IUnknown>,
    ) -> windows::core::Result<()> {
        guard(|| {
            let st = lock(&self.inner.lock);
            check!(check_shutdown_locked(&st));
            // SAFETY: valid queue; the callback and state are the caller's.
            unsafe { queue_of(&st)?.BeginGetEvent(callback.as_ref(), state.as_ref()) }
        })
    }

    fn EndGetEvent(&self, result: Ref<IMFAsyncResult>) -> windows::core::Result<IMFMediaEvent> {
        guard(|| {
            let st = lock(&self.inner.lock);
            check!(check_shutdown_locked(&st));
            // SAFETY: valid queue; the result is the caller's.
            unsafe { queue_of(&st)?.EndGetEvent(result.as_ref()) }
        })
    }

    fn QueueEvent(
        &self,
        met: u32,
        ext: *const GUID,
        status: HRESULT,
        pv: *const PROPVARIANT,
    ) -> windows::core::Result<()> {
        guard(|| {
            let st = lock(&self.inner.lock);
            check!(check_shutdown_locked(&st));
            if ext.is_null() {
                return Err(Error::from_hresult(E_POINTER));
            }
            // SAFETY: valid queue; `ext` is non-null; `pv` is the caller's (may be null).
            unsafe { queue_of(&st)?.QueueEventParamVar(met, &*ext, status, pv) }
        })
    }
}

// ---- IMFMediaStream --------------------------------------------------------

impl IMFMediaStream_Impl for MediaStream_Impl {
    fn GetMediaSource(&self) -> windows::core::Result<IMFMediaSource> {
        guard(|| {
            let st = lock(&self.inner.lock);
            check!(check_shutdown_locked(&st));
            st.parent
                .as_ref()
                .map(|p| p.0.clone())
                .ok_or_else(|| Error::from_hresult(E_UNEXPECTED))
        })
    }

    fn GetStreamDescriptor(&self) -> windows::core::Result<IMFStreamDescriptor> {
        guard(|| {
            let st = lock(&self.inner.lock);
            check!(check_shutdown_locked(&st));
            st.descriptor
                .as_ref()
                .map(|d| d.0.clone())
                .ok_or_else(|| Error::from_hresult(E_UNEXPECTED))
        })
    }

    fn RequestSample(&self, token: Ref<windows::core::IUnknown>) -> windows::core::Result<()> {
        guard(|| {
            let mut st = lock(&self.inner.lock);
            check!(check_shutdown_locked(&st));
            if st.state != MF_STREAM_STATE_RUNNING {
                return Err(Error::from_hresult(MF_E_INVALIDREQUEST));
            }

            // Delivered asynchronously by the worker so that pacing to the frame
            // rate never blocks Frame Server's calling thread.
            st.pending_tokens.push_back(token.cloned().map(Com));
            self.inner.cv.notify_one();
            Ok(())
        })
    }
}

// ---- IMFMediaStream2 -------------------------------------------------------

impl IMFMediaStream2_Impl for MediaStream_Impl {
    fn SetStreamState(&self, state: MF_STREAM_STATE) -> windows::core::Result<()> {
        guard(|| {
            let mut st = lock(&self.inner.lock);
            check!(check_shutdown_locked(&st));
            if state == st.state {
                return Ok(());
            }

            match state {
                MF_STREAM_STATE_PAUSED => {
                    if st.state != MF_STREAM_STATE_RUNNING {
                        return Err(Error::from_hresult(MF_E_INVALID_STATE_TRANSITION));
                    }
                    st.state = MF_STREAM_STATE_PAUSED;
                    Ok(())
                }
                MF_STREAM_STATE_RUNNING => start_locked(&self.inner, &mut st, None, false),
                MF_STREAM_STATE_STOPPED => stop_locked(&self.inner, &mut st, false),
                _ => Err(Error::from_hresult(MF_E_INVALID_STATE_TRANSITION)),
            }
        })
    }

    fn GetStreamState(&self) -> windows::core::Result<MF_STREAM_STATE> {
        guard(|| {
            let st = lock(&self.inner.lock);
            check!(check_shutdown_locked(&st));
            Ok(st.state)
        })
    }
}

// ---- Delivery --------------------------------------------------------------

/// `DeliverLocked`: one sample (the latest host frame, or "no signal") for `token`.
fn deliver_locked(
    inner: &Inner,
    st: &mut State,
    token: Option<&windows::core::IUnknown>,
) -> windows::core::Result<()> {
    let allocator = st
        .allocator
        .as_ref()
        .map(|a| a.0.clone())
        .ok_or_else(|| Error::from_hresult(E_UNEXPECTED))?;
    // SAFETY: valid allocator. MF_E_SAMPLEALLOCATOR_EMPTY is retried by the worker.
    let sample = unsafe { allocator.AllocateSample() }?;

    // SAFETY: valid sample.
    let buffer = check!(unsafe { sample.GetBufferByIndex(0) });

    let now = mf_system_time();
    let (width, height) = (st.width, st.height);
    let buffer2d: windows::core::Result<IMF2DBuffer2> = buffer.cast();
    if let Ok(b2) = buffer2d {
        let mut scanline0: *mut u8 = std::ptr::null_mut();
        let mut start: *mut u8 = std::ptr::null_mut();
        let mut pitch: i32 = 0;
        let mut length: u32 = 0;
        // SAFETY: valid buffer; out pointers are locals.
        check!(unsafe {
            b2.Lock2DSize(
                MF2DBuffer_LockFlags_Write,
                &mut scanline0,
                &mut pitch,
                &mut start,
                &mut length,
            )
        });
        if pitch > 0 {
            let len = pitch as usize * (height as usize + height as usize / 2);
            if len <= length as usize {
                // SAFETY: the locked buffer holds `length >= len` bytes from `scanline0`
                // (pitch > 0: scanline 0 is the buffer start) until `Unlock2D`.
                let dst = unsafe { std::slice::from_raw_parts_mut(scanline0, len) };
                lock(&inner.reader).fill(dst, pitch as usize, width, height, now);
            }
        }
        // SAFETY: locked above.
        unsafe {
            let _ = b2.Unlock2D();
        }
    } else {
        let mut data: *mut u8 = std::ptr::null_mut();
        let mut max_len: u32 = 0;
        // SAFETY: valid buffer; out pointers are locals.
        check!(unsafe { buffer.Lock(&mut data, Some(&mut max_len), None) });
        if max_len as u64 >= nv12_bytes(width, height) {
            // SAFETY: the locked buffer holds `max_len` bytes until `Unlock`.
            let dst = unsafe { std::slice::from_raw_parts_mut(data, max_len as usize) };
            lock(&inner.reader).fill(dst, width as usize, width, height, now);
        }
        // SAFETY: locked above.
        unsafe {
            let _ = buffer.Unlock();
            let _ = buffer.SetCurrentLength(nv12_bytes(width, height) as u32);
        }
    }

    // SAFETY: valid sample / queue / token.
    unsafe {
        check!(sample.SetSampleTime(now));
        check!(sample.SetSampleDuration(st.interval_100ns));
        if let Some(t) = token {
            check!(sample.SetUnknown(&MFSampleExtension_Token, t));
        }
        let q = queue_of(st)?;
        check!(q.QueueEventParamUnk(MEMediaSample.0 as u32, &GUID::zeroed(), HRESULT(0), &sample));
    }
    st.last_delivery_100ns = now;
    Ok(())
}

/// The state `PaceUntilNextFrame` keeps between calls (only the worker thread touches it).
#[derive(Default)]
struct Pacer {
    /// The host's frame rate (frames published per second), measured by the worker.
    rate_start_100ns: i64,
    rate_latest: i64,
    rate_count: i64,
    source_fps: f64,
}

/// Sleeps until `target` (MFGetSystemTime units) on the high-resolution timer, or until `extra`
/// (the host's "new frame" event, when there is one) is signalled.
fn wait_until(inner: &Inner, target: i64, extra: Option<HANDLE>) {
    let now = mf_system_time();
    if target <= now {
        return;
    }
    let due: i64 = -(target - now);
    // SAFETY: valid timer handle; `due` outlives the call.
    unsafe {
        let _ = SetWaitableTimer(inner.timer.0, &due, 0, None, None, false);
    }
    let handles = [inner.timer.0, extra.unwrap_or(INVALID_HANDLE_VALUE)];
    let n = if extra.is_some() { 2 } else { 1 };
    // SAFETY: valid handles (the event belongs to the reader, which outlives the worker).
    unsafe {
        let _ = WaitForMultipleObjects(&handles[..n], false, INFINITE);
    }
}

fn frame_event(inner: &Inner) -> Option<HANDLE> {
    let h = lock(&inner.reader).frame_event();
    if h.is_invalid() {
        None
    } else {
        Some(h)
    }
}

fn latest_delivered(inner: &Inner) -> (i64, i64) {
    let r = lock(&inner.reader);
    (r.latest_frame(), r.delivered_frame())
}

/// Waits for a frame newer than the last delivered, up to `limit`. The event is auto-reset and
/// may still be set from a publish already delivered: check again after each wake.
fn wait_for_frame(inner: &Inner, limit: i64) {
    loop {
        let (latest, delivered) = latest_delivered(inner);
        if latest != delivered || mf_system_time() >= limit {
            break;
        }
        wait_until(inner, limit, frame_event(inner));
    }
}

fn pace_until_next_frame(inner: &Inner, p: &mut Pacer, interval: i64) {
    let mut due = lock(&inner.lock).next_due_100ns;

    let now = mf_system_time();
    // The host's rate, from how many frames it published between our requests.
    {
        let latest = lock(&inner.reader).latest_frame();
        if p.rate_start_100ns == 0 || latest < p.rate_latest {
            p.rate_start_100ns = now;
            p.rate_count = 0;
        } else {
            p.rate_count += latest - p.rate_latest;
            if now - p.rate_start_100ns >= 10_000_000 {
                p.source_fps = p.rate_count as f64 * 1e7 / (now - p.rate_start_100ns) as f64;
                p.rate_start_100ns = now;
                p.rate_count = 0;
            }
        }
        p.rate_latest = latest;
    }
    let follow = p.source_fps > 0.0 && p.source_fps <= 1e7 / interval as f64 * 1.2;

    if due == 0 || now - due > interval {
        due = now; // first frame, or the app fell behind: restart the schedule
    } else if follow {
        // Source at the app's rate or slower (a phone, a 30 fps webcam): each frame goes out as
        // it comes, so it never waits for the next tick (up to a whole interval, ~16 ms on
        // average at 30 fps). Never two closer than half an interval, and the schedule re-phases
        // to the frame, so the cadence stays the source's own.
        let last_delivery = lock(&inner.lock).last_delivery_100ns;
        let earliest = last_delivery + interval / 2;
        wait_until(inner, earliest, None);
        wait_for_frame(inner, due + interval / 2);
        let (latest, delivered) = latest_delivered(inner);
        if latest != delivered {
            due = earliest.max(mf_system_time());
        }
    } else {
        // Deliver on a fixed schedule at the rate the app asked for: a 30 fps
        // consumer of a 100 fps source gets one frame every 33 ms, not one per
        // host write (uneven timestamps stutter in apps like Discord).
        wait_until(inner, due, None);
        // Source slower than the consumer: wait a little for a fresh frame
        // rather than repeat the last one, never more than half an interval.
        wait_for_frame(inner, due + interval / 2);
    }
    lock(&inner.lock).next_due_100ns = due + interval;
}

fn worker_loop(inner: Arc<Inner>) {
    // The app's frames must not wait behind its own busy threads on a weak PC.
    // SAFETY: the current thread's pseudo handle.
    unsafe {
        let _ = SetThreadPriority(GetCurrentThread(), THREAD_PRIORITY_ABOVE_NORMAL);
    }
    let mut pacer = Pacer::default();
    loop {
        let step = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            worker_step(&inner, &mut pacer)
        }));
        match step {
            Ok(true) => {}
            Ok(false) => return,
            Err(_) => {
                // A panic must not end the stream silently: tell the app, then go on.
                vlog!("panic in the worker thread");
                let st = lock(&inner.lock);
                if let Some(q) = &st.event_queue {
                    // SAFETY: valid queue.
                    unsafe {
                        let _ = q.0.QueueEventParamVar(
                            MEError.0 as u32,
                            &GUID::zeroed(),
                            windows::Win32::Foundation::E_FAIL,
                            std::ptr::null(),
                        );
                    }
                }
            }
        }
    }
}

/// One turn of `WorkerLoop`. Returns false when the worker must end.
fn worker_step(inner: &Arc<Inner>, pacer: &mut Pacer) -> bool {
    let token;
    let interval;
    {
        let mut st = lock(&inner.lock);
        while !(st.quit || (st.state == MF_STREAM_STATE_RUNNING && !st.pending_tokens.is_empty())) {
            st = inner.cv.wait(st).unwrap_or_else(|e| e.into_inner());
        }
        if st.quit {
            return false;
        }
        token = st.pending_tokens.pop_front().flatten();
        interval = st.interval_100ns;
    }

    pace_until_next_frame(inner, pacer, interval);

    let allocator_empty;
    {
        let mut st = lock(&inner.lock);
        if st.quit {
            return false;
        }
        if st.state != MF_STREAM_STATE_RUNNING {
            return true;
        }
        let hr = deliver_locked(inner, &mut st, token.as_ref().map(|t| &t.0));
        match hr {
            Err(e) if e.code() == MF_E_SAMPLEALLOCATOR_EMPTY => {
                // Downstream still holds every sample; try again shortly.
                st.pending_tokens.push_front(token);
                allocator_empty = true;
            }
            Err(e) => {
                allocator_empty = false;
                vlog!("DeliverLocked failed 0x{:08X}", e.code().0 as u32);
                if let Some(q) = &st.event_queue {
                    // SAFETY: valid queue.
                    unsafe {
                        let _ = q.0.QueueEventParamVar(
                            MEError.0 as u32,
                            &GUID::zeroed(),
                            e.code(),
                            std::ptr::null(),
                        );
                    }
                }
            }
            Ok(()) => allocator_empty = false,
        }
    }
    if allocator_empty {
        // SAFETY: plain Sleep.
        unsafe { Sleep(2) };
    }
    true
}
