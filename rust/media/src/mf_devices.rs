//! Media Foundation helpers for enumerating cameras and their native formats.
//! Port of `src/common/MfDevices.h`, plus `BuildFormatOptions` / `ReadCameraFormats` of
//! `src/engine/CaptureEngine.cpp` (the "mode list, best first" the picker shows; they work on
//! `NativeFormat` only, so they live here and the capture engine just calls them).
use std::any::Any;
use std::sync::Arc;

use windows::core::{GUID, HRESULT, PWSTR};
use windows::Win32::Media::MediaFoundation::*;
use windows::Win32::System::Com::CoTaskMemFree;

use crate::mf_util::{self, S_OK};

/// `CAMSTUDIO_VCAM_FRIENDLY_NAME` / `CAMSTUDIO_VCAM_LEGACY_NAME` (`SharedFrame.h`).
pub const VCAM_FRIENDLY_NAME: &str = "NiwCam";
pub const VCAM_LEGACY_NAME: &str = "Cam Studio";

/// What the C++ keeps in `std::shared_ptr<phone::PhoneLink>`: the phone link lives in another
/// crate, so a phone camera carries it as an opaque handle the engine downcasts.
pub type PhoneHandle = Arc<dyn Any + Send + Sync>;

#[derive(Clone, Default)]
pub struct CameraDevice {
    pub name: String,
    pub symbolic_link: String,
    pub activate: Option<IMFActivate>,

    /// Phone camera (docs/protocol.md): `activate` is None, frames come through this
    /// authenticated connection instead.
    pub phone: Option<PhoneHandle>,
    /// A window on this PC (Windows.Graphics.Capture): `activate` and `phone` are None.
    /// The HWND value, 0 for none.
    pub window: isize,
}

// SAFETY: IMFActivate is free-threaded; the C++ passes camera descriptors between threads by value.
unsafe impl Send for CameraDevice {}
unsafe impl Sync for CameraDevice {}

impl CameraDevice {
    pub fn is_phone(&self) -> bool {
        self.phone.is_some()
    }
    pub fn is_pc_window(&self) -> bool {
        self.window != 0
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
#[repr(i32)]
pub enum LensFacing {
    #[default]
    None = 0, // webcam: not applicable
    Back = 1,
    Front = 2,
    External = 3,
}

#[derive(Clone, Debug)]
pub struct NativeFormat {
    pub index: u32,
    pub subtype: GUID,
    pub width: u32,
    pub height: u32,
    pub fps_num: u32,
    pub fps_den: u32,

    /// Phones: which camera on the phone this mode belongs to.
    pub phone_camera: String,
    pub lens: LensFacing,
}

impl Default for NativeFormat {
    fn default() -> Self {
        Self {
            index: 0,
            subtype: GUID::zeroed(),
            width: 0,
            height: 0,
            fps_num: 0,
            fps_den: 1,
            phone_camera: String::new(),
            lens: LensFacing::None,
        }
    }
}

impl NativeFormat {
    pub fn fps(&self) -> f64 {
        if self.fps_den != 0 {
            self.fps_num as f64 / self.fps_den as f64
        } else {
            0.0
        }
    }
    pub fn pixels(&self) -> u64 {
        self.width as u64 * self.height as u64
    }
}

pub fn subtype_name(g: &GUID) -> String {
    for (id, name) in [
        (MFVideoFormat_NV12, "NV12"),
        (MFVideoFormat_YUY2, "YUY2"),
        (MFVideoFormat_MJPG, "MJPG"),
        (MFVideoFormat_RGB24, "RGB24"),
        (MFVideoFormat_RGB32, "RGB32"),
        (MFVideoFormat_H264, "H264"),
        (MFVideoFormat_HEVC, "HEVC"),
        (MFVideoFormat_I420, "I420"),
        (MFVideoFormat_L8, "L8"),
    ] {
        if *g == id {
            return name.to_string();
        }
    }
    // swprintf_s(buf, L"%.4hs", &g.Data1): the first four bytes as characters, up to a NUL.
    g.data1
        .to_le_bytes()
        .iter()
        .take_while(|&&b| b != 0)
        .map(|&b| b as char)
        .collect()
}

fn pwstr_to_string(s: PWSTR) -> String {
    // SAFETY: `s` comes from GetAllocatedString: a valid NUL-terminated wide string.
    unsafe { s.to_string() }.unwrap_or_default()
}

pub fn enumerate_cameras() -> Vec<CameraDevice> {
    let mut result = Vec::new();
    // SAFETY: plain Media Foundation calls; the activate array is CoTaskMem-allocated and every
    // element is moved out (read) so its reference is released, then the array is freed.
    unsafe {
        let Ok(attrs) = MFCreateAttributes_(1) else {
            return result;
        };
        let _ = attrs.SetGUID(
            &MF_DEVSOURCE_ATTRIBUTE_SOURCE_TYPE,
            &MF_DEVSOURCE_ATTRIBUTE_SOURCE_TYPE_VIDCAP_GUID,
        );
        let mut devices: *mut Option<IMFActivate> = std::ptr::null_mut();
        let mut count = 0u32;
        if MFEnumDeviceSources(&attrs, &mut devices, &mut count).is_err() {
            return result;
        }
        for i in 0..count as usize {
            let mut d = CameraDevice {
                activate: devices.add(i).read(),
                ..Default::default()
            };
            if let Some(a) = &d.activate {
                let mut s = PWSTR::null();
                let mut len = 0u32;
                if a.GetAllocatedString(&MF_DEVSOURCE_ATTRIBUTE_FRIENDLY_NAME, &mut s, &mut len)
                    .is_ok()
                {
                    d.name = pwstr_to_string(s);
                    CoTaskMemFree(Some(s.0 as *const _));
                }
                if a.GetAllocatedString(
                    &MF_DEVSOURCE_ATTRIBUTE_SOURCE_TYPE_VIDCAP_SYMBOLIC_LINK,
                    &mut s,
                    &mut len,
                )
                .is_ok()
                {
                    d.symbolic_link = pwstr_to_string(s);
                    CoTaskMemFree(Some(s.0 as *const _));
                }
            }
            result.push(d);
        }
        CoTaskMemFree(Some(devices as *const _));
    }
    result
}

/// `MFCreateAttributes` returning the interface.
#[allow(non_snake_case)]
unsafe fn MFCreateAttributes_(initial: u32) -> windows::core::Result<IMFAttributes> {
    let mut a: Option<IMFAttributes> = None;
    // SAFETY: `a` is a valid out slot.
    unsafe { MFCreateAttributes(&mut a, initial)? };
    a.ok_or_else(|| windows::core::Error::from(mf_util::E_UNEXPECTED))
}

/// `towlower` as the engine runs it: the engine never calls `setlocale`, so it is the "C" locale and
/// only A-Z change (checked against the UCRT: Đ, Á, Ж stay as they are).
pub fn to_lower(s: &str) -> String {
    s.to_ascii_lowercase()
}

/// Windows lists software cameras as "<name> (Windows Virtual Camera)".
pub fn is_our_virtual_camera(d: &CameraDevice) -> bool {
    for ours in [VCAM_FRIENDLY_NAME, VCAM_LEGACY_NAME] {
        if d.name == ours || d.name.starts_with(&format!("{ours} (")) {
            return true;
        }
    }
    false
}

/// The same webcam in a fresh enumeration. After it was unplugged and plugged back in, the old
/// IMFActivate may point at a device that is gone: find it again by symbolic link (same USB
/// port), else by name (another port).
pub fn find_camera_again(old: &CameraDevice) -> Option<CameraDevice> {
    let all = enumerate_cameras();
    let link = to_lower(&old.symbolic_link);
    for c in &all {
        if !link.is_empty() && to_lower(&c.symbolic_link) == link {
            return Some(c.clone());
        }
    }
    for c in &all {
        if c.name == old.name && !is_our_virtual_camera(c) {
            return Some(c.clone());
        }
    }
    None
}

/// Lists the native formats of the first video stream of a media source. `handler_out`, when
/// given, receives the media type handler on success.
pub fn get_native_formats(
    source: &IMFMediaSource,
    out: &mut Vec<NativeFormat>,
    handler_out: Option<&mut Option<IMFMediaTypeHandler>>,
) -> HRESULT {
    out.clear();
    // SAFETY: plain COM calls on the source and what it hands out.
    unsafe {
        let pd = match source.CreatePresentationDescriptor() {
            Ok(p) => p,
            Err(e) => return e.code(),
        };
        let mut selected = windows::core::BOOL(0);
        let mut sd: Option<IMFStreamDescriptor> = None;
        if let Err(e) = pd.GetStreamDescriptorByIndex(0, &mut selected, &mut sd) {
            return e.code();
        }
        let Some(sd) = sd else {
            return mf_util::E_UNEXPECTED;
        };
        let handler = match sd.GetMediaTypeHandler() {
            Ok(h) => h,
            Err(e) => return e.code(),
        };
        let count = match handler.GetMediaTypeCount() {
            Ok(c) => c,
            Err(e) => return e.code(),
        };
        for i in 0..count {
            let Ok(ty) = handler.GetMediaTypeByIndex(i) else {
                continue;
            };
            let mut f = NativeFormat {
                index: i,
                ..Default::default()
            };
            if let Ok(g) = ty.GetGUID(&MF_MT_SUBTYPE) {
                f.subtype = g;
            }
            mf_util::get_attribute_size(&ty, &MF_MT_FRAME_SIZE, &mut f.width, &mut f.height);
            mf_util::get_attribute_ratio(&ty, &MF_MT_FRAME_RATE, &mut f.fps_num, &mut f.fps_den);
            out.push(f);
        }
        if let Some(h) = handler_out {
            *h = Some(handler);
        }
    }
    S_OK
}

/// Rank of a pixel format when two formats tie on size and frame rate.
/// Uncompressed formats avoid a decode step.
pub fn subtype_rank(g: &GUID) -> i32 {
    if *g == MFVideoFormat_NV12 {
        return 4;
    }
    if *g == MFVideoFormat_YUY2 {
        return 3;
    }
    if *g == MFVideoFormat_MJPG {
        return 2;
    }
    if *g == MFVideoFormat_H264 || *g == MFVideoFormat_HEVC {
        return 0; // need a decoder; avoid if possible
    }
    1
}

/// "Native resolution": the largest frame size, then the highest frame rate,
/// then the cheapest pixel format to convert.
pub fn pick_largest(formats: &[NativeFormat]) -> Option<&NativeFormat> {
    let mut best: Option<&NativeFormat> = None;
    for f in formats {
        if f.width == 0 || f.height == 0 {
            continue;
        }
        let Some(b) = best else {
            best = Some(f);
            continue;
        };
        if f.pixels() != b.pixels() {
            if f.pixels() > b.pixels() {
                best = Some(f);
            }
            continue;
        }
        if f.fps() != b.fps() {
            if f.fps() > b.fps() {
                best = Some(f);
            }
            continue;
        }
        if subtype_rank(&f.subtype) > subtype_rank(&b.subtype) {
            best = Some(f);
        }
    }
    best
}

/// Picks a device by index ("2") or case-insensitive name substring
/// ("obsbot"). With an empty selector it takes the first camera that is not
/// our own virtual camera.
pub fn select_camera<'a>(cams: &'a [CameraDevice], selector: &str) -> Option<&'a CameraDevice> {
    if selector.is_empty() {
        return cams.iter().find(|c| !is_our_virtual_camera(c));
    }
    if selector.chars().all(|c| c.is_ascii_digit()) {
        // _wtoi: a number too big for an int is not a valid index either way.
        let i = selector.parse::<usize>().unwrap_or(usize::MAX);
        return cams.get(i);
    }
    let needle = to_lower(selector);
    cams.iter()
        .find(|c| !is_our_virtual_camera(c) && to_lower(&c.name).contains(&needle))
}

// ---- BuildFormatOptions / ReadCameraFormats (CaptureEngine.cpp) ---------------------------

/// One entry of the resolution picker: a distinct size + frame rate, backed
/// by the camera's cheapest-to-convert native subtype for that combination.
#[derive(Clone, Debug, Default)]
pub struct FormatOption {
    pub native: NativeFormat,
    /// the camera's (or this lens') native, biggest mode
    pub largest: bool,
    /// phones: e.g. "Camera sau · góc rộng"
    pub lens_label: String,
}

pub fn build_format_options(formats: &[NativeFormat]) -> Vec<FormatOption> {
    let mut options: Vec<FormatOption> = Vec::new();
    for f in formats {
        if f.width == 0 || f.height == 0 || f.fps_num == 0 {
            continue;
        }
        let same = options.iter().position(|o| {
            o.native.width == f.width
                && o.native.height == f.height
                && (o.native.fps() - f.fps()).abs() < 0.01
        });
        match same {
            None => options.push(FormatOption {
                native: f.clone(),
                largest: false,
                lens_label: String::new(),
            }),
            Some(i) => {
                if subtype_rank(&f.subtype) > subtype_rank(&options[i].native.subtype) {
                    options[i].native = f.clone();
                }
            }
        }
    }
    // The C++ uses std::sort (unstable). Two options with the same width, height and rate were
    // merged above, so they only tie when the pixel count and rate match with another shape
    // (e.g. 1280x720 and 960x960 at the same rate). MSVC's introsort is an insertion sort up to 32
    // elements, which keeps the input order like this stable sort; beyond that a tie could come
    // out in another order (no real camera lists such a pair).
    options.sort_by(|a, b| {
        if a.native.pixels() != b.native.pixels() {
            return b.native.pixels().cmp(&a.native.pixels());
        }
        b.native
            .fps()
            .partial_cmp(&a.native.fps())
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    if let Some(first) = options.first_mut() {
        first.largest = true;
    }
    options
}

pub fn read_camera_formats(cam: &CameraDevice, options: &mut Vec<FormatOption>) -> HRESULT {
    options.clear();
    let Some(activate) = &cam.activate else {
        return mf_util::E_UNEXPECTED;
    };
    // SAFETY: activate/shutdown the source like the C++.
    unsafe {
        let source = match activate.ActivateObject::<IMFMediaSource>() {
            Ok(s) => s,
            Err(e) => return e.code(),
        };
        let mut formats = Vec::new();
        let hr = get_native_formats(&source, &mut formats, None);
        let _ = source.Shutdown();
        let _ = activate.ShutdownObject();
        if hr.is_ok() {
            *options = build_format_options(&formats);
        }
        hr
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fmt(sub: GUID, w: u32, h: u32, n: u32, d: u32) -> NativeFormat {
        NativeFormat {
            subtype: sub,
            width: w,
            height: h,
            fps_num: n,
            fps_den: d,
            ..Default::default()
        }
    }

    fn cam(name: &str) -> CameraDevice {
        CameraDevice {
            name: name.into(),
            ..Default::default()
        }
    }

    #[test]
    fn our_virtual_camera_names() {
        assert!(is_our_virtual_camera(&cam("NiwCam")));
        assert!(is_our_virtual_camera(&cam(
            "NiwCam (Windows Virtual Camera)"
        )));
        assert!(is_our_virtual_camera(&cam("Cam Studio")));
        assert!(is_our_virtual_camera(&cam("Cam Studio (x)")));
        assert!(!is_our_virtual_camera(&cam("NiwCam2")));
        assert!(!is_our_virtual_camera(&cam("niwcam")));
        assert!(!is_our_virtual_camera(&cam("")));
    }

    #[test]
    fn select_by_index_name_and_default() {
        let cams = vec![
            cam("NiwCam (Windows Virtual Camera)"),
            cam("OBSBOT Meet"),
            cam("Other"),
        ];
        assert_eq!(select_camera(&cams, "").unwrap().name, "OBSBOT Meet");
        assert_eq!(
            select_camera(&cams, "0").unwrap().name,
            "NiwCam (Windows Virtual Camera)"
        );
        assert!(select_camera(&cams, "3").is_none());
        assert_eq!(select_camera(&cams, "obs").unwrap().name, "OBSBOT Meet");
        assert!(select_camera(&cams, "niwcam").is_none()); // never our own by name
    }

    #[test]
    fn largest_and_option_order() {
        let list = vec![
            fmt(MFVideoFormat_MJPG, 1920, 1080, 30, 1),
            fmt(MFVideoFormat_NV12, 1920, 1080, 30, 1),
            fmt(MFVideoFormat_MJPG, 1920, 1080, 60, 1),
            fmt(MFVideoFormat_YUY2, 1280, 720, 30, 1),
            fmt(MFVideoFormat_YUY2, 0, 0, 90, 1),
            fmt(MFVideoFormat_NV12, 640, 480, 0, 1),
        ];
        let best = pick_largest(&list).unwrap();
        assert_eq!((best.width, best.fps_num), (1920, 60));
        let options = build_format_options(&list);
        assert_eq!(options.len(), 3);
        assert!(options[0].largest && options[0].native.fps_num == 60);
        // the 1080p30 option keeps the cheapest subtype (NV12 over MJPG)
        assert_eq!(options[1].native.subtype, MFVideoFormat_NV12);
        assert_eq!(options[2].native.height, 720);
    }

    #[test]
    fn subtype_names() {
        assert_eq!(subtype_name(&MFVideoFormat_NV12), "NV12");
        let mut odd = MFVideoFormat_NV12;
        odd.data1 = 0x4142_4344;
        assert_eq!(subtype_name(&odd), "DCBA");
        odd.data1 = 0x4142;
        assert_eq!(subtype_name(&odd), "BA");
    }
}
