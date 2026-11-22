//! Small Win32 helpers shared by the writer and the reader: kernel-object names (overridable for
//! tests), a mapped view and an event with RAII, the clock both sides agree on, a log hook.

use std::ffi::c_void;
use std::sync::atomic::Ordering;
use std::sync::OnceLock;

use windows::core::{Error, HRESULT, PCWSTR};
use windows::Win32::Foundation::{CloseHandle, GetLastError, HANDLE, WIN32_ERROR};
use windows::Win32::Media::MediaFoundation::MFGetSystemTime;
use windows::Win32::System::Memory::{
    MapViewOfFile, UnmapViewOfFile, VirtualQuery, FILE_MAP, MEMORY_BASIC_INFORMATION,
    MEMORY_MAPPED_VIEW_ADDRESS,
};

use crate::shared_frame::*;

/// `MFGetSystemTime()` in 100 ns units: the clock the writer stamps frames with and the reader
/// compares against (both sides must use the same one).
pub fn mf_system_time() -> i64 {
    // SAFETY: no arguments, no preconditions.
    unsafe { MFGetSystemTime() }
}

type LogFn = fn(&str);
static LOG_HOOK: OnceLock<LogFn> = OnceLock::new();

/// Where the reader sends what the C++ sent to `VcamLog` (`OutputDebugString` in the DLL). Set
/// once; without a hook nothing is logged.
pub fn set_log_hook(f: fn(&str)) {
    let _ = LOG_HOOK.set(f);
}

pub(crate) fn log(msg: &str) {
    if let Some(f) = LOG_HOOK.get() {
        f(msg);
    }
}

/// `HRESULT_FROM_WIN32(code)` as an error.
pub(crate) fn win32_error(code: WIN32_ERROR) -> Error {
    Error::from_hresult(HRESULT::from_win32(code.0))
}

/// The error of the last Win32 call on this thread (`HRESULT_FROM_WIN32(GetLastError())`).
pub(crate) fn last_error() -> Error {
    // SAFETY: plain thread-local read.
    win32_error(unsafe { GetLastError() })
}

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// Names of the two kernel objects of one channel (mapping + "new frame" event).
#[derive(Clone, Debug)]
pub struct ChannelNames {
    mapping: Vec<u16>,
    event: Vec<u16>,
}

impl ChannelNames {
    pub fn new(mapping: &str, event: &str) -> Self {
        Self {
            mapping: wide(mapping),
            event: wide(event),
        }
    }

    pub(crate) fn mapping(&self) -> PCWSTR {
        PCWSTR(self.mapping.as_ptr())
    }

    pub(crate) fn event(&self) -> PCWSTR {
        PCWSTR(self.event.as_ptr())
    }

    pub fn mapping_string(&self) -> String {
        String::from_utf16_lossy(&self.mapping[..self.mapping.len() - 1])
    }

    pub fn event_string(&self) -> String {
        String::from_utf16_lossy(&self.event[..self.event.len() - 1])
    }
}

/// The names of both channels: the global one (Windows 11, Frame Server) and the local one
/// (Windows 10, the DirectShow filter). `Names::default()` is the real names.
#[derive(Clone, Debug)]
pub struct Names {
    pub global: ChannelNames,
    pub local: ChannelNames,
}

impl Default for Names {
    fn default() -> Self {
        Self {
            global: ChannelNames::new(CAMSTUDIO_MAPPING_NAME, CAMSTUDIO_EVENT_NAME),
            local: ChannelNames::new(CAMSTUDIO_LOCAL_MAPPING_NAME, CAMSTUDIO_LOCAL_EVENT_NAME),
        }
    }
}

impl Names {
    /// Names for tests: session-local objects (a non-elevated test cannot create `Global\` file
    /// mappings) carrying `suffix`, one pair per channel.
    pub fn for_test(suffix: &str) -> Self {
        Self {
            global: ChannelNames::new(
                &format!("Local\\NiwCamTest_{suffix}_G_Frames"),
                &format!("Local\\NiwCamTest_{suffix}_G_NewFrame"),
            ),
            local: ChannelNames::new(
                &format!("Local\\NiwCamTest_{suffix}_L_Frames"),
                &format!("Local\\NiwCamTest_{suffix}_L_NewFrame"),
            ),
        }
    }

    pub fn channel(&self, local: bool) -> &ChannelNames {
        if local {
            &self.local
        } else {
            &self.global
        }
    }
}

/// A kernel handle closed on drop.
pub(crate) struct OwnedHandle(pub HANDLE);

impl Drop for OwnedHandle {
    fn drop(&mut self) {
        if !self.0.is_invalid() {
            // SAFETY: we own the handle and close it once.
            unsafe {
                let _ = CloseHandle(self.0);
            }
        }
    }
}

// SAFETY: a kernel handle is just a number usable from any thread.
unsafe impl Send for OwnedHandle {}
// SAFETY: as above; the handle is only passed to thread-safe kernel calls.
unsafe impl Sync for OwnedHandle {}

/// A mapped view of the shared header + slots, with the mapping handle.
pub(crate) struct View {
    header: *mut SharedFrameHeader,
    /// Bytes of the view (page rounded, from `VirtualQuery`).
    bytes: usize,
    _mapping: OwnedHandle,
}

// SAFETY: the view is shared memory made for cross-thread/process use; all header access goes
// through atomics and the slot bytes are guarded by the seqlock protocol.
unsafe impl Send for View {}
// SAFETY: see above; `&View` only gives atomic header access and raw slot pointers.
unsafe impl Sync for View {}

impl View {
    /// Maps the whole mapping. Takes the handle (closed on failure and on drop). The view must be
    /// at least a header long (else a stranger's mapping: not ours, an error).
    pub(crate) fn map(mapping: HANDLE, access: FILE_MAP) -> Result<View, Error> {
        let mapping = OwnedHandle(mapping);
        // SAFETY: valid mapping handle; offset 0, whole mapping.
        let view = unsafe { MapViewOfFile(mapping.0, access, 0, 0, 0) };
        if view.Value.is_null() {
            return Err(last_error());
        }
        let mut info = MEMORY_BASIC_INFORMATION::default();
        // SAFETY: `view` is the base of a mapped view; `info` is a valid out buffer.
        let got = unsafe {
            VirtualQuery(
                Some(view.Value as *const c_void),
                &mut info,
                std::mem::size_of::<MEMORY_BASIC_INFORMATION>(),
            )
        };
        let bytes = if got == 0 { 0 } else { info.RegionSize };
        if bytes < HEADER_SIZE {
            // SAFETY: unmapping the view we just mapped.
            unsafe {
                let _ = UnmapViewOfFile(view);
            }
            return Err(win32_error(WIN32_ERROR(0x57))); // ERROR_INVALID_PARAMETER
        }
        Ok(View {
            header: view.Value as *mut SharedFrameHeader,
            bytes,
            _mapping: mapping,
        })
    }

    pub(crate) fn header(&self) -> &SharedFrameHeader {
        // SAFETY: the view is at least HEADER_SIZE long (checked in `map`), aligned to a page,
        // and every field is an atomic, so shared references are fine.
        unsafe { &*self.header }
    }

    /// Whether the header's own `frame_bytes` fits three slots into the view: a stranger's or
    /// corrupted header must not make us read or write past the mapping (the C++ trusted it).
    pub(crate) fn frame_bytes_fit(&self) -> bool {
        let fb = self.header().frame_bytes.load(Ordering::Relaxed);
        fb.checked_mul(CAMSTUDIO_SLOTS as u64)
            .and_then(|b| b.checked_add(HEADER_SIZE as u64))
            .is_some_and(|b| b <= self.bytes as u64)
    }

    /// Pointer to slot `slot` and the number of bytes of the slot, or `None` when the header does
    /// not fit the view or the slot index is out of range.
    pub(crate) fn slot(&self, slot: u32) -> Option<(*mut u8, usize)> {
        if slot >= CAMSTUDIO_SLOTS || !self.frame_bytes_fit() {
            return None;
        }
        // SAFETY: `frame_bytes_fit` checked that three slots lie inside the view.
        let p = unsafe { SharedFrameHeader::slot_ptr(self.header, slot) };
        let fb = self.header().frame_bytes.load(Ordering::Relaxed) as usize;
        Some((p, fb))
    }
}

impl Drop for View {
    fn drop(&mut self) {
        // SAFETY: the view was mapped by `map` and is unmapped once; the mapping handle closes after.
        unsafe {
            let _ = UnmapViewOfFile(MEMORY_MAPPED_VIEW_ADDRESS {
                Value: self.header as *mut c_void,
            });
        }
    }
}

/// Minimum length of an NV12 buffer of w x h with row pitch `pitch` (the last chroma row needs
/// only w bytes).
pub(crate) fn nv12_min_len(pitch: usize, w: usize, h: usize) -> usize {
    if w == 0 || h == 0 {
        return 0;
    }
    pitch * (h + h / 2) - (pitch - w.min(pitch))
}
