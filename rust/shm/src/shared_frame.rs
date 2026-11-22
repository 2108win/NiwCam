//! The shared contract between the host app (writer) and the virtual camera media source
//! (reader). Port of `src/common/SharedFrame.h`: the header layout (byte for byte), the
//! constants, the kernel-object names and the SDDL.

use std::sync::atomic::{AtomicI32, AtomicI64, AtomicU32, AtomicU64, Ordering};

/// COM class of the virtual camera media source: {72F5B2FD-C48E-449B-95CE-E8F13BF200CB}.
pub const CAMSTUDIO_VCAM_CLSID_STRING: &str = "{72F5B2FD-C48E-449B-95CE-E8F13BF200CB}";

/// A GUID as (data1, data2, data3, data4), the shape of `windows::core::GUID::from_values`.
pub type GuidValues = (u32, u16, u16, [u8; 8]);

/// `CLSID_CamStudioVCam`.
pub const CLSID_CAMSTUDIO_VCAM: GuidValues = (
    0x72f5_b2fd,
    0xc48e,
    0x449b,
    [0x95, 0xce, 0xe8, 0xf1, 0x3b, 0xf2, 0x00, 0xcb],
);

/// `CAMSTUDIO_ATTR_NATIVE_SIZE`: UINT64 packed like `MF_MT_FRAME_SIZE` (width << 32 | height).
pub const CAMSTUDIO_ATTR_NATIVE_SIZE: GuidValues = (
    0x7274_f18f,
    0x32c3,
    0x4935,
    [0xa7, 0xdf, 0xae, 0x15, 0xff, 0xc5, 0x8f, 0xdc],
);
/// `CAMSTUDIO_ATTR_NATIVE_FPS`: UINT64 packed like `MF_MT_FRAME_RATE` (numerator << 32 | denominator).
pub const CAMSTUDIO_ATTR_NATIVE_FPS: GuidValues = (
    0xa653_111f,
    0xe52f,
    0x433f,
    [0x82, 0x8f, 0x0a, 0xc1, 0x65, 0xd9, 0x65, 0xf2],
);

pub const CAMSTUDIO_VCAM_FRIENDLY_NAME: &str = "NiwCam";
/// What the camera was called before 0.1.14; still recognised as ours.
pub const CAMSTUDIO_VCAM_LEGACY_NAME: &str = "Cam Studio";

/// Kernel object names. "Global\" is required: the reader runs in session 0.
pub const CAMSTUDIO_MAPPING_NAME: &str = "Global\\CamStudioVCam_Frames_v2";
pub const CAMSTUDIO_EVENT_NAME: &str = "Global\\CamStudioVCam_NewFrame_v2";

/// The DACL the media source creates the global objects with so the unelevated host can open them.
pub const CAMSTUDIO_OBJECT_SDDL: &str =
    "D:(A;;GA;;;SY)(A;;GA;;;LS)(A;;GA;;;BA)(A;;GRGW;;;IU)(A;;GRGW;;;AU)";

/// Windows 10: the same channel in the session-local namespace.
pub const CAMSTUDIO_LOCAL_MAPPING_NAME: &str = "Local\\CamStudioVCam_Frames_v2";
pub const CAMSTUDIO_LOCAL_EVENT_NAME: &str = "Local\\CamStudioVCam_NewFrame_v2";
/// 4K either way up fits.
pub const CAMSTUDIO_LOCAL_MAX_PIXELS: u32 = 4096 * 2304;

/// "CMS1".
pub const CAMSTUDIO_MAGIC: u32 = 0x3153_4D43;
pub const CAMSTUDIO_VERSION: u32 = 2;

/// Frames older than this are treated as "no signal" (2 s, in 100 ns units).
pub const CAMSTUDIO_STALE_100NS: i64 = 20_000_000;

/// Frame slots: the writer fills a slot that is neither the latest one nor being read.
pub const CAMSTUDIO_SLOTS: u32 = 3;

/// `#pragma pack(push, 8)` struct `SharedFrameHeader`. All fields are atomics because the other
/// side (another process) reads and writes them while we do; the `volatile LONG64` fields of the
/// C++ are the `Interlocked*` ones. Layout: size 128, offsets checked below and by `xcheck`.
#[repr(C)]
pub struct SharedFrameHeader {
    /// `CAMSTUDIO_MAGIC` once the reader has initialised.
    pub magic: AtomicU32,
    /// `CAMSTUDIO_VERSION`.
    pub version: AtomicU32,
    /// NV12 frame width the mapping was sized for.
    pub width: AtomicU32,
    pub height: AtomicU32,
    /// Bytes per luma row (== width).
    pub stride: AtomicU32,
    /// `CAMSTUDIO_SLOTS`.
    pub slots: AtomicU32,
    /// One slot: stride * height * 3 / 2 (the local channel: its capacity).
    pub frame_bytes: AtomicU64,
    /// Latest complete frame: (frame number << 2) | slot. 0 = nothing written yet.
    pub latest: AtomicI64,
    /// `MFGetSystemTime()` of the last frame.
    pub write_time_100ns: AtomicI64,
    /// > 0 while a stream is running.
    pub reader_active: AtomicI32,
    pub reserved1: AtomicU32,
    /// Per-slot seqlock (odd while the writer fills it).
    pub slot_seq: [AtomicI64; CAMSTUDIO_SLOTS as usize],
    /// Readers copying each slot right now: the writer avoids those slots.
    pub slot_readers: [AtomicI32; CAMSTUDIO_SLOTS as usize],
    pub reserved2: AtomicU32,
    pub pad: [u8; 32],
    // CAMSTUDIO_SLOTS NV12 frames follow at offset sizeof(SharedFrameHeader).
}

/// `sizeof(SharedFrameHeader)`.
pub const HEADER_SIZE: usize = std::mem::size_of::<SharedFrameHeader>();

// The layout the C++ compiler produces for the `pack(8)` struct (printed by xcheck from C++ too).
const _: () = {
    use std::mem::{align_of, offset_of};
    assert!(HEADER_SIZE == 128);
    assert!(align_of::<SharedFrameHeader>() == 8);
    assert!(offset_of!(SharedFrameHeader, magic) == 0);
    assert!(offset_of!(SharedFrameHeader, version) == 4);
    assert!(offset_of!(SharedFrameHeader, width) == 8);
    assert!(offset_of!(SharedFrameHeader, height) == 12);
    assert!(offset_of!(SharedFrameHeader, stride) == 16);
    assert!(offset_of!(SharedFrameHeader, slots) == 20);
    assert!(offset_of!(SharedFrameHeader, frame_bytes) == 24);
    assert!(offset_of!(SharedFrameHeader, latest) == 32);
    assert!(offset_of!(SharedFrameHeader, write_time_100ns) == 40);
    assert!(offset_of!(SharedFrameHeader, reader_active) == 48);
    assert!(offset_of!(SharedFrameHeader, reserved1) == 52);
    assert!(offset_of!(SharedFrameHeader, slot_seq) == 56);
    assert!(offset_of!(SharedFrameHeader, slot_readers) == 80);
    assert!(offset_of!(SharedFrameHeader, reserved2) == 92);
    assert!(offset_of!(SharedFrameHeader, pad) == 96);
};

/// `sizeof` and `offsetof` of every field, in the order and format `xcheck` prints from C++.
pub fn layout_lines() -> Vec<String> {
    use std::mem::offset_of;
    // Printed with the C++ field names so the two outputs can be compared as text.
    macro_rules! off {
        ($f:ident, $cpp:literal) => {
            format!("offsetof {} {}", $cpp, offset_of!(SharedFrameHeader, $f))
        };
    }
    vec![
        format!("sizeof SharedFrameHeader {}", HEADER_SIZE),
        off!(magic, "magic"),
        off!(version, "version"),
        off!(width, "width"),
        off!(height, "height"),
        off!(stride, "stride"),
        off!(slots, "slots"),
        off!(frame_bytes, "frameBytes"),
        off!(latest, "latest"),
        off!(write_time_100ns, "writeTime100ns"),
        off!(reader_active, "readerActive"),
        off!(reserved1, "reserved1"),
        off!(slot_seq, "slotSeq"),
        off!(slot_readers, "slotReaders"),
        off!(reserved2, "reserved2"),
        off!(pad, "pad"),
    ]
}

/// `CamStudioNv12Bytes`.
pub fn nv12_bytes(w: u32, h: u32) -> u64 {
    w as u64 * h as u64 * 3 / 2
}

/// `CamStudioMappingBytes`.
pub fn mapping_bytes(w: u32, h: u32) -> u64 {
    HEADER_SIZE as u64 + CAMSTUDIO_SLOTS as u64 * nv12_bytes(w, h)
}

/// `CamStudioLocalSlotBytes`.
pub fn local_slot_bytes() -> u64 {
    CAMSTUDIO_LOCAL_MAX_PIXELS as u64 * 3 / 2
}

/// `CamStudioLocalMappingBytes`.
pub fn local_mapping_bytes() -> u64 {
    HEADER_SIZE as u64 + CAMSTUDIO_SLOTS as u64 * local_slot_bytes()
}

impl SharedFrameHeader {
    /// `CamStudioSlot`: where slot `slot` starts.
    ///
    /// # Safety
    /// `this` points at a mapped header followed by at least `slot + 1` slots of `frame_bytes`
    /// each (callers check the view size, see `ShmView::frame_bytes_fit`).
    pub unsafe fn slot_ptr(this: *const SharedFrameHeader, slot: u32) -> *mut u8 {
        // SAFETY: by the contract above; the offset stays inside the mapping.
        unsafe {
            let frame_bytes = (*this).frame_bytes.load(Ordering::Relaxed);
            (this as *mut u8).add(HEADER_SIZE + slot as usize * frame_bytes as usize)
        }
    }

    /// `CamStudioInitHeader`: everything but the magic, then the magic (readers and the host wait
    /// for it). `frame_bytes` is the slot stride: the frame itself, or the local channel's capacity.
    pub fn init(&self, width: u32, height: u32, frame_bytes: u64) {
        self.version.store(CAMSTUDIO_VERSION, Ordering::Relaxed);
        self.width.store(width, Ordering::Relaxed);
        self.height.store(height, Ordering::Relaxed);
        self.stride.store(width, Ordering::Relaxed);
        self.slots.store(CAMSTUDIO_SLOTS, Ordering::Relaxed);
        self.frame_bytes.store(frame_bytes, Ordering::Relaxed);
        self.latest.store(0, Ordering::Relaxed);
        self.write_time_100ns.store(0, Ordering::Relaxed);
        self.reader_active.store(0, Ordering::Relaxed);
        for s in 0..CAMSTUDIO_SLOTS as usize {
            self.slot_seq[s].store(0, Ordering::Relaxed);
            self.slot_readers[s].store(0, Ordering::Relaxed);
        }
        // MemoryBarrier(), then the magic.
        std::sync::atomic::fence(Ordering::SeqCst);
        self.magic.store(CAMSTUDIO_MAGIC, Ordering::SeqCst);
    }
}

/// `CamStudioClampLocalSize`: a picture larger than the local slots are sized for is written
/// smaller, same shape.
pub fn clamp_local_size(w: &mut u32, h: &mut u32) {
    let pixels = *w as u64 * *h as u64;
    if pixels <= CAMSTUDIO_LOCAL_MAX_PIXELS as u64 || pixels == 0 {
        return;
    }
    let k = (CAMSTUDIO_LOCAL_MAX_PIXELS as f64 / pixels as f64).sqrt();
    // C++: static_cast<uint32_t>(w * k) & ~1u  (w * k in double, truncated)
    *w = ((*w as f64 * k) as u32) & !1u32;
    *h = ((*h as f64 * k) as u32) & !1u32;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sizes() {
        assert_eq!(nv12_bytes(1920, 1080), 1920 * 1080 * 3 / 2);
        assert_eq!(mapping_bytes(2, 2), 128 + 3 * 6);
        assert_eq!(local_slot_bytes(), 4096 * 2304 * 3 / 2);
    }

    #[test]
    fn clamp() {
        let (mut w, mut h) = (1920, 1080);
        clamp_local_size(&mut w, &mut h);
        assert_eq!((w, h), (1920, 1080));
        let (mut w, mut h) = (7680, 4320);
        clamp_local_size(&mut w, &mut h);
        assert!(w as u64 * h as u64 <= CAMSTUDIO_LOCAL_MAX_PIXELS as u64);
        assert!(w % 2 == 0 && h % 2 == 0);
        let (mut w, mut h) = (0, 5000);
        clamp_local_size(&mut w, &mut h);
        assert_eq!((w, h), (0, 5000));
    }
}
