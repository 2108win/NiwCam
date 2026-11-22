//! AM_MEDIA_TYPE helpers (no DirectShow base classes). Ports the top of `src/dshow/SourceFilter.cpp`.

use core::mem::{size_of, ManuallyDrop};
use core::ptr;
use windows::core::HRESULT;
use windows::Win32::Foundation::{E_OUTOFMEMORY, S_OK};
use windows::Win32::Media::MediaFoundation::AM_MEDIA_TYPE;
use windows::Win32::System::Com::{CoTaskMemAlloc, CoTaskMemFree};

/// `FreeMediaTypeFields`.
///
/// # Safety
/// `mt` must be a valid media type whose `pbFormat` came from `CoTaskMemAlloc`.
pub unsafe fn free_media_type_fields(mt: &mut AM_MEDIA_TYPE) {
    if mt.cbFormat != 0 && !mt.pbFormat.is_null() {
        // SAFETY: pbFormat was allocated with CoTaskMemAlloc (precondition).
        unsafe { CoTaskMemFree(Some(mt.pbFormat as *const _)) };
    }
    mt.cbFormat = 0;
    mt.pbFormat = ptr::null_mut();
    // SAFETY: the field is reset right below, so the old value is dropped exactly once (Release).
    unsafe { ManuallyDrop::drop(&mut mt.pUnk) };
    mt.pUnk = ManuallyDrop::new(None);
}

/// `CopyMediaTypeFields`: a bitwise copy, then a private copy of the format block and an AddRef of `pUnk`.
///
/// # Safety
/// `dst` must be valid for writes (it may be uninitialised), `src` a valid media type.
pub unsafe fn copy_media_type_fields(dst: *mut AM_MEDIA_TYPE, src: &AM_MEDIA_TYPE) -> HRESULT {
    // SAFETY: dst is writable, src readable, they do not overlap (a bitwise copy of the C++ `dst = src`).
    unsafe {
        ptr::copy_nonoverlapping(src as *const AM_MEDIA_TYPE, dst, 1);
        (*dst).pbFormat = ptr::null_mut();
        if src.cbFormat != 0 && !src.pbFormat.is_null() {
            let p = CoTaskMemAlloc(src.cbFormat as usize) as *mut u8;
            if p.is_null() {
                return E_OUTOFMEMORY;
            }
            ptr::copy_nonoverlapping(src.pbFormat as *const u8, p, src.cbFormat as usize);
            (*dst).pbFormat = p;
        } else {
            (*dst).cbFormat = 0;
        }
        if let Some(unk) = (*dst).pUnk.as_ref() {
            core::mem::forget(unk.clone()); // AddRef
        }
    }
    S_OK
}

/// `AllocMediaType`: a CoTaskMem copy of `src` (null on failure).
///
/// # Safety
/// `src` must be a valid media type.
pub unsafe fn alloc_media_type(src: &AM_MEDIA_TYPE) -> *mut AM_MEDIA_TYPE {
    // SAFETY: the allocation is checked, and freed again when the copy fails.
    unsafe {
        let mt = CoTaskMemAlloc(size_of::<AM_MEDIA_TYPE>()) as *mut AM_MEDIA_TYPE;
        if mt.is_null() {
            return ptr::null_mut();
        }
        if copy_media_type_fields(mt, src).is_err() {
            CoTaskMemFree(Some(mt as *const _));
            return ptr::null_mut();
        }
        mt
    }
}

/// `DeleteMediaType`.
///
/// # Safety
/// `mt` must be null or a media type allocated by [`alloc_media_type`] (or `CoTaskMemAlloc`).
#[allow(
    dead_code,
    reason = "kept with the other helpers, as in SourceFilter.cpp"
)]
pub unsafe fn delete_media_type(mt: *mut AM_MEDIA_TYPE) {
    if mt.is_null() {
        return;
    }
    // SAFETY: precondition.
    unsafe {
        free_media_type_fields(&mut *mt);
        CoTaskMemFree(Some(mt as *const _));
    }
}
