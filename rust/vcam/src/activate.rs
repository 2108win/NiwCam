//! Port of `src/vcam/Activate.h` / `Activate.cpp`: the COM object Frame Server creates from our
//! CLSID. Frame Server copies the attributes the host set on IMFVirtualCamera into it, then calls
//! `ActivateObject` to get the media source. The `IMFAttributes` methods are delegated, argument
//! for argument, to an internal store (through its vtable, so null pointers and error codes pass
//! through untouched, like the C++).

use std::ffi::c_void;
use std::sync::Mutex;

use windows::core::{implement, ComObject, Error, Interface, Ref, BOOL, GUID, PCWSTR, PWSTR};
use windows::Win32::Foundation::E_POINTER;
use windows::Win32::Media::MediaFoundation::*;
use windows::Win32::System::Com::StructuredStorage::PROPVARIANT;

use crate::common::{check, guard, lock};
use crate::media_source::MediaSource;

#[implement(IMFActivate, Agile = false)]
pub struct Activate {
    attrs: IMFAttributes,
    source: Mutex<Option<ComObject<MediaSource>>>,
}

// SAFETY: the C++ object has no lock and is used by Frame Server from one thread at a time; the
// attribute store is free-threaded and `source` is behind a mutex.
unsafe impl Send for Activate {}
// SAFETY: see above.
unsafe impl Sync for Activate {}

impl Activate {
    /// `RuntimeClassInitialize`: `MFCreateAttributes(&m_attrs, 4)`.
    pub fn new() -> windows::core::Result<Activate> {
        let mut attrs: Option<IMFAttributes> = None;
        // SAFETY: plain MF call with a valid out parameter.
        check!(unsafe { MFCreateAttributes(&mut attrs, 4) });
        let attrs = attrs.ok_or_else(|| Error::from_hresult(E_POINTER))?;
        Ok(Activate {
            attrs,
            source: Mutex::new(None),
        })
    }
}

/// The raw interface pointer of a `Ref` (null for a null reference).
fn raw<T: Interface>(r: &Ref<T>) -> *mut c_void {
    r.as_ref().map_or(std::ptr::null_mut(), |i| i.as_raw())
}

impl IMFActivate_Impl for Activate_Impl {
    fn ActivateObject(
        &self,
        riid: *const GUID,
        ppv: *mut *mut c_void,
    ) -> windows::core::Result<()> {
        guard(|| {
            #[cfg(feature = "diag-file")]
            crate::common::vcam_log("ActivateObject");
            if ppv.is_null() {
                return Err(Error::from_hresult(E_POINTER));
            }
            // SAFETY: non-null out pointer from the caller.
            unsafe { *ppv = std::ptr::null_mut() };

            let mut slot = lock(&self.source);
            // Frame Server may call Shutdown on the source directly and then activate
            // again; never hand out a source that has been shut down.
            if slot.as_ref().is_some_and(|s| s.get().is_shutdown()) {
                *slot = None;
            }
            if slot.is_none() {
                *slot = Some(check!(MediaSource::create(&self.attrs)));
            }
            let source = slot
                .as_ref()
                .ok_or_else(|| Error::from_hresult(E_POINTER))?;
            let unk: windows::core::IUnknown = source.to_interface();
            // SAFETY: valid object; `riid` and `ppv` are the caller's.
            unsafe { unk.query(riid, ppv) }.ok()
        })
    }

    fn ShutdownObject(&self) -> windows::core::Result<()> {
        guard(|| {
            let source = lock(&self.source).take();
            if let Some(s) = source {
                s.get().do_shutdown();
            }
            Ok(())
        })
    }

    fn DetachObject(&self) -> windows::core::Result<()> {
        guard(|| {
            *lock(&self.source) = None;
            Ok(())
        })
    }
}

macro_rules! vt {
    ($s:expr, $m:ident $(, $a:expr)*) => {{
        let a = &$s.attrs;
        crate::common::guard_hr(|| {
            // SAFETY: a call through the vtable of our own live attribute store; the arguments
            // are the caller's, forwarded unchanged like the C++.
            unsafe { (Interface::vtable(a).$m)(Interface::as_raw(a) $(, $a)*) }
        })
    }};
}

impl IMFAttributes_Impl for Activate_Impl {
    fn GetItem(&self, k: *const GUID, v: *mut PROPVARIANT) -> windows::core::Result<()> {
        vt!(self, GetItem, k, v).ok()
    }
    fn GetItemType(&self, k: *const GUID) -> windows::core::Result<MF_ATTRIBUTE_TYPE> {
        let mut t = MF_ATTRIBUTE_TYPE::default();
        vt!(self, GetItemType, k, &mut t).ok()?;
        Ok(t)
    }
    fn CompareItem(&self, k: *const GUID, v: *const PROPVARIANT) -> windows::core::Result<BOOL> {
        let mut r = BOOL(0);
        vt!(self, CompareItem, k, v, &mut r).ok()?;
        Ok(r)
    }
    fn Compare(
        &self,
        o: Ref<IMFAttributes>,
        m: MF_ATTRIBUTES_MATCH_TYPE,
    ) -> windows::core::Result<BOOL> {
        let mut r = BOOL(0);
        vt!(self, Compare, raw(&o), m, &mut r).ok()?;
        Ok(r)
    }
    fn GetUINT32(&self, k: *const GUID) -> windows::core::Result<u32> {
        let mut v = 0u32;
        vt!(self, GetUINT32, k, &mut v).ok()?;
        Ok(v)
    }
    fn GetUINT64(&self, k: *const GUID) -> windows::core::Result<u64> {
        let mut v = 0u64;
        vt!(self, GetUINT64, k, &mut v).ok()?;
        Ok(v)
    }
    fn GetDouble(&self, k: *const GUID) -> windows::core::Result<f64> {
        let mut v = 0f64;
        vt!(self, GetDouble, k, &mut v).ok()?;
        Ok(v)
    }
    fn GetGUID(&self, k: *const GUID) -> windows::core::Result<GUID> {
        let mut v = GUID::zeroed();
        vt!(self, GetGUID, k, &mut v).ok()?;
        Ok(v)
    }
    fn GetStringLength(&self, k: *const GUID) -> windows::core::Result<u32> {
        let mut v = 0u32;
        vt!(self, GetStringLength, k, &mut v).ok()?;
        Ok(v)
    }
    fn GetString(
        &self,
        k: *const GUID,
        v: PWSTR,
        n: u32,
        l: *mut u32,
    ) -> windows::core::Result<()> {
        vt!(self, GetString, k, v, n, l).ok()
    }
    fn GetAllocatedString(
        &self,
        k: *const GUID,
        v: *mut PWSTR,
        l: *mut u32,
    ) -> windows::core::Result<()> {
        vt!(self, GetAllocatedString, k, v, l).ok()
    }
    fn GetBlobSize(&self, k: *const GUID) -> windows::core::Result<u32> {
        let mut v = 0u32;
        vt!(self, GetBlobSize, k, &mut v).ok()?;
        Ok(v)
    }
    fn GetBlob(
        &self,
        k: *const GUID,
        b: *mut u8,
        n: u32,
        s: *mut u32,
    ) -> windows::core::Result<()> {
        vt!(self, GetBlob, k, b, n, s).ok()
    }
    fn GetAllocatedBlob(
        &self,
        k: *const GUID,
        b: *mut *mut u8,
        s: *mut u32,
    ) -> windows::core::Result<()> {
        vt!(self, GetAllocatedBlob, k, b, s).ok()
    }
    fn GetUnknown(
        &self,
        k: *const GUID,
        r: *const GUID,
        p: *mut *mut c_void,
    ) -> windows::core::Result<()> {
        vt!(self, GetUnknown, k, r, p).ok()
    }
    fn SetItem(&self, k: *const GUID, v: *const PROPVARIANT) -> windows::core::Result<()> {
        vt!(self, SetItem, k, v).ok()
    }
    fn DeleteItem(&self, k: *const GUID) -> windows::core::Result<()> {
        vt!(self, DeleteItem, k).ok()
    }
    fn DeleteAllItems(&self) -> windows::core::Result<()> {
        vt!(self, DeleteAllItems).ok()
    }
    fn SetUINT32(&self, k: *const GUID, v: u32) -> windows::core::Result<()> {
        vt!(self, SetUINT32, k, v).ok()
    }
    fn SetUINT64(&self, k: *const GUID, v: u64) -> windows::core::Result<()> {
        vt!(self, SetUINT64, k, v).ok()
    }
    fn SetDouble(&self, k: *const GUID, v: f64) -> windows::core::Result<()> {
        vt!(self, SetDouble, k, v).ok()
    }
    fn SetGUID(&self, k: *const GUID, v: *const GUID) -> windows::core::Result<()> {
        vt!(self, SetGUID, k, v).ok()
    }
    fn SetString(&self, k: *const GUID, v: &PCWSTR) -> windows::core::Result<()> {
        vt!(self, SetString, k, *v).ok()
    }
    fn SetBlob(&self, k: *const GUID, b: *const u8, s: u32) -> windows::core::Result<()> {
        vt!(self, SetBlob, k, b, s).ok()
    }
    fn SetUnknown(
        &self,
        k: *const GUID,
        p: Ref<windows::core::IUnknown>,
    ) -> windows::core::Result<()> {
        vt!(self, SetUnknown, k, raw(&p)).ok()
    }
    fn LockStore(&self) -> windows::core::Result<()> {
        vt!(self, LockStore).ok()
    }
    fn UnlockStore(&self) -> windows::core::Result<()> {
        vt!(self, UnlockStore).ok()
    }
    fn GetCount(&self) -> windows::core::Result<u32> {
        let mut v = 0u32;
        vt!(self, GetCount, &mut v).ok()?;
        Ok(v)
    }
    fn GetItemByIndex(
        &self,
        i: u32,
        k: *mut GUID,
        v: *mut PROPVARIANT,
    ) -> windows::core::Result<()> {
        vt!(self, GetItemByIndex, i, k, v).ok()
    }
    fn CopyAllItems(&self, d: Ref<IMFAttributes>) -> windows::core::Result<()> {
        vt!(self, CopyAllItems, raw(&d)).ok()
    }
}
