//! Writer side of the shared-memory channel (the host app). Port of `src/engine/FrameWriter.h`.
//!
//! The global mapping is created by the media source inside Frame Server, so it only exists
//! once some app has opened the virtual camera. Until then `write` is a cheap no-op.

use std::sync::atomic::Ordering;

use niwcam_core::nv12::nv12_scale;
use windows::core::Error;
use windows::Win32::Foundation::INVALID_HANDLE_VALUE;
use windows::Win32::Foundation::{GetLastError, ERROR_ALREADY_EXISTS, E_UNEXPECTED, HANDLE};
use windows::Win32::System::Memory::{
    CreateFileMappingW, OpenFileMappingW, FILE_MAP_READ, FILE_MAP_WRITE, PAGE_READWRITE,
};
use windows::Win32::System::SystemInformation::GetTickCount64;
use windows::Win32::System::Threading::{
    CreateEventW, OpenEventW, SetEvent, Sleep, EVENT_MODIFY_STATE,
};

use crate::os::{last_error, mf_system_time, nv12_min_len, Names, OwnedHandle, View};
use crate::shared_frame::*;

pub struct FrameWriter {
    names: Names,
    view: Option<View>,
    event: Option<OwnedHandle>,
    last_attempt: u64,
    /// `ReleaseHold` since the last `write`: `hold_for` does nothing.
    released: bool,
    /// The Windows 10 channel (`open_local`), owned by the host.
    local: bool,
    /// Which channel `try_open` looks for.
    use_local: bool,
}

impl Default for FrameWriter {
    fn default() -> Self {
        Self::new()
    }
}

impl FrameWriter {
    /// A writer on the real kernel-object names.
    pub fn new() -> Self {
        Self::with_names(Names::default())
    }

    /// A writer on other names (tests).
    pub fn with_names(names: Names) -> Self {
        Self {
            names,
            view: None,
            event: None,
            last_attempt: 0,
            released: false,
            local: false,
            use_local: false,
        }
    }

    pub fn is_open(&self) -> bool {
        self.view.is_some()
    }

    pub fn consumer_active(&self) -> bool {
        self.view
            .as_ref()
            .is_some_and(|v| v.header().reader_active.load(Ordering::SeqCst) > 0)
    }

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

    /// Windows 10: the local channel (`open_local`) instead of Frame Server's global one.
    pub fn use_local(&mut self, local: bool) {
        self.use_local = local;
    }

    pub fn is_local(&self) -> bool {
        self.local
    }

    /// Opens the channel the media source created, at most every 500 ms. A mapping that is not
    /// ours (magic, version, slots, local capacity) is closed again.
    pub fn try_open(&mut self) {
        if self.view.is_some() {
            return;
        }
        // SAFETY: no arguments.
        let now = unsafe { GetTickCount64() };
        if now.wrapping_sub(self.last_attempt) < 500 {
            return;
        }
        self.last_attempt = now;

        let names = self.names.channel(self.use_local).clone();
        // SAFETY: the name is a NUL-terminated wide string that outlives the call.
        let Ok(mapping) = (unsafe {
            OpenFileMappingW(FILE_MAP_READ.0 | FILE_MAP_WRITE.0, false, names.mapping())
        }) else {
            return;
        };
        let Ok(view) = View::map(mapping, FILE_MAP_READ | FILE_MAP_WRITE) else {
            return;
        };
        {
            let h = view.header();
            if h.magic.load(Ordering::SeqCst) != CAMSTUDIO_MAGIC
                || h.version.load(Ordering::Relaxed) != CAMSTUDIO_VERSION
                || h.slots.load(Ordering::Relaxed) != CAMSTUDIO_SLOTS
                || (self.use_local && h.frame_bytes.load(Ordering::Relaxed) != local_slot_bytes())
                || !view.frame_bytes_fit()
            {
                return; // `view` drops: unmapped and closed
            }
        }
        self.view = Some(view);
        self.local = self.use_local;
        // SAFETY: the name is a NUL-terminated wide string that outlives the call.
        self.event = unsafe { OpenEventW(EVENT_MODIFY_STATE, false, names.event()) }
            .ok()
            .map(OwnedHandle);
    }

    /// Windows 10 (no virtual camera API): the host owns the channel. Opens the local mapping, or
    /// creates it, and makes the host's picture w x h unless a DirectShow reader streams right now
    /// (then its size stays, and the caller fits the picture into it). Returns the size in force.
    pub fn open_local(&mut self, w: u32, h: u32) -> Result<(u32, u32), Error> {
        let (mut w, mut h) = (w, h);
        clamp_local_size(&mut w, &mut h);
        if self.view.is_none() {
            let names = self.names.channel(true).clone();
            let bytes = local_mapping_bytes();
            // SAFETY: the name is a NUL-terminated wide string that outlives the call.
            let created = unsafe {
                CreateFileMappingW(
                    INVALID_HANDLE_VALUE,
                    None,
                    PAGE_READWRITE,
                    (bytes >> 32) as u32,
                    bytes as u32,
                    names.mapping(),
                )
            };
            let mapping = created?;
            // SAFETY: plain thread-local read, directly after the call it describes.
            let existed = unsafe { GetLastError() } == ERROR_ALREADY_EXISTS;
            let view = View::map(mapping, FILE_MAP_READ | FILE_MAP_WRITE)?;
            {
                let hdr = view.header();
                if existed && hdr.magic.load(Ordering::SeqCst) != CAMSTUDIO_MAGIC {
                    for _ in 0..100 {
                        if hdr.magic.load(Ordering::SeqCst) == CAMSTUDIO_MAGIC {
                            break;
                        }
                        // SAFETY: plain Sleep.
                        unsafe { Sleep(1) };
                    }
                }
                if !existed || hdr.magic.load(Ordering::SeqCst) != CAMSTUDIO_MAGIC {
                    hdr.init(w, h, local_slot_bytes());
                } else if hdr.version.load(Ordering::Relaxed) != CAMSTUDIO_VERSION
                    || hdr.slots.load(Ordering::Relaxed) != CAMSTUDIO_SLOTS
                    || hdr.frame_bytes.load(Ordering::Relaxed) != local_slot_bytes()
                {
                    // The layout of another build: nothing to do but wait for it to go.
                    return Err(Error::from_hresult(E_UNEXPECTED));
                }
            }
            self.view = Some(view);
            // SAFETY: the name is a NUL-terminated wide string that outlives the call.
            self.event = unsafe { CreateEventW(None, false, false, names.event()) }
                .ok()
                .map(OwnedHandle);
            self.local = true;
        }
        let hdr = match &self.view {
            Some(v) => v.header(),
            None => return Err(last_error()),
        };
        if (hdr.width.load(Ordering::SeqCst) != w || hdr.height.load(Ordering::SeqCst) != h)
            && hdr.reader_active.load(Ordering::SeqCst) <= 0
        {
            // Every slot "being written" while the size changes: a reader that raced it sees the
            // sequence move and takes the next frame instead of a torn one.
            for seq in &hdr.slot_seq {
                seq.fetch_add(1, Ordering::SeqCst);
            }
            hdr.width.store(w, Ordering::SeqCst);
            hdr.height.store(h, Ordering::SeqCst);
            hdr.stride.store(w, Ordering::SeqCst);
            hdr.latest.swap(0, Ordering::SeqCst); // the old picture has the wrong size now
            for seq in &hdr.slot_seq {
                seq.fetch_add(1, Ordering::SeqCst);
            }
        }
        Ok((
            hdr.width.load(Ordering::SeqCst),
            hdr.height.load(Ordering::SeqCst),
        ))
    }

    pub fn close(&mut self) {
        self.local = false;
        self.view = None;
        self.event = None;
    }

    /// Publishes one NV12 frame (`src`: w x h, rows `src_pitch` bytes apart, the UV plane right
    /// after `h` rows). Scales only if the mapping was sized for a different format.
    ///
    /// Unlike the C++ it does nothing when `src` is too short for w x h x pitch or the header's
    /// size does not fit its slot (the C++ would read or write out of bounds).
    pub fn write(&mut self, src: &[u8], src_pitch: usize, w: u32, h: u32) {
        let Some(view) = &self.view else { return };
        let hdr = view.header();
        if w == 0
            || h == 0
            || src_pitch < w as usize
            || src.len() < nv12_min_len(src_pitch, w as usize, h as usize)
        {
            return;
        }
        let (dw, dh) = (
            hdr.width.load(Ordering::SeqCst),
            hdr.height.load(Ordering::SeqCst),
        );
        let stride = hdr.stride.load(Ordering::SeqCst) as usize;
        let dst_len = stride * (dh as usize + dh as usize / 2);
        if dw == 0 || dh == 0 || stride < dw as usize {
            return;
        }

        // A slot that is neither the one readers are told to use nor being read.
        let latest = hdr.latest.load(Ordering::SeqCst);
        let latest_slot = if latest != 0 {
            (latest & 3) as u32
        } else {
            CAMSTUDIO_SLOTS
        };
        let mut slot = CAMSTUDIO_SLOTS;
        for s in 0..CAMSTUDIO_SLOTS {
            if slot != CAMSTUDIO_SLOTS {
                break;
            }
            if s != latest_slot && hdr.slot_readers[s as usize].load(Ordering::SeqCst) == 0 {
                slot = s;
            }
        }
        for s in 0..CAMSTUDIO_SLOTS {
            if slot != CAMSTUDIO_SLOTS {
                break;
            }
            if s != latest_slot {
                slot = s; // every other slot busy: the reader's seqlock catches it
            }
        }
        let Some((slot_ptr, slot_bytes)) = view.slot(slot) else {
            return;
        };
        if dst_len > slot_bytes {
            return;
        }

        hdr.slot_seq[slot as usize].fetch_add(1, Ordering::SeqCst); // odd: writing
                                                                    // SAFETY: `slot_ptr` is inside the view and `dst_len <= slot_bytes`. Readers in another
                                                                    // process may read these bytes meanwhile; the odd sequence number tells them (seqlock),
                                                                    // and they discard what they copied.
        let dst = unsafe { std::slice::from_raw_parts_mut(slot_ptr, dst_len) };
        nv12_scale(src, src_pitch, w, h, dst, stride, dw, dh);
        hdr.slot_seq[slot as usize].fetch_add(1, Ordering::SeqCst); // even: done
        hdr.write_time_100ns
            .swap(mf_system_time(), Ordering::SeqCst);
        hdr.latest
            .swap((((latest >> 2) + 1) << 2) | slot as i64, Ordering::SeqCst);
        self.released = false;
        if let Some(ev) = &self.event {
            // SAFETY: a valid event handle we own.
            unsafe {
                let _ = SetEvent(ev.0);
            }
        }
    }

    /// The latest picture on the camera (NV12, contiguous w x h): the paused camera blurs it.
    pub fn copy_latest(&self, out: &mut Vec<u8>) -> Option<(u32, u32)> {
        let view = self.view.as_ref()?;
        let hdr = view.header();
        let latest = hdr.latest.load(Ordering::SeqCst);
        if latest == 0 {
            return None;
        }
        let w = hdr.width.load(Ordering::SeqCst);
        let h = hdr.height.load(Ordering::SeqCst);
        let stride = hdr.stride.load(Ordering::SeqCst) as usize;
        if w == 0 || h == 0 || nv12_bytes(w, h) > hdr.frame_bytes.load(Ordering::SeqCst) {
            return None;
        }
        let (slot_ptr, slot_bytes) = view.slot((hdr.latest.load(Ordering::SeqCst) & 3) as u32)?;
        let src_len = stride * (h as usize + h as usize / 2);
        if stride < w as usize || src_len > slot_bytes {
            return None;
        }
        out.resize(nv12_bytes(w, h) as usize, 0);
        // SAFETY: inside the view (checked above). The writer may overwrite this slot while we
        // copy (the C++ has the same race: a torn picture for the paused blur is harmless).
        let src = unsafe { std::slice::from_raw_parts(slot_ptr, src_len) };
        nv12_scale(src, stride, w, h, out, w as usize, w, h);
        Some((w, h))
    }

    /// Same, only if it was written in the last 2 s (what a reader would still show).
    pub fn copy_latest_fresh(&self, out: &mut Vec<u8>) -> Option<(u32, u32)> {
        let view = self.view.as_ref()?;
        if mf_system_time() - view.header().write_time_100ns.load(Ordering::SeqCst)
            > CAMSTUDIO_STALE_100NS
        {
            return None;
        }
        self.copy_latest(out)
    }

    /// Keeps the latest frame good for readers for `ms` more, so an app keeps showing it (frozen)
    /// instead of "no signal" while the next source opens. Nothing is copied: only its time
    /// moves (ahead of now: readers check the age with a signed difference). The next `write` or
    /// `release_hold` sets the real time again.
    pub fn hold_for(&self, ms: u32) {
        // After a loss the old picture stays gone: retries must not bring it back frozen.
        let Some(view) = &self.view else { return };
        let hdr = view.header();
        if hdr.latest.load(Ordering::SeqCst) == 0 || self.released {
            return;
        }
        let until = mf_system_time() + ms as i64 * 10000 - CAMSTUDIO_STALE_100NS;
        if until > hdr.write_time_100ns.load(Ordering::SeqCst) {
            hdr.write_time_100ns.swap(until, Ordering::SeqCst);
        }
    }

    /// The source is gone: the held frame ages from now, and readers show "no signal" in 2 s.
    pub fn release_hold(&mut self) {
        self.released = true; // until the next frame is written
        let Some(view) = &self.view else { return };
        let hdr = view.header();
        let now = mf_system_time();
        if hdr.write_time_100ns.load(Ordering::SeqCst) > now {
            hdr.write_time_100ns.swap(now, Ordering::SeqCst);
        }
    }

    /// The raw event handle ("new frame"), if the channel has one (tests).
    pub fn event_handle(&self) -> Option<HANDLE> {
        self.event.as_ref().map(|e| e.0)
    }
}
