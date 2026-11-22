//! Port of `src/vcam/MediaSource.h` / `MediaSource.cpp`: the media source Frame Server loads.

use std::ffi::c_void;
use std::sync::{Arc, Mutex};

use niwcam_shm::{mf_system_time, set_log_hook, FrameReader};
use windows::core::{implement, ComObject, Error, Interface, Ref, GUID, HRESULT, PCWSTR};
use windows::Win32::Foundation::{
    ERROR_SET_NOT_FOUND, E_INVALIDARG, E_NOTIMPL, E_POINTER, E_UNEXPECTED,
};
use windows::Win32::Media::KernelStreaming::{
    IKsControl, IKsControl_Impl, KSCAMERAPROFILE_Legacy, KSIDENTIFIER,
};
use windows::Win32::Media::MediaFoundation::*;
use windows::Win32::System::Com::StructuredStorage::PROPVARIANT;

use crate::common::{
    channel_names, check, guard, guard_drop, lock, vcam_log, vlog, ATTR_NATIVE_FPS,
    ATTR_NATIVE_SIZE,
};
use crate::media_stream::{MediaStream, VideoFormat};

const STREAM_ID: u32 = 0;
const DEFAULT_WIDTH: u32 = 1920;
const DEFAULT_HEIGHT: u32 = 1080;

#[derive(Clone, Copy, PartialEq, Eq)]
enum SourceStateKind {
    Invalid,
    Stopped,
    Started,
    Shutdown,
}

struct SourceState {
    state: SourceStateKind,
    event_queue: Option<IMFMediaEventQueue>,
    pd: Option<IMFPresentationDescriptor>,
    attributes: Option<IMFAttributes>,
    stream: Option<ComObject<MediaStream>>,
}

// SAFETY: the MF objects held here are free-threaded (the C++ shares them between Frame Server's
// threads under `m_lock`); all access is under `MediaSource::lock`.
unsafe impl Send for SourceState {}

#[implement(
    IMFMediaSourceEx,
    IMFGetService,
    IKsControl,
    IMFSampleAllocatorControl,
    Agile = false
)]
pub struct MediaSource {
    lock: Mutex<SourceState>,
    reader: Arc<Mutex<FrameReader>>,
}

/// `MediaSource::BuildFormats`: the formats we publish: native first, then downscales of the
/// same aspect.
pub fn build_formats(w: u32, h: u32, fps_num: u32, fps_den: u32) -> Vec<VideoFormat> {
    // Every size is offered at the native rate, plus 30 fps when the native
    // rate is higher: many conferencing apps reject or mishandle >30 fps.
    let add30 = fps_den != 0 && fps_num / fps_den > 31;
    let mut formats = Vec::new();
    let mut add = |fw: u32, fh: u32| {
        formats.push(VideoFormat {
            width: fw,
            height: fh,
            fps_num,
            fps_den,
        });
        if add30 {
            formats.push(VideoFormat {
                width: fw,
                height: fh,
                fps_num: 30,
                fps_den: 1,
            });
        }
    };

    add(w, h); // native first

    // Smaller sizes with the native aspect ratio, for apps that only accept
    // common resolutions. They are scaled from the native frame.
    const HEIGHTS: [u32; 7] = [2160, 1440, 1080, 720, 540, 480, 360];
    for th in HEIGHTS {
        if th >= h {
            continue;
        }
        let mut tw = ((th as u64 * w as u64 + h as u64 / 2) / h as u64) as u32;
        tw = (tw + 1) & !1u32;
        if tw < 160 {
            continue;
        }
        add(tw, th);
    }
    formats
}

impl MediaSource {
    /// `MakeAndInitialize<MediaSource>(&m_source, activateAttributes)`.
    pub fn create(
        activate_attributes: &IMFAttributes,
    ) -> windows::core::Result<ComObject<MediaSource>> {
        set_log_hook(vcam_log);
        let obj = ComObject::new(MediaSource {
            lock: Mutex::new(SourceState {
                state: SourceStateKind::Invalid,
                event_queue: None,
                pd: None,
                attributes: None,
                stream: None,
            }),
            reader: Arc::new(Mutex::new(FrameReader::with_names(channel_names()))),
        });
        let me: IMFMediaSource = obj.to_interface::<IMFMediaSourceEx>().cast()?;
        obj.initialize(me, Some(activate_attributes))?;
        Ok(obj)
    }

    /// `RuntimeClassInitialize`. `me` is this object's own `IMFMediaSource` (the stream's parent).
    fn initialize(
        &self,
        me: IMFMediaSource,
        activate_attributes: Option<&IMFAttributes>,
    ) -> windows::core::Result<()> {
        let mut st = lock(&self.lock);
        if st.state != SourceStateKind::Invalid {
            return Err(Error::from_hresult(MF_E_ALREADY_INITIALIZED));
        }

        let (mut w, mut h, mut fn_, mut fd) = (DEFAULT_WIDTH, DEFAULT_HEIGHT, 30u32, 1u32);
        if let Some(attrs) = activate_attributes {
            // SAFETY: valid attribute store.
            if let Ok(v) = unsafe { attrs.GetUINT64(&ATTR_NATIVE_SIZE) } {
                let (a, b) = ((v >> 32) as u32, v as u32);
                if a != 0 && b != 0 {
                    w = a & !1u32;
                    h = b & !1u32;
                }
            }
            // SAFETY: valid attribute store.
            if let Ok(v) = unsafe { attrs.GetUINT64(&ATTR_NATIVE_FPS) } {
                let (a, b) = ((v >> 32) as u32, v as u32);
                if a != 0 && b != 0 {
                    fn_ = a;
                    fd = b;
                }
            }
        }
        vlog!("Initialize native {}x{} @ {}/{}", w, h, fn_, fd);

        #[cfg(feature = "diag-file")]
        {
            crate::common::diag_token();
            crate::common::diag_objects();
        }
        check!(lock(&self.reader).open(w, h, false));
        st.attributes = Some(check!(create_source_attributes(activate_attributes)));
        // SAFETY: plain MF call.
        st.event_queue = Some(check!(unsafe { MFCreateEventQueue() }));

        let stream = check!(MediaStream::create(
            me,
            STREAM_ID,
            &build_formats(w, h, fn_, fd),
            self.reader.clone(),
        ));
        let stream_if: IMFMediaStream2 = stream.to_interface();
        // SAFETY: valid stream.
        let sd = check!(unsafe { stream_if.GetStreamDescriptor() });
        st.stream = Some(stream);
        // SAFETY: valid descriptor.
        st.pd = Some(check!(unsafe {
            MFCreatePresentationDescriptor(Some(&[Some(sd)]))
        }));

        st.state = SourceStateKind::Stopped;
        Ok(())
    }

    /// `IsShutdown`.
    pub fn is_shutdown(&self) -> bool {
        lock(&self.lock).state == SourceStateKind::Shutdown
    }

    /// `Shutdown` (also the destructor).
    pub fn do_shutdown(&self) {
        let stream;
        {
            let mut st = lock(&self.lock);
            if st.state == SourceStateKind::Shutdown {
                return;
            }
            st.state = SourceStateKind::Shutdown;
            if let Some(q) = &st.event_queue {
                // SAFETY: valid queue.
                unsafe {
                    let _ = q.Shutdown();
                }
            }
            stream = st.stream.take();
        }
        // Outside the source lock: this joins the stream's worker thread.
        if let Some(s) = stream {
            s.get().shutdown();
        }

        let mut st = lock(&self.lock);
        st.event_queue = None;
        st.pd = None;
        st.attributes = None;
        lock(&self.reader).close();
    }
}

impl Drop for MediaSource {
    fn drop(&mut self) {
        guard_drop(|| self.do_shutdown());
    }
}

/// `CreateSourceAttributes`.
fn create_source_attributes(
    activate_attributes: Option<&IMFAttributes>,
) -> windows::core::Result<IMFAttributes> {
    // SAFETY: plain MF calls on objects we own.
    unsafe {
        let mut attrs: Option<IMFAttributes> = None;
        check!(MFCreateAttributes(&mut attrs, 4));
        let attrs = attrs.ok_or_else(|| Error::from_hresult(E_UNEXPECTED))?;
        if let Some(a) = activate_attributes {
            check!(a.CopyAllItems(&attrs));
        }

        // A legacy profile that accepts every media type, so profile-aware apps
        // (e.g. the Windows Camera app) still see all of our formats. Optional:
        // if any step fails we log it and run without profiles.
        let profile_result = (|| -> windows::core::Result<()> {
            let profiles = MFCreateSensorProfileCollection()?;
            let profile = MFCreateSensorProfile(&KSCAMERAPROFILE_Legacy, 0, PCWSTR::null())?;
            profile.AddProfileFilter(STREAM_ID, windows::core::w!("((RES==;FRT==;SUT==))"))?;
            profiles.AddProfile(&profile)?;
            attrs.SetUnknown(&MF_DEVICEMFT_SENSORPROFILE_COLLECTION, &profiles)?;
            Ok(())
        })();
        if let Err(e) = profile_result {
            vlog!("Sensor profile skipped (0x{:08X})", e.code().0 as u32);
        }
        Ok(attrs)
    }
}

impl MediaSource {
    /// `CheckShutdownLocked`.
    fn check_shutdown(st: &SourceState) -> windows::core::Result<()> {
        if st.state == SourceStateKind::Shutdown {
            return Err(Error::from_hresult(MF_E_SHUTDOWN));
        }
        if st.event_queue.is_none() || st.stream.is_none() {
            return Err(Error::from_hresult(E_UNEXPECTED));
        }
        Ok(())
    }
}

fn queue(st: &SourceState) -> windows::core::Result<&IMFMediaEventQueue> {
    st.event_queue
        .as_ref()
        .ok_or_else(|| Error::from_hresult(E_UNEXPECTED))
}

fn stream_of(st: &SourceState) -> windows::core::Result<&ComObject<MediaStream>> {
    st.stream
        .as_ref()
        .ok_or_else(|| Error::from_hresult(E_UNEXPECTED))
}

// ---- IMFMediaEventGenerator ------------------------------------------------

impl IMFMediaEventGenerator_Impl for MediaSource_Impl {
    fn GetEvent(
        &self,
        flags: MEDIA_EVENT_GENERATOR_GET_EVENT_FLAGS,
    ) -> windows::core::Result<IMFMediaEvent> {
        guard(|| {
            let q = {
                let st = lock(&self.lock);
                check!(MediaSource::check_shutdown(&st));
                queue(&st)?.clone()
            };
            // SAFETY: valid queue (GetEvent may block, so the lock is not held).
            unsafe { q.GetEvent(flags.0) }
        })
    }

    fn BeginGetEvent(
        &self,
        callback: Ref<IMFAsyncCallback>,
        state: Ref<windows::core::IUnknown>,
    ) -> windows::core::Result<()> {
        guard(|| {
            let st = lock(&self.lock);
            check!(MediaSource::check_shutdown(&st));
            // SAFETY: valid queue; the callback and state are the caller's.
            unsafe { queue(&st)?.BeginGetEvent(callback.as_ref(), state.as_ref()) }
        })
    }

    fn EndGetEvent(&self, result: Ref<IMFAsyncResult>) -> windows::core::Result<IMFMediaEvent> {
        guard(|| {
            let st = lock(&self.lock);
            check!(MediaSource::check_shutdown(&st));
            // SAFETY: valid queue; the result is the caller's.
            unsafe { queue(&st)?.EndGetEvent(result.as_ref()) }
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
            let st = lock(&self.lock);
            check!(MediaSource::check_shutdown(&st));
            if ext.is_null() {
                return Err(Error::from_hresult(E_POINTER));
            }
            // SAFETY: valid queue; `ext` non-null; `pv` is the caller's (may be null).
            unsafe { queue(&st)?.QueueEventParamVar(met, &*ext, status, pv) }
        })
    }
}

// ---- IMFMediaSource --------------------------------------------------------

impl IMFMediaSource_Impl for MediaSource_Impl {
    fn GetCharacteristics(&self) -> windows::core::Result<u32> {
        guard(|| {
            let st = lock(&self.lock);
            check!(MediaSource::check_shutdown(&st));
            Ok(MFMEDIASOURCE_IS_LIVE.0 as u32)
        })
    }

    fn CreatePresentationDescriptor(&self) -> windows::core::Result<IMFPresentationDescriptor> {
        guard(|| {
            let st = lock(&self.lock);
            check!(MediaSource::check_shutdown(&st));
            let pd = st
                .pd
                .as_ref()
                .ok_or_else(|| Error::from_hresult(E_UNEXPECTED))?;
            // SAFETY: valid descriptor.
            unsafe { pd.Clone() }
        })
    }

    fn Start(
        &self,
        pd: Ref<IMFPresentationDescriptor>,
        time_format: *const GUID,
        start_pos: *const PROPVARIANT,
    ) -> windows::core::Result<()> {
        guard(|| {
            if pd.is_null() || start_pos.is_null() {
                return Err(Error::from_hresult(E_INVALIDARG));
            }
            // SAFETY: non-null, per the contract the caller's GUID.
            if !time_format.is_null() && unsafe { *time_format } != GUID::zeroed() {
                return Err(Error::from_hresult(MF_E_UNSUPPORTED_TIME_FORMAT));
            }
            let pd = pd.ok()?;

            let mut st = lock(&self.lock);
            check!(MediaSource::check_shutdown(&st));
            if st.state == SourceStateKind::Invalid {
                return Err(Error::from_hresult(MF_E_INVALID_STATE_TRANSITION));
            }

            // SAFETY: valid descriptors and queue throughout this block.
            unsafe {
                let count = check!(pd.GetStreamDescriptorCount());
                if count != 1 {
                    return Err(Error::from_hresult(E_INVALIDARG));
                }

                let mut selected = windows::core::BOOL(0);
                let mut sd: Option<IMFStreamDescriptor> = None;
                check!(pd.GetStreamDescriptorByIndex(0, &mut selected, &mut sd));
                let sd = sd.ok_or_else(|| Error::from_hresult(E_UNEXPECTED))?;

                let mut was_selected = windows::core::BOOL(0);
                let mut ours: Option<IMFStreamDescriptor> = None;
                let my_pd = st
                    .pd
                    .clone()
                    .ok_or_else(|| Error::from_hresult(E_UNEXPECTED))?;
                check!(my_pd.GetStreamDescriptorByIndex(0, &mut was_selected, &mut ours));

                let start_time = PROPVARIANT::from(mf_system_time());
                let q = queue(&st)?.clone();
                let stream = stream_of(&st)?.clone();

                if selected.as_bool() {
                    check!(my_pd.SelectStream(0));

                    let handler = check!(sd.GetMediaTypeHandler());
                    let ty = check!(handler.GetCurrentMediaType());

                    let unk: windows::core::IUnknown = stream.to_interface();
                    check!(q.QueueEventParamUnk(
                        if was_selected.as_bool() {
                            MEUpdatedStream.0
                        } else {
                            MENewStream.0
                        } as u32,
                        &GUID::zeroed(),
                        HRESULT(0),
                        &unk,
                    ));
                    check!(stream.get().start(&ty));
                } else if was_selected.as_bool() {
                    check!(my_pd.DeselectStream(0));
                    check!(stream.get().stop(false));
                }

                check!(q.QueueEventParamVar(
                    MESourceStarted.0 as u32,
                    &GUID::zeroed(),
                    HRESULT(0),
                    &start_time,
                ));
            }
            st.state = SourceStateKind::Started;
            Ok(())
        })
    }

    fn Stop(&self) -> windows::core::Result<()> {
        guard(|| {
            let mut st = lock(&self.lock);
            check!(MediaSource::check_shutdown(&st));
            if st.state != SourceStateKind::Started {
                return Err(Error::from_hresult(MF_E_INVALID_STATE_TRANSITION));
            }

            check!(stream_of(&st)?.get().stop(true));
            let pd = st
                .pd
                .clone()
                .ok_or_else(|| Error::from_hresult(E_UNEXPECTED))?;
            // SAFETY: valid descriptor and queue.
            unsafe {
                check!(pd.DeselectStream(0));

                let stop_time = PROPVARIANT::from(mf_system_time());
                check!(queue(&st)?.QueueEventParamVar(
                    MESourceStopped.0 as u32,
                    &GUID::zeroed(),
                    HRESULT(0),
                    &stop_time,
                ));
            }
            st.state = SourceStateKind::Stopped;
            Ok(())
        })
    }

    fn Pause(&self) -> windows::core::Result<()> {
        guard(|| Err(Error::from_hresult(MF_E_INVALID_STATE_TRANSITION)))
    }

    fn Shutdown(&self) -> windows::core::Result<()> {
        guard(|| {
            self.do_shutdown();
            Ok(())
        })
    }
}

// ---- IMFMediaSourceEx ------------------------------------------------------

impl IMFMediaSourceEx_Impl for MediaSource_Impl {
    fn GetSourceAttributes(&self) -> windows::core::Result<IMFAttributes> {
        guard(|| {
            let st = lock(&self.lock);
            check!(MediaSource::check_shutdown(&st));
            // Must hand out a reference to our store, not a copy.
            st.attributes
                .clone()
                .ok_or_else(|| Error::from_hresult(E_UNEXPECTED))
        })
    }

    fn GetStreamAttributes(&self, stream_id: u32) -> windows::core::Result<IMFAttributes> {
        guard(|| {
            let st = lock(&self.lock);
            check!(MediaSource::check_shutdown(&st));
            let stream = stream_of(&st)?;
            if stream_id != stream.get().id() {
                return Err(Error::from_hresult(MF_E_NOT_FOUND));
            }
            stream
                .get()
                .attributes()
                .ok_or_else(|| Error::from_hresult(E_UNEXPECTED))
        })
    }

    fn SetD3DManager(&self, _manager: Ref<windows::core::IUnknown>) -> windows::core::Result<()> {
        // System-memory samples only for now.
        guard(|| Err(Error::from_hresult(E_NOTIMPL)))
    }
}

// ---- IMFGetService ---------------------------------------------------------

impl IMFGetService_Impl for MediaSource_Impl {
    fn GetService(
        &self,
        _service: *const GUID,
        _riid: *const GUID,
        object: *mut *mut c_void,
    ) -> windows::core::Result<()> {
        guard(|| {
            if object.is_null() {
                return Err(Error::from_hresult(E_POINTER));
            }
            // SAFETY: non-null out pointer from the caller.
            unsafe { *object = std::ptr::null_mut() };
            Err(Error::from_hresult(MF_E_UNSUPPORTED_SERVICE))
        })
    }
}

// ---- IKsControl ------------------------------------------------------------
// No camera controls yet. ERROR_SET_NOT_FOUND mimics an AVStream driver that
// has no handler for the requested property set.

fn clear_bytes_returned(bytes_returned: *mut u32) {
    if !bytes_returned.is_null() {
        // SAFETY: non-null out pointer from the caller.
        unsafe { *bytes_returned = 0 };
    }
}

fn set_not_found() -> Error {
    Error::from_hresult(HRESULT::from_win32(ERROR_SET_NOT_FOUND.0))
}

impl IKsControl_Impl for MediaSource_Impl {
    fn KsProperty(
        &self,
        property: *const KSIDENTIFIER,
        property_length: u32,
        _data: *mut c_void,
        _data_length: u32,
        bytes_returned: *mut u32,
    ) -> windows::core::Result<()> {
        guard(|| {
            if property.is_null()
                || (property_length as usize) < std::mem::size_of::<KSIDENTIFIER>()
            {
                return Err(Error::from_hresult(E_INVALIDARG));
            }
            clear_bytes_returned(bytes_returned);
            Err(set_not_found())
        })
    }

    fn KsMethod(
        &self,
        _method: *const KSIDENTIFIER,
        _method_length: u32,
        _data: *mut c_void,
        _data_length: u32,
        bytes_returned: *mut u32,
    ) -> windows::core::Result<()> {
        guard(|| {
            clear_bytes_returned(bytes_returned);
            Err(set_not_found())
        })
    }

    fn KsEvent(
        &self,
        _event: *const KSIDENTIFIER,
        _event_length: u32,
        _data: *mut c_void,
        _data_length: u32,
        bytes_returned: *mut u32,
    ) -> windows::core::Result<()> {
        guard(|| {
            clear_bytes_returned(bytes_returned);
            Err(set_not_found())
        })
    }
}

// ---- IMFSampleAllocatorControl ---------------------------------------------

impl IMFSampleAllocatorControl_Impl for MediaSource_Impl {
    fn SetDefaultAllocator(
        &self,
        output_stream_id: u32,
        allocator: Ref<windows::core::IUnknown>,
    ) -> windows::core::Result<()> {
        guard(|| {
            let allocator = allocator.ok().map_err(|_| Error::from_hresult(E_POINTER))?;
            let stream = {
                let st = lock(&self.lock);
                check!(MediaSource::check_shutdown(&st));
                let s = stream_of(&st)?;
                if output_stream_id != s.get().id() {
                    return Err(Error::from_hresult(MF_E_NOT_FOUND));
                }
                s.clone()
            };
            let video_allocator: IMFVideoSampleAllocator = check!(allocator.cast());
            stream.get().set_sample_allocator(&video_allocator)
        })
    }

    fn GetAllocatorUsage(
        &self,
        output_stream_id: u32,
        input_stream_id: *mut u32,
        usage: *mut MFSampleAllocatorUsage,
    ) -> windows::core::Result<()> {
        guard(|| {
            if input_stream_id.is_null() || usage.is_null() {
                return Err(Error::from_hresult(E_POINTER));
            }
            let st = lock(&self.lock);
            check!(MediaSource::check_shutdown(&st));
            if output_stream_id != stream_of(&st)?.get().id() {
                return Err(Error::from_hresult(MF_E_NOT_FOUND));
            }
            // SAFETY: non-null out pointers from the caller.
            unsafe {
                *input_stream_id = output_stream_id;
                *usage = MFSampleAllocatorUsage_UsesProvidedAllocator;
            }
            Ok(())
        })
    }
}
