//! PNG snapshot through WIC: `still::SavePng` of `src/engine/StillPicture.cpp` (the part the
//! recorder's sibling "Chụp ảnh" needs). The rest of StillPicture (Direct2D pictures) is not here.
use niwcam_core::nv12::nv12_to_bgra;
use windows::core::{HRESULT, PCWSTR};
use windows::Win32::Foundation::GENERIC_WRITE;
use windows::Win32::Graphics::Imaging::*;
use windows::Win32::Storage::FileSystem::DeleteFileW;
use windows::Win32::System::Com::{CoCreateInstance, CLSCTX_INPROC_SERVER};

use crate::mf_util::{E_INVALIDARG, S_OK};

/// `WINCODEC_ERR_UNSUPPORTEDPIXELFORMAT`.
const UNSUPPORTED_PIXEL_FORMAT: HRESULT = HRESULT(0x8898_2F80u32 as i32);

/// Saves a contiguous NV12 picture (`w * h` luma bytes, then `w * h / 2` UV bytes) as a PNG
/// (32-bit BGRA, alpha 255). Any failure after the first step deletes the target file, like the C++.
pub fn save_png(path: &str, nv12: &[u8], w: u32, h: u32) -> HRESULT {
    if nv12.is_empty() || w < 2 || h < 2 {
        return E_INVALIDARG;
    }
    let luma = w as usize * h as usize;
    let mut bgra = vec![0u32; luma];
    nv12_to_bgra(
        &nv12[..luma],
        &nv12[luma..],
        w as usize,
        w,
        h,
        &mut bgra,
        w,
        h,
    );
    let bytes: Vec<u8> = bgra.iter().flat_map(|p| p.to_le_bytes()).collect();

    let wide: Vec<u16> = path.encode_utf16().chain(std::iter::once(0)).collect();
    let result = (|| -> windows::core::Result<HRESULT> {
        // SAFETY: plain WIC calls on interfaces created here; `wide` and `bytes` outlive them.
        unsafe {
            let wic: IWICImagingFactory =
                CoCreateInstance(&CLSID_WICImagingFactory, None, CLSCTX_INPROC_SERVER)?;
            let stream = wic.CreateStream()?;
            stream.InitializeFromFilename(PCWSTR(wide.as_ptr()), GENERIC_WRITE.0)?;
            let encoder = wic.CreateEncoder(&GUID_ContainerFormatPng, std::ptr::null())?;
            encoder.Initialize(&stream, WICBitmapEncoderNoCache)?;
            let mut frame = None;
            encoder.CreateNewFrame(&mut frame, std::ptr::null_mut())?;
            let frame = frame.ok_or(windows::core::Error::from(E_INVALIDARG))?;
            frame.Initialize(None)?;
            frame.SetSize(w, h)?;
            let mut format = GUID_WICPixelFormat32bppBGRA; // alpha 255 everywhere (Nv12ToBgra)
            frame.SetPixelFormat(&mut format)?;
            if format != GUID_WICPixelFormat32bppBGRA {
                return Ok(UNSUPPORTED_PIXEL_FORMAT);
            }
            frame.WritePixels(h, w * 4, &bytes)?;
            frame.Commit()?;
            encoder.Commit()?;
            drop(stream);
            Ok(S_OK)
        }
    })();
    let hr = match result {
        Ok(hr) => hr,
        Err(e) => e.code(),
    };
    if hr.is_err() {
        // no half-written file left behind
        // SAFETY: `wide` is NUL-terminated.
        let _ = unsafe { DeleteFileW(PCWSTR(wide.as_ptr())) };
    }
    hr
}
