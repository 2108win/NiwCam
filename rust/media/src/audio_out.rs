//! "Micro điện thoại": the phone's microphone (AUDIO records, docs/protocol.md 5.1) played into an
//! output device, typically VB-Audio's "CABLE Input", so a meeting app picks "CABLE Output" as its
//! microphone. WASAPI shared mode on its own thread, a ~60 ms jitter buffer trimmed when it grows.
//! "wav:<path>" as the target writes the sound to a WAV file instead (tests, no sound made).
//!
//! Port of `src/engine/AudioOut.{h,cpp}`.
use std::collections::VecDeque;
use std::fs::File;
use std::io::{Seek, SeekFrom, Write};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::thread::JoinHandle;
use std::time::Duration;

use windows::core::{GUID, HRESULT, PCWSTR};
use windows::Win32::Foundation::{CloseHandle, PROPERTYKEY};
use windows::Win32::Media::Audio::*;
use windows::Win32::System::Com::StructuredStorage::{PropVariantClear, PROPVARIANT};
use windows::Win32::System::Com::{
    CoCreateInstance, CoInitializeEx, CoTaskMemFree, CoUninitialize, CLSCTX_ALL,
    COINIT_MULTITHREADED, STGM_READ,
};
use windows::Win32::System::SystemInformation::GetTickCount64;
use windows::Win32::System::Threading::{CreateEventW, WaitForSingleObject};

const TARGET_MS: u32 = 60; // what the buffer aims to hold
const MAX_MS: u32 = 150; // more than this (clock drift, a burst): drop back to the target

/// PKEY_Device_FriendlyName (functiondiscoverykeys_devpkey.h).
const FRIENDLY_NAME: PROPERTYKEY = PROPERTYKEY {
    fmtid: GUID::from_values(
        0xa45c254e,
        0xdf1c,
        0x4efd,
        [0x80, 0x20, 0x67, 0xd1, 0x46, 0xa8, 0x50, 0xe0],
    ),
    pid: 14,
};

fn write_wav_header(
    f: &mut File,
    rate: u32,
    channels: u32,
    data_bytes: u32,
) -> std::io::Result<()> {
    let byte_rate = rate.wrapping_mul(channels).wrapping_mul(2);
    let riff = 36u32.wrapping_add(data_bytes);
    let format = 1u16;
    let ch = channels as u16;
    let block = channels.wrapping_mul(2) as u16;
    let bits = 16u16;
    let fmt_size = 16u32;
    f.seek(SeekFrom::Start(0))?;
    let mut h = Vec::with_capacity(44);
    h.extend_from_slice(b"RIFF");
    h.extend_from_slice(&riff.to_le_bytes());
    h.extend_from_slice(b"WAVEfmt ");
    h.extend_from_slice(&fmt_size.to_le_bytes());
    h.extend_from_slice(&format.to_le_bytes());
    h.extend_from_slice(&ch.to_le_bytes());
    h.extend_from_slice(&rate.to_le_bytes());
    h.extend_from_slice(&byte_rate.to_le_bytes());
    h.extend_from_slice(&block.to_le_bytes());
    h.extend_from_slice(&bits.to_le_bytes());
    h.extend_from_slice(b"data");
    h.extend_from_slice(&data_bytes.to_le_bytes());
    f.write_all(&h)
}

#[derive(Default)]
struct State {
    ring: VecDeque<i16>,
    rate: u32,
    channels: u32,
    quit: bool,
    problem: String,
    last_push: u64,
    // "wav:" target
    wav: Option<File>,
    wav_bytes: u32,
}

struct Inner {
    target: String,
    lock: Mutex<State>,
    wake: Condvar,
}

impl Inner {
    fn state(&self) -> MutexGuard<'_, State> {
        self.lock.lock().unwrap_or_else(|p| p.into_inner())
    }
    fn is_wav(&self) -> bool {
        self.target.starts_with("wav:")
    }
    fn set_problem(&self, p: &str) {
        self.state().problem = p.to_string();
    }
}

pub struct AudioOut {
    inner: Arc<Inner>,
    thread: Option<JoinHandle<()>>,
}

impl AudioOut {
    /// `target`: a render endpoint id (`Outputs()`), or `wav:<path>`.
    pub fn new(target: &str) -> Self {
        let inner = Arc::new(Inner {
            target: target.to_string(),
            lock: Mutex::new(State {
                channels: 1,
                ..State::default()
            }),
            wake: Condvar::new(),
        });
        let thread = if inner.is_wav() {
            None // written in push, no thread
        } else {
            let i = inner.clone();
            std::thread::Builder::new()
                .name("audio-out".into())
                .spawn(move || run(&i))
                .ok()
        };
        Self { inner, thread }
    }

    /// 16-bit PCM, interleaved. A new rate or channel count reopens the device.
    pub fn push(&self, pcm: &[u8], rate: u32, channels: u32) {
        if rate == 0 || channels == 0 || pcm.len() < 2 {
            return;
        }
        let samples = pcm.len() / 2;
        let mut st = self.inner.state();
        // SAFETY: GetTickCount64 has no preconditions.
        st.last_push = unsafe { GetTickCount64() };
        if self.inner.is_wav() {
            if st.wav.is_none() {
                let path = &self.inner.target[4..];
                st.wav = File::create(path).ok();
                if let Some(f) = st.wav.as_mut() {
                    let _ = write_wav_header(f, rate, channels, 0);
                }
            }
            st.rate = rate;
            st.channels = channels;
            if st.wav.is_some() {
                // fseek(f, 0, SEEK_END) before every write: the writes are append-only anyway.
                if let Some(f) = st.wav.as_mut() {
                    let _ = f.seek(SeekFrom::End(0));
                    let _ = f.write_all(&pcm[..samples * 2]);
                }
                st.wav_bytes = st.wav_bytes.wrapping_add((samples * 2) as u32);
            }
            return;
        }
        if rate != st.rate || channels != st.channels {
            st.rate = rate;
            st.channels = channels;
            st.ring.clear(); // the render thread reopens the device in this format
        }
        st.ring.extend(
            pcm[..samples * 2]
                .as_chunks::<2>()
                .0
                .iter()
                .map(|b| i16::from_le_bytes(*b)),
        );
        // Too far behind (the device runs slower than the phone, or a burst after a stall).
        let max = rate as usize * channels as usize * MAX_MS as usize / 1000;
        let target = rate as usize * channels as usize * TARGET_MS as usize / 1000;
        if st.ring.len() > max {
            let n = (st.ring.len() - target) / channels as usize * channels as usize;
            st.ring.drain(..n);
        }
        drop(st);
        self.inner.wake.notify_all();
    }

    /// Why nothing plays ("" when it does).
    pub fn problem(&self) -> String {
        self.inner.state().problem.clone()
    }

    /// Sound arrived in the last second.
    pub fn receiving(&self) -> bool {
        let st = self.inner.state();
        // SAFETY: GetTickCount64 has no preconditions.
        st.last_push != 0 && unsafe { GetTickCount64() }.wrapping_sub(st.last_push) < 1000
    }

    /// Active output devices: (endpoint id, name).
    pub fn outputs() -> Vec<(String, String)> {
        let mut out = Vec::new();
        // SAFETY: plain COM calls; the id string is freed with CoTaskMemFree, the PROPVARIANT cleared.
        unsafe {
            let Ok(devices) =
                CoCreateInstance::<_, IMMDeviceEnumerator>(&MMDeviceEnumerator, None, CLSCTX_ALL)
            else {
                return out;
            };
            let Ok(list) = devices.EnumAudioEndpoints(eRender, DEVICE_STATE_ACTIVE) else {
                return out;
            };
            let count = list.GetCount().unwrap_or(0);
            for i in 0..count {
                let Ok(device) = list.Item(i) else { continue };
                let Ok(id) = device.GetId() else { continue };
                let mut name = String::new();
                if let Ok(props) = device.OpenPropertyStore(STGM_READ) {
                    if let Ok(mut v) = props.GetValue(&FRIENDLY_NAME) {
                        name = propvariant_lpwstr(&v).unwrap_or_default();
                        let _ = PropVariantClear(&mut v);
                    }
                }
                out.push((id.to_string().unwrap_or_default(), name));
                CoTaskMemFree(Some(id.0 as *const _));
            }
        }
        out
    }
}

/// The string of a VT_LPWSTR PROPVARIANT, None for any other type.
fn propvariant_lpwstr(v: &PROPVARIANT) -> Option<String> {
    // SAFETY: the union is read as the variant type says (VT_LPWSTR holds a NUL-terminated string).
    unsafe {
        let inner = &v.Anonymous.Anonymous;
        if inner.vt.0 != 31 {
            return None; // VT_LPWSTR
        }
        let p = inner.Anonymous.pwszVal;
        if p.is_null() {
            return Some(String::new());
        }
        p.to_string().ok()
    }
}

impl Drop for AudioOut {
    fn drop(&mut self) {
        self.inner.state().quit = true;
        self.inner.wake.notify_all();
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
        let mut st = self.inner.state();
        let (rate, channels, bytes) = (st.rate, st.channels, st.wav_bytes);
        if let Some(f) = st.wav.as_mut() {
            let _ = write_wav_header(f, rate, channels, bytes);
        }
        st.wav = None;
    }
}

struct Open {
    client: IAudioClient,
    render: IAudioRenderClient,
    buffer_frames: u32,
}

fn run(inner: &Inner) {
    // SAFETY: COM thread init paired with CoUninitialize below; every COM object is dropped
    // before it; the event handle is closed at the end.
    unsafe {
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
        let event = CreateEventW(None, false, false, None).unwrap_or_default();
        let mut open: Option<Open> = None;
        let mut open_rate = 0u32;
        let mut open_channels = 0u32;
        let mut primed = false; // the buffer reached its target since (re)opening or an underrun

        loop {
            let (rate, channels);
            {
                let mut st = inner.state();
                if open.is_none() {
                    let (g, _) = inner
                        .wake
                        .wait_timeout_while(st, Duration::from_millis(200), |s| {
                            !(s.quit || s.rate != 0)
                        })
                        .unwrap_or_else(|p| p.into_inner());
                    st = g;
                }
                if st.quit {
                    break;
                }
                rate = st.rate;
                channels = st.channels;
            }
            if rate == 0 {
                continue;
            }

            if open.is_none() || rate != open_rate || channels != open_channels {
                if let Some(o) = &open {
                    let _ = o.client.Stop();
                }
                open = None;
                open_rate = 0;
                open_channels = 0;
                match open_device(&inner.target, rate, channels, event) {
                    Ok(o) => {
                        open = Some(o);
                    }
                    Err(hr) => {
                        // 0x80070490 = ERROR_NOT_FOUND = E_NOTFOUND
                        inner.set_problem(if hr == HRESULT(0x8007_0490u32 as i32) {
                            "Không thấy thiết bị phát đã chọn (đã gỡ VB-Audio Cable?)"
                        } else {
                            "Không mở được thiết bị phát tiếng"
                        });
                        let st = inner.state();
                        let _ = inner
                            .wake
                            .wait_timeout_while(st, Duration::from_secs(2), |s| !s.quit)
                            .unwrap_or_else(|p| p.into_inner());
                        continue;
                    }
                }
                inner.set_problem("");
                open_rate = rate;
                open_channels = channels;
                primed = false;
            }

            let Some(o) = &open else { continue };
            let _ = WaitForSingleObject(event, 100);
            let padding = match o.client.GetCurrentPadding() {
                Ok(p) => p,
                Err(_) => {
                    open = None; // the device went away: reopen
                    continue;
                }
            };
            let free = o.buffer_frames.saturating_sub(padding);
            if free == 0 {
                continue;
            }
            let Ok(data) = o.render.GetBuffer(free) else {
                continue;
            };
            let mut filled = 0u32;
            {
                let mut st = inner.state();
                let have = st.ring.len() / channels as usize;
                if !primed && have >= (rate.wrapping_mul(TARGET_MS) / 1000) as usize {
                    primed = true;
                }
                if primed {
                    filled = (free as usize).min(have) as u32;
                    let n = filled as usize * channels as usize;
                    let out = std::slice::from_raw_parts_mut(data, n * 2);
                    for (dst, s) in out
                        .as_chunks_mut::<2>()
                        .0
                        .iter_mut()
                        .zip(st.ring.drain(..n))
                    {
                        *dst = s.to_le_bytes();
                    }
                    if filled < free {
                        primed = false; // ran dry: wait for the buffer again
                    }
                }
            }
            if filled < free {
                let start = filled as usize * channels as usize * 2;
                let len = (free - filled) as usize * channels as usize * 2;
                std::ptr::write_bytes(data.add(start), 0, len);
            }
            let _ = o.render.ReleaseBuffer(free, 0);
        }
        if let Some(o) = &open {
            let _ = o.client.Stop();
        }
        drop(open);
        if !event.is_invalid() {
            let _ = CloseHandle(event);
        }
        CoUninitialize();
    }
}

/// Opens the endpoint in the phone's format. Err carries the HRESULT of the step that failed.
///
/// # Safety
/// COM must be initialised on the calling thread and `event` must be a valid event handle.
unsafe fn open_device(
    target: &str,
    rate: u32,
    channels: u32,
    event: windows::Win32::Foundation::HANDLE,
) -> Result<Open, HRESULT> {
    let e = |e: windows::core::Error| e.code();
    // SAFETY: plain COM calls; `format` outlives Initialize.
    unsafe {
        let devices =
            CoCreateInstance::<_, IMMDeviceEnumerator>(&MMDeviceEnumerator, None, CLSCTX_ALL)
                .map_err(e)?;
        let wide: Vec<u16> = target.encode_utf16().chain(std::iter::once(0)).collect();
        let device = devices.GetDevice(PCWSTR(wide.as_ptr())).map_err(e)?;
        let client: IAudioClient = device.Activate(CLSCTX_ALL, None).map_err(e)?;
        let block_align = channels.wrapping_mul(2) as u16;
        let format = WAVEFORMATEX {
            wFormatTag: WAVE_FORMAT_PCM as u16,
            nChannels: channels as u16,
            nSamplesPerSec: rate,
            wBitsPerSample: 16,
            nBlockAlign: block_align,
            nAvgBytesPerSec: rate.wrapping_mul(block_align as u32),
            cbSize: 0,
        };
        // Windows converts to the device's own format (rate, channels, float).
        client
            .Initialize(
                AUDCLNT_SHAREMODE_SHARED,
                AUDCLNT_STREAMFLAGS_EVENTCALLBACK
                    | AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM
                    | AUDCLNT_STREAMFLAGS_SRC_DEFAULT_QUALITY,
                50 * 10000,
                0,
                &format,
                None,
            )
            .map_err(e)?;
        client.SetEventHandle(event).map_err(e)?;
        let buffer_frames = client.GetBufferSize().map_err(e)?;
        let render: IAudioRenderClient = client.GetService().map_err(e)?;
        client.Start().map_err(e)?;
        Ok(Open {
            client,
            render,
            buffer_frames,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wav_target_writes_header_and_samples() {
        let path =
            std::env::temp_dir().join(format!("niwcam-audio-test-{}.wav", std::process::id()));
        let target = format!("wav:{}", path.display());
        {
            let a = AudioOut::new(&target);
            assert!(!a.receiving());
            a.push(&[1], 48000, 1); // under 2 bytes: ignored
            a.push(&[1, 0, 2, 0, 3], 48000, 2); // 2 whole samples, the odd byte dropped
            assert!(a.receiving());
            assert_eq!(a.problem(), "");
        }
        let bytes = std::fs::read(&path).unwrap();
        let _ = std::fs::remove_file(&path);
        assert_eq!(bytes.len(), 44 + 4);
        assert_eq!(&bytes[0..4], b"RIFF");
        assert_eq!(u32::from_le_bytes(bytes[4..8].try_into().unwrap()), 36 + 4);
        assert_eq!(u16::from_le_bytes(bytes[22..24].try_into().unwrap()), 2); // channels
        assert_eq!(u32::from_le_bytes(bytes[24..28].try_into().unwrap()), 48000);
        assert_eq!(
            u32::from_le_bytes(bytes[28..32].try_into().unwrap()),
            192000
        );
        assert_eq!(u32::from_le_bytes(bytes[40..44].try_into().unwrap()), 4);
        assert_eq!(&bytes[44..], &[1, 0, 2, 0]);
    }
}
