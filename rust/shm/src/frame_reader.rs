//! Reader side of the shared-memory frame channel (lives in Frame Server, or in the capturing
//! app on Windows 10). Port of `src/vcam/FrameReader.h` / `FrameReader.cpp`.

use std::sync::atomic::{fence, Ordering};

use niwcam_core::nv12::nv12_fit_into;
use windows::core::{Error, BOOL, PCWSTR};
use windows::Win32::Foundation::{
    GetLastError, LocalFree, ERROR_ALREADY_EXISTS, HANDLE, HLOCAL, INVALID_HANDLE_VALUE,
};
use windows::Win32::Security::Authorization::{
    ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1,
};
use windows::Win32::Security::{PSECURITY_DESCRIPTOR, SECURITY_ATTRIBUTES};
use windows::Win32::System::Memory::{
    CreateFileMappingW, OpenFileMappingW, FILE_MAP_ALL_ACCESS, PAGE_READWRITE,
};
use windows::Win32::System::Threading::{
    CreateEventW, OpenEventW, Sleep, EVENT_MODIFY_STATE, SYNCHRONIZATION_SYNCHRONIZE,
};

use crate::os::{last_error, log, Names, OwnedHandle, View};
use crate::shared_frame::*;

pub struct FrameReader {
    names: Names,
    view: Option<View>,
    event: Option<OwnedHandle>,
    active: bool,
    delivered: i64,
}

impl Default for FrameReader {
    fn default() -> Self {
        Self::new()
    }
}

impl Drop for FrameReader {
    fn drop(&mut self) {
        self.close();
    }
}

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

impl FrameReader {
    /// A reader on the real kernel-object names.
    pub fn new() -> Self {
        Self::with_names(Names::default())
    }

    /// A reader on other names (tests).
    pub fn with_names(names: Names) -> Self {
        Self {
            names,
            view: None,
            event: None,
            active: false,
            delivered: 0,
        }
    }

    /// Creates (or opens) the global mapping sized for the native format. `local`: the Windows 10
    /// channel instead (sized once for any picture; the size given is only what a fresh mapping
    /// starts with).
    pub fn open(
        &mut self,
        native_width: u32,
        native_height: u32,
        local: bool,
    ) -> Result<(), Error> {
        if self.view.is_some() {
            return Ok(());
        }

        // The global channel is created by Frame Server (LocalService) with a DACL the unelevated
        // host can open. The local one lives in the user's own session: default security.
        let mut sd = PSECURITY_DESCRIPTOR::default();
        if !local {
            let sddl = wide(CAMSTUDIO_OBJECT_SDDL);
            // SAFETY: `sddl` is NUL-terminated and outlives the call; `sd` is a valid out pointer.
            unsafe {
                ConvertStringSecurityDescriptorToSecurityDescriptorW(
                    PCWSTR(sddl.as_ptr()),
                    SDDL_REVISION_1,
                    &mut sd,
                    None,
                )
            }?;
        }
        let sa = SECURITY_ATTRIBUTES {
            nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
            lpSecurityDescriptor: sd.0,
            bInheritHandle: BOOL(0),
        };
        let psa: Option<*const SECURITY_ATTRIBUTES> = if local { None } else { Some(&sa) };

        let bytes = if local {
            local_mapping_bytes()
        } else {
            mapping_bytes(native_width, native_height)
        };
        let names = self.names.channel(local).clone();
        // SAFETY: names are NUL-terminated wide strings that outlive the calls; `psa` points at `sa`
        // (with a valid descriptor or null) for the duration of each call.
        let mapping = unsafe {
            CreateFileMappingW(
                INVALID_HANDLE_VALUE,
                psa,
                PAGE_READWRITE,
                (bytes >> 32) as u32,
                bytes as u32,
                names.mapping(),
            )
        };
        // SAFETY: plain thread-local read, directly after the call it describes.
        let map_err = unsafe { GetLastError() };
        // SAFETY: as above.
        let event = unsafe { CreateEventW(psa, false, false, names.event()) };
        if !sd.0.is_null() {
            // SAFETY: the descriptor was allocated by ConvertString... with LocalAlloc.
            unsafe {
                let _ = LocalFree(Some(HLOCAL(sd.0)));
            }
        }

        let (mapping, event) = match (mapping, event) {
            (Ok(m), Ok(e)) => (m, OwnedHandle(e)),
            (m, e) => {
                // The error of the failing call (the C++ read GetLastError once for both).
                let err = e
                    .as_ref()
                    .err()
                    .or(m.as_ref().err())
                    .cloned()
                    .unwrap_or_else(last_error);
                log(&format!(
                    "CreateFileMapping/CreateEvent failed 0x{:08X}",
                    err.code().0 as u32
                ));
                // Whatever did succeed closes here.
                drop(m.map(OwnedHandle));
                drop(e.map(OwnedHandle));
                return Err(err);
            }
        };

        // `View::map` takes the mapping handle (it closes it if mapping fails).
        let view = View::map(mapping, FILE_MAP_ALL_ACCESS)?;
        self.event = Some(event);

        let existed = map_err == ERROR_ALREADY_EXISTS;
        {
            let hdr = view.header();
            if existed && hdr.magic.load(Ordering::SeqCst) != CAMSTUDIO_MAGIC {
                // Someone else created it a moment ago and is still filling the header.
                for _ in 0..100 {
                    if hdr.magic.load(Ordering::SeqCst) == CAMSTUDIO_MAGIC {
                        break;
                    }
                    // SAFETY: plain Sleep.
                    unsafe { Sleep(1) };
                }
            }
            if !existed || hdr.magic.load(Ordering::SeqCst) != CAMSTUDIO_MAGIC {
                // First opener initialises the header. The host waits for magic.
                let (mut w, mut h) = (native_width, native_height);
                if local {
                    clamp_local_size(&mut w, &mut h);
                }
                hdr.init(
                    w,
                    h,
                    if local {
                        local_slot_bytes()
                    } else {
                        nv12_bytes(w, h)
                    },
                );
            }
            log(&format!(
                "Shared memory ready {}x{} (existing={})",
                hdr.width.load(Ordering::Relaxed),
                hdr.height.load(Ordering::Relaxed),
                existed as i32
            ));
        }
        self.view = Some(view);
        Ok(())
    }

    /// Opens the mapping only if it exists (the host runs): nothing is created.
    pub fn open_existing(&mut self, local: bool) -> bool {
        if self.view.is_some() {
            return true;
        }
        let names = self.names.channel(local).clone();
        // SAFETY: the name is a NUL-terminated wide string that outlives the call.
        let Ok(mapping) =
            (unsafe { OpenFileMappingW(FILE_MAP_ALL_ACCESS.0, false, names.mapping()) })
        else {
            return false;
        };
        let Ok(view) = View::map(mapping, FILE_MAP_ALL_ACCESS) else {
            return false;
        };
        {
            let h = view.header();
            if h.magic.load(Ordering::SeqCst) != CAMSTUDIO_MAGIC
                || h.version.load(Ordering::Relaxed) != CAMSTUDIO_VERSION
                || h.slots.load(Ordering::Relaxed) != CAMSTUDIO_SLOTS
            {
                return false; // `view` drops: unmapped and closed
            }
        }
        self.view = Some(view);
        // SAFETY: the name is a NUL-terminated wide string that outlives the call.
        self.event = unsafe {
            OpenEventW(
                SYNCHRONIZATION_SYNCHRONIZE | EVENT_MODIFY_STATE,
                false,
                names.event(),
            )
        }
        .ok()
        .map(OwnedHandle);
        true
    }

    /// The picture the host writes now (local channel: it changes between streams).
    pub fn width(&self) -> u32 {
        self.view
            .as_ref()
            .map_or(0, |v| v.header().width.load(Ordering::SeqCst))
    }

    pub fn height(&self) -> u32 {
        self.view
            .as_ref()
            .map_or(0, |v| v.header().height.load(Ordering::SeqCst))
    }

    pub fn is_open(&self) -> bool {
        self.view.is_some()
    }

    pub fn close(&mut self) {
        self.set_active(false);
        self.view = None;
        self.event = None;
    }

    /// The "new frame" event (null handle when there is none, like the C++ `HANDLE`).
    pub fn frame_event(&self) -> HANDLE {
        self.event.as_ref().map_or(HANDLE::default(), |e| e.0)
    }

    /// Number of the newest frame the host published (0 = none yet).
    pub fn latest_frame(&self) -> i64 {
        self.view
            .as_ref()
            .map_or(0, |v| v.header().latest.load(Ordering::SeqCst) >> 2)
    }

    /// Number of the last frame `fill` delivered: equal to `latest_frame` means nothing new.
    pub fn delivered_frame(&self) -> i64 {
        self.delivered
    }

    pub fn set_active(&mut self, active: bool) {
        let Some(view) = &self.view else { return };
        if active == self.active {
            return;
        }
        self.active = active;
        view.header()
            .reader_active
            .fetch_add(if active { 1 } else { -1 }, Ordering::SeqCst);
    }

    /// Copies the latest host frame into an NV12 destination of any size (`dst`: UV plane right
    /// after `height` rows of `pitch` bytes), scaled to fit (black bars when the shape differs).
    /// Draws a "no signal" pattern if the host is not writing. Returns true when a live frame
    /// was delivered. A `dst` shorter than `pitch * height * 3 / 2` is left alone (false).
    pub fn fill(
        &mut self,
        dst: &mut [u8],
        pitch: usize,
        width: u32,
        height: u32,
        now_100ns: i64,
    ) -> bool {
        if dst.len() < pitch * (height as usize + height as usize / 2) || pitch < width as usize {
            return false;
        }
        if let Some(view) = &self.view {
            let hdr = view.header();
            if hdr.slots.load(Ordering::Relaxed) == CAMSTUDIO_SLOTS {
                let sw = hdr.width.load(Ordering::SeqCst);
                let sh = hdr.height.load(Ordering::SeqCst);
                if sw == 0 || sh == 0 || nv12_bytes(sw, sh) > hdr.frame_bytes.load(Ordering::SeqCst)
                {
                    draw_no_signal(dst, pitch, width, height, now_100ns);
                    return false;
                }
                let src_len = sw as usize * (sh as usize + sh as usize / 2);

                // "No signal" only when the host really stopped (or never wrote):
                // racing the writer just means trying the newer frame.
                for _ in 0..4 {
                    let latest = hdr.latest.load(Ordering::SeqCst);
                    if latest == 0 {
                        break; // host never wrote
                    }
                    if now_100ns - hdr.write_time_100ns.load(Ordering::SeqCst)
                        > CAMSTUDIO_STALE_100NS
                    {
                        break; // host stopped
                    }
                    let slot = (latest & 3) as u32;
                    if slot >= CAMSTUDIO_SLOTS {
                        break;
                    }
                    let Some((slot_ptr, slot_bytes)) = view.slot(slot) else {
                        break;
                    };
                    if src_len > slot_bytes {
                        break;
                    }

                    hdr.slot_readers[slot as usize].fetch_add(1, Ordering::SeqCst);
                    let seq1 = hdr.slot_seq[slot as usize].load(Ordering::SeqCst);
                    let mut ok = false;
                    if seq1 & 1 == 0 {
                        // SAFETY: inside the view (`view.slot` and `src_len <= slot_bytes`). The
                        // writer may overwrite the slot meanwhile; the sequence number below
                        // tells and the copy is then thrown away (seqlock).
                        let src = unsafe { std::slice::from_raw_parts(slot_ptr, src_len) };
                        nv12_fit_into(src, sw as usize, sw, sh, dst, pitch, width, height);
                        fence(Ordering::SeqCst);
                        ok = hdr.slot_seq[slot as usize].load(Ordering::SeqCst) == seq1;
                    }
                    hdr.slot_readers[slot as usize].fetch_sub(1, Ordering::SeqCst);
                    if ok {
                        self.delivered = latest >> 2;
                        return true;
                    }
                }
                // Still racing after several tries (several readers at a very high rate): a
                // possibly torn copy of the latest frame beats a grey flash.
                let latest = hdr.latest.load(Ordering::SeqCst);
                if latest != 0
                    && now_100ns - hdr.write_time_100ns.load(Ordering::SeqCst)
                        <= CAMSTUDIO_STALE_100NS
                {
                    if let Some((slot_ptr, slot_bytes)) =
                        view.slot(((latest & 3) as u32) % CAMSTUDIO_SLOTS)
                    {
                        if src_len <= slot_bytes {
                            // SAFETY: as above; here the copy is delivered even if it raced.
                            let src = unsafe { std::slice::from_raw_parts(slot_ptr, src_len) };
                            nv12_fit_into(src, sw as usize, sw, sh, dst, pitch, width, height);
                            self.delivered = latest >> 2;
                            return true;
                        }
                    }
                }
            }
        }

        draw_no_signal(dst, pitch, width, height, now_100ns);
        false
    }
}

/// `FrameReader::DrawNoSignal`: dark grey with a sweeping bar so a stuck pipeline is easy to spot.
pub fn draw_no_signal(dst: &mut [u8], pitch: usize, width: u32, height: u32, now_100ns: i64) {
    let bar_w = (width / 16) & !1u32;
    let period = width as i64 + bar_w as i64;
    if period == 0 {
        return; // the C++ divides by zero here
    }
    let bar_x = (((now_100ns / 100_000) % period) as u32) & !1u32;
    let (wu, hu) = (width as usize, height as usize);
    for y in 0..hu {
        let row = &mut dst[y * pitch..y * pitch + wu];
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
    let uv = &mut dst[hu * pitch..];
    for y in 0..hu / 2 {
        uv[y * pitch..y * pitch + wu].fill(0x80);
    }
}
