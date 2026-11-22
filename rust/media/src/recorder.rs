//! "Ghi hình": what the virtual camera shows, to an MP4 (H.264, the GPU's encoder when there is
//! one), with the default microphone (AAC) when asked. Nothing exists until `start`; frames come
//! from the capture thread (`push`), the microphone is read on its own thread.
//!
//! Port of `src/engine/Recorder.{h,cpp}`. All methods take `&self` (the C++ object is shared
//! between the capture thread, the microphone thread and the API thread).
use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread::JoinHandle;
use std::time::Duration;

use niwcam_core::nv12::{nv12_fit_into, nv12_scale};
use windows::core::{HRESULT, PCWSTR};
use windows::Win32::Media::Audio::{eCapture, eConsole, IMMDeviceEnumerator, MMDeviceEnumerator};
use windows::Win32::Media::MediaFoundation::*;
use windows::Win32::System::Com::{
    CoCreateInstance, CoInitializeEx, CoTaskMemFree, CoUninitialize, CLSCTX_ALL,
    COINIT_MULTITHREADED,
};
use windows::Win32::System::Performance::{QueryPerformanceCounter, QueryPerformanceFrequency};

use crate::mf_util::{self, E_ACCESSDENIED, E_INVALIDARG, S_FALSE, S_OK};

const MF_E_INVALIDMEDIATYPE_: HRESULT = MF_E_INVALIDMEDIATYPE;

/// The audio half: the default microphone as a source reader, PCM in.
#[derive(Clone)]
struct Mic {
    source: IMFMediaSource,
    reader: IMFSourceReader,
    stream: u32, // the sink writer's audio stream
    rate: u32,   // samples a second
    block: u32,  // bytes per sample frame
}

/// `m_lock`: the sink writer, from the capture and microphone threads.
#[derive(Default)]
struct Core {
    writer: Option<IMFSinkWriter>,
    video_stream: u32,
    width: u32,
    height: u32,
    frame_interval: i64, // 100 ns
    last_video: i64,
}

// SAFETY: Media Foundation sink writers, source readers and media sources are free-threaded
// (the C++ shares them between the capture, microphone and API threads under the same locks).
unsafe impl Send for Shared {}
unsafe impl Sync for Shared {}
unsafe impl Send for Mic {}
unsafe impl Sync for Mic {}

struct Shared {
    core: Mutex<Core>,
    active: AtomicBool,
    has_audio: AtomicBool,
    start_qpc: AtomicI64,
    freq: AtomicI64,
    mic: Mutex<Option<Mic>>,
    mic_stop: AtomicBool,
    mic_done: AtomicBool, // the microphone's thread has left its loop
    problem: Mutex<String>,
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|p| p.into_inner())
}

impl Shared {
    /// 100 ns since `start`.
    fn now_100ns(&self) -> i64 {
        let mut now = 0i64;
        // SAFETY: valid out pointer.
        let _ = unsafe { QueryPerformanceCounter(&mut now) };
        ((now - self.start_qpc.load(Ordering::Relaxed)) as f64 * 10_000_000.0
            / self.freq.load(Ordering::Relaxed) as f64) as i64
    }
}

pub struct Recorder {
    sh: Arc<Shared>,
    mic_thread: Mutex<Option<JoinHandle<()>>>,
}

impl Default for Recorder {
    fn default() -> Self {
        Self::new()
    }
}

impl Drop for Recorder {
    fn drop(&mut self) {
        let _ = self.stop();
    }
}

/// The default microphone (Windows' "communications"-independent default for recording).
fn default_microphone() -> Result<IMFMediaSource, HRESULT> {
    let e = |e: windows::core::Error| e.code();
    // SAFETY: plain COM / Media Foundation calls; the endpoint id is freed with CoTaskMemFree.
    unsafe {
        let devices =
            CoCreateInstance::<_, IMMDeviceEnumerator>(&MMDeviceEnumerator, None, CLSCTX_ALL)
                .map_err(e)?;
        let device = devices
            .GetDefaultAudioEndpoint(eCapture, eConsole)
            .map_err(e)?;
        let id = device.GetId().map_err(e)?;
        let mut attrs: Option<IMFAttributes> = None;
        let _ = MFCreateAttributes(&mut attrs, 2);
        let attrs = attrs.ok_or(mf_util::E_UNEXPECTED)?;
        let _ = attrs.SetGUID(
            &MF_DEVSOURCE_ATTRIBUTE_SOURCE_TYPE,
            &MF_DEVSOURCE_ATTRIBUTE_SOURCE_TYPE_AUDCAP_GUID,
        );
        let _ = attrs.SetString(
            &MF_DEVSOURCE_ATTRIBUTE_SOURCE_TYPE_AUDCAP_ENDPOINT_ID,
            PCWSTR(id.0),
        );
        CoTaskMemFree(Some(id.0 as *const _));
        MFCreateDeviceSource(&attrs).map_err(e)
    }
}

fn new_attributes(initial: u32) -> Option<IMFAttributes> {
    let mut a: Option<IMFAttributes> = None;
    // SAFETY: valid out slot.
    unsafe { MFCreateAttributes(&mut a, initial) }.ok()?;
    a
}

fn new_media_type() -> Option<IMFMediaType> {
    // SAFETY: plain call.
    unsafe { MFCreateMediaType() }.ok()
}

impl Recorder {
    pub fn new() -> Self {
        Self {
            sh: Arc::new(Shared {
                core: Mutex::new(Core {
                    frame_interval: 333_333,
                    last_video: -1,
                    ..Core::default()
                }),
                active: AtomicBool::new(false),
                has_audio: AtomicBool::new(false),
                start_qpc: AtomicI64::new(0),
                freq: AtomicI64::new(1),
                mic: Mutex::new(None),
                mic_stop: AtomicBool::new(false),
                mic_done: AtomicBool::new(true),
                problem: Mutex::new(String::new()),
            }),
            mic_thread: Mutex::new(None),
        }
    }

    /// width x height (even) at up to fps frames a second. microphone: also the default microphone;
    /// without one (none, or blocked in Windows' privacy settings) the video is recorded alone.
    pub fn start(
        &self,
        path: &str,
        width: u32,
        height: u32,
        fps: u32,
        microphone: bool,
    ) -> HRESULT {
        let sh = &self.sh;
        let _ = self.stop();
        let width = width & !1u32;
        let height = height & !1u32;
        if width < 16 || height < 16 {
            return E_INVALIDARG;
        }
        let fps = fps.clamp(5, 60);
        lock(&sh.problem).clear();

        let wide: Vec<u16> = path.encode_utf16().chain(std::iter::once(0)).collect();
        let attrs = new_attributes(3);
        // SAFETY: plain Media Foundation calls on interfaces we own; `wide` outlives the call.
        unsafe {
            if let Some(a) = &attrs {
                let _ = a.SetUINT32(&MF_READWRITE_ENABLE_HARDWARE_TRANSFORMS, 1); // the GPU's H.264 encoder
                let _ = a.SetUINT32(&MF_SINK_WRITER_DISABLE_THROTTLING, 1); // a live source: never block it
            }
            let writer =
                match MFCreateSinkWriterFromURL(PCWSTR(wide.as_ptr()), None, attrs.as_ref()) {
                    Ok(w) => w,
                    Err(e) => return e.code(),
                };

            // H.264 at about 0.1 bit per pixel and frame: 1080p30 ~ 6 Mbit/s, 4K30 ~ 25 Mbit/s.
            let bitrate = ((width as u64 * height as u64 * fps as u64 / 10) as u32)
                .clamp(2_000_000, 40_000_000);
            let (Some(out), Some(inp)) = (new_media_type(), new_media_type()) else {
                return mf_util::E_UNEXPECTED;
            };
            let _ = out.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video);
            let _ = out.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_H264);
            let _ = out.SetUINT32(&MF_MT_AVG_BITRATE, bitrate);
            let _ = out.SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32);
            mf_util::set_attribute_size(&out, &MF_MT_FRAME_SIZE, width, height);
            mf_util::set_attribute_ratio(&out, &MF_MT_FRAME_RATE, fps, 1);
            mf_util::set_attribute_ratio(&out, &MF_MT_PIXEL_ASPECT_RATIO, 1, 1);
            let video = match writer.AddStream(&out) {
                Ok(v) => v,
                Err(e) => return e.code(),
            };
            let _ = inp.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video);
            let _ = inp.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_NV12);
            let _ = inp.SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32);
            let _ = inp.SetUINT32(&MF_MT_DEFAULT_STRIDE, width);
            mf_util::set_attribute_size(&inp, &MF_MT_FRAME_SIZE, width, height);
            mf_util::set_attribute_ratio(&inp, &MF_MT_FRAME_RATE, fps, 1);
            mf_util::set_attribute_ratio(&inp, &MF_MT_PIXEL_ASPECT_RATIO, 1, 1);
            if let Err(e) = writer.SetInputMediaType(video, &inp, None) {
                return e.code();
            }

            sh.has_audio.store(false, Ordering::SeqCst);
            if microphone {
                if let Err(audio) = self.add_audio(&writer) {
                    *lock(&sh.problem) = if audio == E_ACCESSDENIED {
                        "Không ghi được tiếng: Windows đang chặn micro (Cài đặt › Quyền riêng tư › Micro)."
                            .to_string()
                    } else {
                        "Không ghi được tiếng: không mở được micro.".to_string()
                    };
                }
            }
            if let Err(e) = writer.BeginWriting() {
                *lock(&sh.mic) = None;
                return e.code();
            }

            let mut core = lock(&sh.core);
            core.writer = Some(writer);
            core.video_stream = video;
            core.width = width;
            core.height = height;
            core.frame_interval = 10_000_000 / fps as i64;
            core.last_video = -1;
            let mut f = 0i64;
            let mut s = 0i64;
            let _ = QueryPerformanceFrequency(&mut f);
            let _ = QueryPerformanceCounter(&mut s);
            sh.freq.store(f, Ordering::Relaxed);
            sh.start_qpc.store(s, Ordering::Relaxed);
            sh.active.store(true, Ordering::SeqCst);
            let mic = lock(&sh.mic).clone();
            if let Some(mic) = mic {
                sh.mic_stop.store(false, Ordering::SeqCst);
                sh.mic_done.store(false, Ordering::SeqCst);
                let sh2 = sh.clone();
                match std::thread::Builder::new()
                    .name("recorder-mic".into())
                    .spawn(move || read_microphone(&sh2, &mic))
                {
                    Ok(t) => *lock(&self.mic_thread) = Some(t),
                    Err(_) => sh.mic_done.store(true, Ordering::SeqCst),
                }
            }
        }
        S_OK
    }

    /// One frame of the camera (NV12, any size: fitted into the recording's, bars around). Frames
    /// closer together than the frame rate allows are skipped. Any thread. `nv12` holds `h` luma
    /// rows of `pitch` bytes, then `h / 2` UV rows.
    pub fn push(&self, nv12: &[u8], pitch: i32, w: u32, h: u32) {
        let sh = &self.sh;
        if !sh.active.load(Ordering::SeqCst) {
            return;
        }
        let mut core = lock(&sh.core);
        let Some(writer) = core.writer.clone() else {
            return;
        };
        let now = sh.now_100ns();
        // A source faster than the recording (a 100 fps webcam): every other frame or so.
        if core.last_video >= 0 && now - core.last_video < core.frame_interval * 8 / 10 {
            return;
        }

        let (mw, mh) = (core.width, core.height);
        let size = (mw as u64 * mh as u64 * 3 / 2) as u32; // CamStudioNv12Bytes
                                                           // SAFETY: the buffer is locked while the frame is written into its `size` bytes.
        unsafe {
            let Ok(buffer) = MFCreateMemoryBuffer(size) else {
                return;
            };
            let mut dst: *mut u8 = std::ptr::null_mut();
            if buffer.Lock(&mut dst, None, None).is_err() {
                return;
            }
            let out = std::slice::from_raw_parts_mut(dst, size as usize);
            if w == mw && h == mh {
                nv12_scale(nv12, pitch as usize, w, h, out, mw as usize, mw, mh);
            } else {
                nv12_fit_into(nv12, pitch as usize, w, h, out, mw as usize, mw, mh);
                // another source's shape
            }
            let _ = buffer.Unlock();
            let _ = buffer.SetCurrentLength(size);
            let Ok(sample) = MFCreateSample() else {
                return;
            };
            let _ = sample.AddBuffer(&buffer);
            let _ = sample.SetSampleTime(now);
            let _ = sample.SetSampleDuration(core.frame_interval);
            if writer.WriteSample(core.video_stream, &sample).is_ok() {
                core.last_video = now;
            }
        }
    }

    /// Finishes the file (it is playable only after this).
    pub fn stop(&self) -> HRESULT {
        let sh = &self.sh;
        if !sh.active.load(Ordering::SeqCst) && lock(&sh.core).writer.is_none() {
            return S_FALSE;
        }
        sh.active.store(false, Ordering::SeqCst);
        sh.mic_stop.store(true, Ordering::SeqCst);
        let thread = lock(&self.mic_thread).take();
        if let Some(thread) = thread {
            // A sample comes every ~10 ms, so the loop sees the stop at once. A microphone that went
            // silent (unplugged) is shut down; one that still hangs is left behind, the file finishes.
            let wait_done = |ms: u32| {
                let mut i = 0;
                while i < ms / 10 && !sh.mic_done.load(Ordering::SeqCst) {
                    std::thread::sleep(Duration::from_millis(10));
                    i += 1;
                }
                sh.mic_done.load(Ordering::SeqCst)
            };
            if !wait_done(1000) {
                if let Some(m) = lock(&sh.mic).as_ref() {
                    // SAFETY: plain COM call.
                    let _ = unsafe { m.source.Shutdown() };
                }
            }
            if wait_done(1000) {
                let _ = thread.join();
            } else {
                drop(thread); // detach
            }
        }
        if sh.mic_done.load(Ordering::SeqCst) {
            if let Some(m) = lock(&sh.mic).take() {
                // SAFETY: plain COM call.
                let _ = unsafe { m.source.Shutdown() };
            }
        }
        let writer = lock(&sh.core).writer.take();
        match writer {
            // SAFETY: plain COM call.
            Some(w) => mf_util::hr_of(unsafe { w.Finalize() }),
            None => S_FALSE,
        }
    }

    pub fn active(&self) -> bool {
        self.sh.active.load(Ordering::SeqCst)
    }

    pub fn elapsed_ms(&self) -> i64 {
        if self.active() {
            self.sh.now_100ns() / 10000
        } else {
            0
        }
    }

    pub fn has_audio(&self) -> bool {
        self.sh.has_audio.load(Ordering::SeqCst)
    }

    pub fn audio_problem(&self) -> String {
        lock(&self.sh.problem).clone()
    }

    /// PCM from the microphone in, AAC out. Only rates and channel counts the AAC encoder takes.
    fn add_audio(&self, writer: &IMFSinkWriter) -> Result<(), HRESULT> {
        let e = |e: windows::core::Error| e.code();
        let first_audio = MF_SOURCE_READER_FIRST_AUDIO_STREAM.0 as u32;
        let source = default_microphone()?;
        // SAFETY: plain Media Foundation calls on interfaces we own.
        unsafe {
            let reader = MFCreateSourceReaderFromMediaSource(&source, None).map_err(e)?;
            let pcm = new_media_type().ok_or(mf_util::E_UNEXPECTED)?;
            let _ = pcm.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Audio);
            let _ = pcm.SetGUID(&MF_MT_SUBTYPE, &MFAudioFormat_PCM);
            let _ = pcm.SetUINT32(&MF_MT_AUDIO_BITS_PER_SAMPLE, 16);
            let _ = pcm.SetUINT32(&MF_MT_AUDIO_SAMPLES_PER_SECOND, 48000);
            let _ = pcm.SetUINT32(&MF_MT_AUDIO_NUM_CHANNELS, 2);
            reader
                .SetCurrentMediaType(first_audio, None, &pcm)
                .map_err(e)?;
            let actual = reader.GetCurrentMediaType(first_audio).map_err(e)?;
            let rate = mf_util::get_attribute_uint32(&actual, &MF_MT_AUDIO_SAMPLES_PER_SECOND, 0);
            let channels = mf_util::get_attribute_uint32(&actual, &MF_MT_AUDIO_NUM_CHANNELS, 0);
            if (rate != 44100 && rate != 48000) || (channels != 1 && channels != 2) {
                return Err(MF_E_INVALIDMEDIATYPE_);
            }

            let aac = new_media_type().ok_or(mf_util::E_UNEXPECTED)?;
            let _ = aac.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Audio);
            let _ = aac.SetGUID(&MF_MT_SUBTYPE, &MFAudioFormat_AAC);
            let _ = aac.SetUINT32(&MF_MT_AUDIO_BITS_PER_SAMPLE, 16);
            let _ = aac.SetUINT32(&MF_MT_AUDIO_SAMPLES_PER_SECOND, rate);
            let _ = aac.SetUINT32(&MF_MT_AUDIO_NUM_CHANNELS, channels);
            let _ = aac.SetUINT32(&MF_MT_AUDIO_AVG_BYTES_PER_SECOND, 20000); // 160 kbit/s
            let _ = aac.SetUINT32(&MF_MT_AAC_PAYLOAD_TYPE, 0);
            let stream = writer.AddStream(&aac).map_err(e)?;
            writer.SetInputMediaType(stream, &actual, None).map_err(e)?;

            let block =
                mf_util::get_attribute_uint32(&actual, &MF_MT_AUDIO_BLOCK_ALIGNMENT, channels * 2);
            *lock(&self.sh.mic) = Some(Mic {
                source,
                reader,
                stream,
                rate,
                block,
            });
            self.sh.has_audio.store(true, Ordering::SeqCst);
        }
        Ok(())
    }
}

/// The microphone's thread: its samples are timed by how many there were (no drift, no gaps),
/// from when the first one arrived.
fn read_microphone(sh: &Shared, mic: &Mic) {
    let first_audio = MF_SOURCE_READER_FIRST_AUDIO_STREAM.0 as u32;
    // SAFETY: COM init paired with CoUninitialize; COM calls on the reader we were given.
    unsafe {
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
        let mut offset: i64 = -1;
        let mut frames: u64 = 0;
        while !sh.mic_stop.load(Ordering::SeqCst) {
            let mut flags = 0u32;
            let mut ts = 0i64;
            let mut sample: Option<IMFSample> = None;
            let r = mic.reader.ReadSample(
                first_audio,
                0,
                None,
                Some(&mut flags),
                Some(&mut ts),
                Some(&mut sample),
            );
            if r.is_err()
                || (flags & (MF_SOURCE_READERF_ERROR.0 | MF_SOURCE_READERF_ENDOFSTREAM.0) as u32)
                    != 0
            {
                *lock(&sh.problem) = "Micro ngừng giữa chừng: phần sau không có tiếng.".to_string();
                break;
            }
            let Some(sample) = sample else { continue };
            let bytes = sample.GetTotalLength().unwrap_or(0);
            let count = bytes.checked_div(mic.block).unwrap_or(0) as u64;
            if count == 0 {
                continue;
            }
            if offset < 0 {
                offset = sh.now_100ns();
            }
            let time = offset + (frames * 10_000_000 / mic.rate as u64) as i64;
            frames += count;
            let _ = sample.SetSampleTime(time);
            let _ = sample.SetSampleDuration((count * 10_000_000 / mic.rate as u64) as i64);
            let core = lock(&sh.core);
            if let Some(w) = &core.writer {
                let _ = w.WriteSample(mic.stream, &sample);
            }
        }
        sh.mic_done.store(true, Ordering::SeqCst);
        CoUninitialize();
    }
}
