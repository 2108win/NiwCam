//! The inline helpers of `mfapi.h` the windows crate does not carry, with the same semantics, plus
//! small HRESULT helpers shared by the modules.
use windows::core::{GUID, HRESULT};
use windows::Win32::Media::MediaFoundation::IMFAttributes;

pub const S_OK: HRESULT = HRESULT(0);
pub const S_FALSE: HRESULT = HRESULT(1);
pub const E_UNEXPECTED: HRESULT = HRESULT(0x8000_FFFFu32 as i32);
pub const E_INVALIDARG: HRESULT = HRESULT(0x8007_0057u32 as i32);
pub const E_ACCESSDENIED: HRESULT = HRESULT(0x8007_0005u32 as i32);

/// `Result<()>` back to the HRESULT it came from (S_OK for Ok).
pub fn hr_of(r: windows::core::Result<()>) -> HRESULT {
    match r {
        Ok(()) => S_OK,
        Err(e) => e.code(),
    }
}

/// `MFGetAttribute2UINT32asUINT64`: Err (nothing written) when the attribute is missing.
pub fn get_2uint32(attrs: &IMFAttributes, key: &GUID) -> Option<(u32, u32)> {
    // SAFETY: `attrs` is a live COM interface; GetUINT64 only writes the returned value.
    let packed = unsafe { attrs.GetUINT64(key) }.ok()?;
    Some(((packed >> 32) as u32, packed as u32))
}

/// `MFGetAttributeSize`: leaves `w`/`h` untouched on failure, like the C++ out parameters.
pub fn get_attribute_size(a: &IMFAttributes, key: &GUID, w: &mut u32, h: &mut u32) {
    if let Some((hi, lo)) = get_2uint32(a, key) {
        *w = hi;
        *h = lo;
    }
}

/// `MFGetAttributeRatio` (numerator, denominator).
pub fn get_attribute_ratio(a: &IMFAttributes, key: &GUID, num: &mut u32, den: &mut u32) {
    get_attribute_size(a, key, num, den);
}

pub fn set_attribute_size(a: &IMFAttributes, key: &GUID, w: u32, h: u32) {
    // SAFETY: plain attribute store on a live interface.
    let _ = unsafe { a.SetUINT64(key, ((w as u64) << 32) | h as u64) };
}

pub fn set_attribute_ratio(a: &IMFAttributes, key: &GUID, num: u32, den: u32) {
    set_attribute_size(a, key, num, den);
}

/// `MFGetAttributeUINT32`: the default when the attribute is missing.
pub fn get_attribute_uint32(a: &IMFAttributes, key: &GUID, default: u32) -> u32 {
    // SAFETY: live COM interface, read only.
    unsafe { a.GetUINT32(key) }.unwrap_or(default)
}
