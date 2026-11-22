//! niwcam-dshow: the DirectShow source filter "NiwCam" for Windows 10, which has no virtual camera API
//! (docs/windows10.md). Rust port of `src/dshow` (dllmain.cpp, SourceFilter.h/.cpp, dshow.def); builds
//! `CamStudioDShow.dll` for x64 and for i686 (a 32-bit app loads a 32-bit filter).
//!
//! It runs inside the app that uses the camera and reads the host's frames from the session-local
//! shared-memory channel (`reader`), the same NV12 slots the Windows 11 media source reads. Written without
//! the DirectShow base classes: one output pin of category CAPTURE, NV12 / YUY2 / I420, any of a few sizes,
//! paced at the rate the app asked for.
//!
//! The DLL exports exactly `DllCanUnloadNow`, `DllGetClassObject`, `DllRegisterServer` and
//! `DllUnregisterServer` (dshow.def).
#![deny(unsafe_op_in_unsafe_fn)]
#![allow(
    non_snake_case,
    reason = "the library is named CamStudioDShow like the DLL"
)]

pub mod com;
pub mod filter;
pub mod media_type;
pub mod pin;
pub mod reader;
pub mod registration;

use core::ffi::c_void;
use core::sync::atomic::{AtomicI32, AtomicPtr, Ordering::SeqCst};
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::{Mutex, MutexGuard};
use windows::core::{implement, IUnknown, Interface, Ref, BOOL, GUID, HRESULT, PCWSTR};
use windows::Win32::Foundation::{
    CLASS_E_CLASSNOTAVAILABLE, CLASS_E_NOAGGREGATION, E_FAIL, E_POINTER, HMODULE, S_FALSE,
};
use windows::Win32::System::Com::{IClassFactory, IClassFactory_Impl};
use windows::Win32::System::LibraryLoader::{
    GetModuleFileNameW, GetModuleHandleExW, GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS,
    GET_MODULE_HANDLE_EX_FLAG_UNCHANGED_REFCOUNT,
};

pub use filter::SourceFilter;
pub use pin::{fourcc_subtype, PinFormat, FOURCC_I420, FOURCC_NV12, FOURCC_YUY2};
pub use reader::FrameReader;

/// {7D2B9C41-3A6E-4F58-B0D1-9E4C2F6A8B13}: `CLSID_CamStudioDShow`.
pub const CLSID_CAMSTUDIO_DSHOW: GUID = GUID::from_u128(0x7d2b9c41_3a6e_4f58_b0d1_9e4c2f6a8b13);

/// What the pin needs to know of its filter: the graph state and the graph (not counted, per DirectShow).
pub struct FilterShared {
    pub state: AtomicI32,
    pub graph: AtomicPtr<c_void>,
}

impl Default for FilterShared {
    fn default() -> Self {
        Self {
            state: AtomicI32::new(windows::Win32::Media::DirectShow::State_Stopped.0),
            graph: AtomicPtr::new(core::ptr::null_mut()),
        }
    }
}

/// A mutex that ignores poisoning (a panicking COM method is caught and reported; the data stays usable).
pub(crate) fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// No panic may cross a COM boundary (it would take down the host app): a panic becomes `E_FAIL`.
pub(crate) fn guard(f: impl FnOnce() -> HRESULT) -> HRESULT {
    catch_unwind(AssertUnwindSafe(f)).unwrap_or(E_FAIL)
}

// ---- class factory ----------------------------------------------------------------------------

/// WRL's module object count: LockServer counts it, and nothing reads it (DllCanUnloadNow is always S_FALSE).
static OBJECT_COUNT: AtomicI32 = AtomicI32::new(0);

#[implement(IClassFactory, Agile = false)]
struct FilterFactory;

impl IClassFactory_Impl for FilterFactory_Impl {
    fn CreateInstance(
        &self,
        outer: Ref<'_, IUnknown>,
        riid: *const GUID,
        ppv: *mut *mut c_void,
    ) -> windows::core::Result<()> {
        let hr = guard(|| {
            if ppv.is_null() {
                return E_POINTER;
            }
            // SAFETY: ppv is non-null and writable (COM contract).
            unsafe { *ppv = core::ptr::null_mut() };
            if !outer.is_null() {
                return CLASS_E_NOAGGREGATION;
            }
            match SourceFilter::create() {
                Err(e) => e.code(),
                // SAFETY: riid and ppv are valid (contract); QueryInterface writes an AddRef'd pointer.
                Ok(filter) => unsafe {
                    filter.to_interface::<com::IBaseFilterX>().query(riid, ppv)
                },
            }
        });
        hr.ok()
    }

    fn LockServer(&self, lock: BOOL) -> windows::core::Result<()> {
        if lock.as_bool() {
            OBJECT_COUNT.fetch_add(1, SeqCst);
        } else {
            OBJECT_COUNT.fetch_sub(1, SeqCst);
        }
        Ok(())
    }
}

// ---- exports ----------------------------------------------------------------------------------

/// Stay loaded: the streaming thread runs our code, and an unload under it would crash the app.
#[no_mangle]
pub extern "system" fn DllCanUnloadNow() -> HRESULT {
    S_FALSE
}

/// # Safety
/// COM contract: `clsid` and `riid` are valid GUID pointers, `ppv` is null or writable.
#[no_mangle]
pub unsafe extern "system" fn DllGetClassObject(
    clsid: *const GUID,
    riid: *const GUID,
    ppv: *mut *mut c_void,
) -> HRESULT {
    guard(|| {
        if ppv.is_null() {
            return E_POINTER;
        }
        // SAFETY: ppv is non-null and writable; clsid and riid are valid per the COM contract.
        unsafe {
            *ppv = core::ptr::null_mut();
            if clsid.is_null() || *clsid != CLSID_CAMSTUDIO_DSHOW {
                return CLASS_E_CLASSNOTAVAILABLE;
            }
            let factory: IClassFactory = FilterFactory.into();
            factory.query(riid, ppv)
        }
    })
}

/// The path of this DLL. The C++ keeps its HMODULE from `DllMain`; there is no `DllMain` here, because a
/// `#[no_mangle]` one would be exported too and dshow.def does not export it.
fn module_path() -> Result<String, HRESULT> {
    let mut module = HMODULE::default();
    // SAFETY: the address belongs to this module; UNCHANGED_REFCOUNT keeps the load count as it was.
    unsafe {
        GetModuleHandleExW(
            GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS | GET_MODULE_HANDLE_EX_FLAG_UNCHANGED_REFCOUNT,
            PCWSTR(DllCanUnloadNow as *const () as *const u16),
            &mut module,
        )
        .map_err(|e| e.code())?;
    }
    let mut path = [0u16; 260];
    // SAFETY: the buffer is valid for MAX_PATH wide characters.
    let n = unsafe { GetModuleFileNameW(Some(module), &mut path) };
    if n == 0 {
        // SAFETY: GetLastError has no preconditions.
        return Err(HRESULT::from_win32(
            unsafe { windows::Win32::Foundation::GetLastError() }.0,
        ));
    }
    Ok(String::from_utf16_lossy(&path[..n as usize]))
}

/// Machine-wide (HKLM) from an elevated regsvr32 (the installer); for the current user (HKCU) otherwise.
#[no_mangle]
pub extern "system" fn DllRegisterServer() -> HRESULT {
    guard(|| match module_path() {
        Err(hr) => hr,
        Ok(path) => registration::register_server(&path),
    })
}

#[no_mangle]
pub extern "system" fn DllUnregisterServer() -> HRESULT {
    guard(registration::unregister_server)
}
