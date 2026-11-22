//! Private, minimal reader of the shared-memory frame channel: a faithful copy of the logic of
//! `src/vcam/FrameReader.cpp` (local channel only: the DirectShow filter never opens the Global one)
//! and the layout of `src/common/SharedFrame.h`. The `niwcam-shm` crate is meant to replace it.
//!
//! Differences from the C++: the `local == false` paths (SDDL, Global names) are left out, as the filter
//! never takes them; the object names can carry a test suffix under the `test-names` feature.

use core::ffi::c_void;
use core::ptr;
use core::sync::atomic::{
    fence, AtomicBool, AtomicI32, AtomicI64, AtomicIsize, AtomicPtr, Ordering::SeqCst,
};
use niwcam_core::nv12::nv12_fit_into;
use windows::core::{HRESULT, PCWSTR};
use windows::Win32::Foundation::{
    CloseHandle, GetLastError, ERROR_ALREADY_EXISTS, HANDLE, INVALID_HANDLE_VALUE, S_OK,
};
use windows::Win32::System::Diagnostics::Debug::OutputDebugStringW;
use windows::Win32::System::Memory::{
    CreateFileMappingW, MapViewOfFile, OpenFileMappingW, UnmapViewOfFile, FILE_MAP_ALL_ACCESS,
    MEMORY_MAPPED_VIEW_ADDRESS, PAGE_READWRITE,
};
use windows::Win32::System::Threading::{
    CreateEventW, OpenEventW, Sleep, EVENT_MODIFY_STATE, SYNCHRONIZATION_SYNCHRONIZE,
};

// ---- SharedFrame.h ----------------------------------------------------------------------------

/// `CAMSTUDIO_LOCAL_MAPPING_NAME`.
pub const LOCAL_MAPPING_NAME: &str = r"Local\CamStudioVCam_Frames_v2";
/// `CAMSTUDIO_LOCAL_EVENT_NAME`.
pub const LOCAL_EVENT_NAME: &str = r"Local\CamStudioVCam_NewFrame_v2";
/// `CAMSTUDIO_LOCAL_MAX_PIXELS`: 4K either way up fits.
pub const LOCAL_MAX_PIXELS: u64 = 4096 * 2304;
/// `CAMSTUDIO_MAGIC`, "CMS1".
pub const MAGIC: u32 = 0x3153_4D43;
/// `CAMSTUDIO_VERSION`.
pub const VERSION: u32 = 2;
/// `CAMSTUDIO_STALE_100NS`: frames older than this are "no signal" (2 s).
pub const STALE_100NS: i64 = 20_000_000;
/// `CAMSTUDIO_SLOTS`.
pub const SLOTS: u32 = 3;

/// `SharedFrameHeader` (`#pragma pack(8)`; every field is naturally aligned already). The slots follow it.
#[repr(C)]
pub struct SharedFrameHeader {
    pub magic: u32,
    pub version: u32,
    pub width: u32,
    pub height: u32,
    pub stride: u32,
    pub slots: u32,
    pub frame_bytes: u64,
    /// (frame number << 2) | slot; 0 = nothing written yet.
    pub latest: i64,
    pub write_time_100ns: i64,
    pub reader_active: i32,
    pub reserved1: u32,
    pub slot_seq: [i64; SLOTS as usize],
    pub slot_readers: [i32; SLOTS as usize],
    pub reserved2: u32,
    pub pad: [u8; 32],
}

/// `sizeof(SharedFrameHeader)`.
pub const HEADER_BYTES: usize = 128;
const _: () = assert!(core::mem::size_of::<SharedFrameHeader>() == HEADER_BYTES);

/// `CamStudioNv12Bytes`.
pub fn nv12_bytes(w: u32, h: u32) -> u64 {
    w as u64 * h as u64 * 3 / 2
}

/// `CamStudioLocalSlotBytes`.
pub fn local_slot_bytes() -> u64 {
    LOCAL_MAX_PIXELS * 3 / 2
}

/// `CamStudioLocalMappingBytes`.
pub fn local_mapping_bytes() -> u64 {
    HEADER_BYTES as u64 + SLOTS as u64 * local_slot_bytes()
}

/// `CamStudioClampLocalSize`: a picture larger than the slots are sized for is written smaller, same shape.
pub fn clamp_local_size(w: &mut u32, h: &mut u32) {
    let pixels = *w as u64 * *h as u64;
    if pixels <= LOCAL_MAX_PIXELS || pixels == 0 {
        return;
    }
    let k = (LOCAL_MAX_PIXELS as f64 / pixels as f64).sqrt();
    *w = (*w as f64 * k) as u32 & !1;
    *h = (*h as f64 * k) as u32 & !1;
}

/// A kernel object name as a NUL-terminated wide string, with the test suffix when that feature is on.
fn object_name(base: &str) -> Vec<u16> {
    #[allow(unused_mut, reason = "only the test-names build appends")]
    let mut name = String::from(base);
    #[cfg(feature = "test-names")]
    if let Ok(suffix) = std::env::var("NIWCAM_TEST_SUFFIX") {
        name.push_str(&suffix);
    }
    name.encode_utf16().chain(Some(0)).collect()
}

fn vcam_log(text: &str) {
    let line: Vec<u16> = format!("[CamStudioVCam] {text}\n")
        .encode_utf16()
        .chain(Some(0))
        .collect();
    // SAFETY: `line` is NUL-terminated and outlives the call.
    unsafe { OutputDebugStringW(PCWSTR(line.as_ptr())) };
}

// ---- FrameReader ------------------------------------------------------------------------------

/// Reader side of the shared-memory frame channel (`FrameReader` of src/vcam). All methods take `&self`:
/// the capture thread reads while the COM threads may still open it, as in the C++ (which has no lock).
pub struct FrameReader {
    mapping: AtomicIsize,
    event: AtomicIsize,
    header: AtomicPtr<SharedFrameHeader>,
    active: AtomicBool,
    delivered: AtomicI64,
}

impl Default for FrameReader {
    fn default() -> Self {
        Self::new()
    }
}

impl FrameReader {
    pub const fn new() -> Self {
        Self {
            mapping: AtomicIsize::new(0),
            event: AtomicIsize::new(0),
            header: AtomicPtr::new(ptr::null_mut()),
            active: AtomicBool::new(false),
            delivered: AtomicI64::new(0),
        }
    }

    fn hdr(&self) -> *mut SharedFrameHeader {
        self.header.load(SeqCst)
    }

    fn field<T: Copy>(&self, f: impl FnOnce(*mut SharedFrameHeader) -> *const T) -> Option<T> {
        let h = self.hdr();
        if h.is_null() {
            return None;
        }
        // SAFETY: h is a live mapping of at least HEADER_BYTES (checked at open); the writer may change the
        // field at any time, which is why it is read volatile.
        Some(unsafe { ptr::read_volatile(f(h)) })
    }

    /// `Open(w, h, local = true)`: creates (or opens) the local mapping and event, sized for any picture.
    pub fn open(&self, native_width: u32, native_height: u32) -> HRESULT {
        if !self.hdr().is_null() {
            return S_OK;
        }
        let bytes = local_mapping_bytes();
        let mapping_name = object_name(LOCAL_MAPPING_NAME);
        let event_name = object_name(LOCAL_EVENT_NAME);

        // SAFETY: names are NUL-terminated and outlive the calls; default security (the user's own session).
        let (mapping, map_err, event) = unsafe {
            let mapping = CreateFileMappingW(
                INVALID_HANDLE_VALUE,
                None,
                PAGE_READWRITE,
                (bytes >> 32) as u32,
                bytes as u32,
                PCWSTR(mapping_name.as_ptr()),
            );
            let map_err = GetLastError();
            let event = CreateEventW(None, false, false, PCWSTR(event_name.as_ptr()));
            (mapping, map_err, event)
        };
        let (mapping, event) = match (mapping, event) {
            (Ok(m), Ok(e)) => (m, e),
            (m, e) => {
                // SAFETY: GetLastError has no preconditions; the handles that exist are ours.
                let hr = HRESULT::from_win32(unsafe { GetLastError() }.0);
                vcam_log(&format!(
                    "CreateFileMapping/CreateEvent failed 0x{:08X}",
                    hr.0 as u32
                ));
                // SAFETY: closing handles this function just created.
                unsafe {
                    if let Ok(m) = m {
                        let _ = CloseHandle(m);
                    }
                    if let Ok(e) = e {
                        let _ = CloseHandle(e);
                    }
                }
                return hr;
            }
        };
        self.mapping.store(mapping.0 as isize, SeqCst);
        self.event.store(event.0 as isize, SeqCst);

        // SAFETY: mapping is a valid section handle.
        let view = unsafe { MapViewOfFile(mapping, FILE_MAP_ALL_ACCESS, 0, 0, 0) };
        if view.Value.is_null() {
            // SAFETY: GetLastError has no preconditions.
            let hr = HRESULT::from_win32(unsafe { GetLastError() }.0);
            self.close();
            return hr;
        }
        let header = view.Value as *mut SharedFrameHeader;
        self.header.store(header, SeqCst);

        let magic = |h: *mut SharedFrameHeader| -> u32 {
            // SAFETY: header is mapped for the whole of HEADER_BYTES; the other opener writes it concurrently.
            unsafe { ptr::read_volatile(&raw const (*h).magic) }
        };
        if map_err == ERROR_ALREADY_EXISTS && magic(header) != MAGIC {
            // Someone else created it a moment ago and is still filling the header.
            for _ in 0..100 {
                if magic(header) == MAGIC {
                    break;
                }
                // SAFETY: Sleep has no preconditions.
                unsafe { Sleep(1) };
            }
        }
        if map_err != ERROR_ALREADY_EXISTS || magic(header) != MAGIC {
            // First opener initialises the header. The host waits for magic.
            let (mut w, mut h) = (native_width, native_height);
            clamp_local_size(&mut w, &mut h);
            // SAFETY: header is a fresh read/write mapping of at least HEADER_BYTES.
            unsafe { init_header(header, w, h, local_slot_bytes()) };
        }

        // SAFETY: header is mapped.
        let (w, h) = unsafe {
            (
                ptr::read_volatile(&raw const (*header).width),
                ptr::read_volatile(&raw const (*header).height),
            )
        };
        vcam_log(&format!(
            "Shared memory ready {w}x{h} (existing={})",
            (map_err == ERROR_ALREADY_EXISTS) as i32
        ));
        S_OK
    }

    /// `OpenExisting(local = true)`: opens the mapping only if it exists (the host runs); nothing is created.
    pub fn open_existing(&self) -> bool {
        if !self.hdr().is_null() {
            return true;
        }
        let mapping_name = object_name(LOCAL_MAPPING_NAME);
        let event_name = object_name(LOCAL_EVENT_NAME);
        // SAFETY: names are NUL-terminated and outlive the calls.
        unsafe {
            let Ok(mapping) =
                OpenFileMappingW(FILE_MAP_ALL_ACCESS.0, false, PCWSTR(mapping_name.as_ptr()))
            else {
                return false;
            };
            let view = MapViewOfFile(mapping, FILE_MAP_ALL_ACCESS, 0, 0, 0);
            let header = view.Value as *mut SharedFrameHeader;
            if header.is_null()
                || ptr::read_volatile(&raw const (*header).magic) != MAGIC
                || ptr::read_volatile(&raw const (*header).version) != VERSION
                || ptr::read_volatile(&raw const (*header).slots) != SLOTS
            {
                if !header.is_null() {
                    let _ = UnmapViewOfFile(view);
                }
                let _ = CloseHandle(mapping);
                return false;
            }
            self.mapping.store(mapping.0 as isize, SeqCst);
            self.header.store(header, SeqCst);
            let event = OpenEventW(
                SYNCHRONIZATION_SYNCHRONIZE | EVENT_MODIFY_STATE,
                false,
                PCWSTR(event_name.as_ptr()),
            );
            self.event.store(event.map_or(0, |e| e.0 as isize), SeqCst);
        }
        true
    }

    /// The picture the host writes now (local channel: it changes between streams).
    pub fn width(&self) -> u32 {
        // SAFETY: the pointer is only used by `field` on a live header.
        self.field(|h| unsafe { &raw const (*h).width })
            .unwrap_or(0)
    }

    pub fn height(&self) -> u32 {
        // SAFETY: as in `width`.
        self.field(|h| unsafe { &raw const (*h).height })
            .unwrap_or(0)
    }

    pub fn close(&self) {
        self.set_active(false);
        let h = self.header.swap(ptr::null_mut(), SeqCst);
        // SAFETY: each handle / view was created by this reader and is released once (swapped out first).
        unsafe {
            if !h.is_null() {
                let _ = UnmapViewOfFile(MEMORY_MAPPED_VIEW_ADDRESS {
                    Value: h as *mut c_void,
                });
            }
            let m = self.mapping.swap(0, SeqCst);
            if m != 0 {
                let _ = CloseHandle(HANDLE(m as *mut c_void));
            }
            let e = self.event.swap(0, SeqCst);
            if e != 0 {
                let _ = CloseHandle(HANDLE(e as *mut c_void));
            }
        }
    }

    /// `FrameEvent()`: the "new frame" event (a null handle when it could not be opened).
    pub fn frame_event(&self) -> HANDLE {
        HANDLE(self.event.load(SeqCst) as *mut c_void)
    }

    /// Number of the newest frame the host published (0 = none yet).
    pub fn latest_frame(&self) -> i64 {
        let h = self.hdr();
        if h.is_null() {
            return 0;
        }
        // SAFETY: live header; `latest` is 8-aligned (offset 32 of a page-aligned mapping).
        unsafe { AtomicI64::from_ptr(&raw mut (*h).latest) }.load(SeqCst) >> 2
    }

    /// Number of the last frame `fill` delivered: equal to `latest_frame` means nothing new to send.
    pub fn delivered_frame(&self) -> i64 {
        self.delivered.load(SeqCst)
    }

    /// `SetActive`: counts this reader in the header's `readerActive` (the host opens its source for it).
    pub fn set_active(&self, active: bool) {
        let h = self.hdr();
        if h.is_null() || self.active.swap(active, SeqCst) == active {
            return;
        }
        // SAFETY: live header; `reader_active` is 4-aligned (offset 48).
        unsafe { AtomicI32::from_ptr(&raw mut (*h).reader_active) }
            .fetch_add(if active { 1 } else { -1 }, SeqCst);
    }

    /// `Fill`: copies the latest host frame into an NV12 destination of any size (`pitch` bytes per row,
    /// `dst` holds `pitch * height * 3 / 2` bytes), scaled to fit (black bars when the shape differs).
    /// Draws a "no signal" pattern if the host is not writing. Returns true when a live frame was delivered.
    pub fn fill(
        &self,
        dst: &mut [u8],
        pitch: usize,
        width: u32,
        height: u32,
        now100ns: i64,
    ) -> bool {
        let h = self.hdr();
        // SAFETY (whole block): h is a live mapping; the writer is another process that follows the seqlock
        // protocol of SharedFrame.h. A torn read can only produce wrong pixel values (plain bytes), which the
        // seqlock check below detects and retries, exactly as the C++ does.
        unsafe {
            if !h.is_null() && ptr::read_volatile(&raw const (*h).slots) == SLOTS {
                let sw = ptr::read_volatile(&raw const (*h).width);
                let sh = ptr::read_volatile(&raw const (*h).height);
                let frame_bytes = ptr::read_volatile(&raw const (*h).frame_bytes);
                if sw == 0 || sh == 0 || nv12_bytes(sw, sh) > frame_bytes {
                    draw_no_signal(dst, pitch, width, height, now100ns);
                    return false;
                }
                let latest_a = AtomicI64::from_ptr(&raw mut (*h).latest);
                let write_time = AtomicI64::from_ptr(&raw mut (*h).write_time_100ns);
                let src_len = nv12_bytes(sw, sh) as usize;
                let base = h as *const u8;
                let slot_ptr = move |slot: u32| -> *const u8 {
                    // SAFETY: the slot lies inside the mapping (frame_bytes was checked against the picture).
                    base.add(HEADER_BYTES + slot as usize * frame_bytes as usize)
                };

                // "No signal" only when the host really stopped (or never wrote):
                // racing the writer just means trying the newer frame.
                for _attempt in 0..4 {
                    let latest = latest_a.load(SeqCst);
                    if latest == 0 {
                        break; // host never wrote
                    }
                    if now100ns - write_time.load(SeqCst) > STALE_100NS {
                        break; // host stopped
                    }
                    let slot = (latest & 3) as u32;
                    if slot >= SLOTS {
                        break;
                    }
                    let readers = AtomicI32::from_ptr(&raw mut (*h).slot_readers[slot as usize]);
                    let seq = AtomicI64::from_ptr(&raw mut (*h).slot_seq[slot as usize]);
                    readers.fetch_add(1, SeqCst);
                    let seq1 = seq.load(SeqCst);
                    let mut ok = false;
                    if seq1 & 1 == 0 {
                        let src = core::slice::from_raw_parts(slot_ptr(slot), src_len);
                        nv12_fit_into(src, sw as usize, sw, sh, dst, pitch, width, height);
                        fence(SeqCst);
                        ok = seq.load(SeqCst) == seq1;
                    }
                    readers.fetch_sub(1, SeqCst);
                    if ok {
                        self.delivered.store(latest >> 2, SeqCst);
                        return true;
                    }
                }
                // Still racing after several tries (several readers at a very high
                // rate): a possibly torn copy of the latest frame beats a grey flash.
                let latest = latest_a.load(SeqCst);
                if latest != 0 && now100ns - write_time.load(SeqCst) <= STALE_100NS {
                    let slot = (latest & 3) as u32 % SLOTS;
                    let src = core::slice::from_raw_parts(slot_ptr(slot), src_len);
                    nv12_fit_into(src, sw as usize, sw, sh, dst, pitch, width, height);
                    self.delivered.store(latest >> 2, SeqCst);
                    return true;
                }
            }
        }
        draw_no_signal(dst, pitch, width, height, now100ns);
        false
    }
}

impl Drop for FrameReader {
    fn drop(&mut self) {
        self.close();
    }
}

/// `CamStudioInitHeader`: everything but the magic, then the magic (readers and the host wait for it).
///
/// # Safety
/// `h` must point to a writable mapping of at least `HEADER_BYTES`.
unsafe fn init_header(h: *mut SharedFrameHeader, width: u32, height: u32, frame_bytes: u64) {
    // SAFETY: precondition; plain volatile stores into shared memory.
    unsafe {
        ptr::write_volatile(&raw mut (*h).version, VERSION);
        ptr::write_volatile(&raw mut (*h).width, width);
        ptr::write_volatile(&raw mut (*h).height, height);
        ptr::write_volatile(&raw mut (*h).stride, width);
        ptr::write_volatile(&raw mut (*h).slots, SLOTS);
        ptr::write_volatile(&raw mut (*h).frame_bytes, frame_bytes);
        ptr::write_volatile(&raw mut (*h).latest, 0);
        ptr::write_volatile(&raw mut (*h).write_time_100ns, 0);
        ptr::write_volatile(&raw mut (*h).reader_active, 0);
        for s in 0..SLOTS as usize {
            ptr::write_volatile(&raw mut (*h).slot_seq[s], 0);
            ptr::write_volatile(&raw mut (*h).slot_readers[s], 0);
        }
        fence(SeqCst);
        ptr::write_volatile(&raw mut (*h).magic, MAGIC);
    }
}

/// `DrawNoSignal`: dark grey with a sweeping bar so a stuck pipeline is easy to spot.
pub fn draw_no_signal(dst: &mut [u8], pitch: usize, width: u32, height: u32, now100ns: i64) {
    let width_u = width as usize;
    let bar_w = (width / 16) & !1;
    let bar_x = ((now100ns / 100_000) % (width as i64 + bar_w as i64)) as u32 & !1;
    for y in 0..height as usize {
        let row = &mut dst[y * pitch..y * pitch + width_u];
        row.fill(0x30);
        if bar_x < width {
            let w = if bar_x + bar_w > width {
                width - bar_x
            } else {
                bar_w
            };
            row[bar_x as usize..(bar_x + w) as usize].fill(0xB0);
        }
    }
    let uv = &mut dst[height as usize * pitch..];
    for y in 0..(height / 2) as usize {
        uv[y * pitch..y * pitch + width_u].fill(0x80);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn header_layout_matches_the_c_struct() {
        // Offsets of SharedFrameHeader under #pragma pack(8), as computed by hand from SharedFrame.h.
        assert_eq!(core::mem::offset_of!(SharedFrameHeader, frame_bytes), 24);
        assert_eq!(core::mem::offset_of!(SharedFrameHeader, latest), 32);
        assert_eq!(
            core::mem::offset_of!(SharedFrameHeader, write_time_100ns),
            40
        );
        assert_eq!(core::mem::offset_of!(SharedFrameHeader, reader_active), 48);
        assert_eq!(core::mem::offset_of!(SharedFrameHeader, slot_seq), 56);
        assert_eq!(core::mem::offset_of!(SharedFrameHeader, slot_readers), 80);
        assert_eq!(core::mem::offset_of!(SharedFrameHeader, pad), 96);
    }

    #[test]
    fn local_sizes() {
        assert_eq!(local_slot_bytes(), 4096 * 2304 * 3 / 2);
        let (mut w, mut h) = (8192, 4608);
        clamp_local_size(&mut w, &mut h);
        assert!(w as u64 * h as u64 <= LOCAL_MAX_PIXELS && w % 2 == 0 && h % 2 == 0);
        let (mut w, mut h) = (1920, 1080);
        clamp_local_size(&mut w, &mut h);
        assert_eq!((w, h), (1920, 1080));
    }

    #[test]
    fn no_signal_pattern() {
        let (w, h) = (64u32, 32u32);
        let mut f = vec![0u8; (w * h * 3 / 2) as usize];
        draw_no_signal(&mut f, w as usize, w, h, 0);
        assert!(f[..(w * h) as usize]
            .iter()
            .all(|&b| b == 0x30 || b == 0xB0));
        assert!(f[(w * h) as usize..].iter().all(|&b| b == 0x80));
        // bar of width/16 = 4 at x = 0
        assert_eq!(&f[..8], &[0xB0, 0xB0, 0xB0, 0xB0, 0x30, 0x30, 0x30, 0x30]);
    }
}
