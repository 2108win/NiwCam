//! NiwCam virtual camera media source: the COM DLL the Windows 11 Frame Server loads
//! (`CamStudioVCam.dll`). Port of `src/vcam/*` (dllmain.cpp, Activate, MediaSource, MediaStream,
//! Common.h, vcam.def). Same CLSID, same exports (`DllGetClassObject`, `DllCanUnloadNow`,
//! `DllRegisterServer`, `DllUnregisterServer`), same behaviour.
//!
//! Differences from the C++ `dllmain.cpp`: there is no `DllMain`. The C++ one stored the module
//! handle and called `DisableThreadLibraryCalls`; here the module handle is looked up when needed
//! (`GetModuleHandleExW` on an address inside this DLL), and thread notifications stay on because
//! Rust's thread-local destructors run from them.

// The DLL's file name must stay `CamStudioVCam` (see CLAUDE.md), so the crate name is not snake case.
#![allow(non_snake_case)]
#![deny(unsafe_op_in_unsafe_fn)]

mod activate;
mod common;
mod media_source;
mod media_stream;

use std::ffi::c_void;
use std::sync::atomic::{AtomicI32, Ordering};

use windows::core::{implement, ComObject, Error, Interface, Ref, BOOL, GUID, HRESULT, PCWSTR};
use windows::Win32::Foundation::{
    CLASS_E_CLASSNOTAVAILABLE, CLASS_E_NOAGGREGATION, ERROR_FILE_NOT_FOUND, ERROR_SUCCESS, E_FAIL,
    E_POINTER, HMODULE, MAX_PATH, S_FALSE, S_OK, WIN32_ERROR,
};
use windows::Win32::System::Com::{IClassFactory, IClassFactory_Impl};
use windows::Win32::System::LibraryLoader::{
    GetModuleFileNameW, GetModuleHandleExW, GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS,
    GET_MODULE_HANDLE_EX_FLAG_UNCHANGED_REFCOUNT,
};
use windows::Win32::System::Registry::{
    RegCloseKey, RegCreateKeyExW, RegDeleteTreeW, RegSetValueExW, HKEY, HKEY_CURRENT_USER,
    HKEY_LOCAL_MACHINE, KEY_WRITE, REG_OPTION_NON_VOLATILE, REG_SZ,
};

use crate::activate::Activate;
use crate::common::{check, guard, vcam_log, CLSID_CAM_STUDIO_VCAM};

pub use media_source::build_formats;
pub use media_stream::VideoFormat;

/// What `Module<InProc>::IncrementObjectCount` counted in WRL: unobservable here, since
/// `DllCanUnloadNow` always says no.
static SERVER_LOCKS: AtomicI32 = AtomicI32::new(0);

#[implement(IClassFactory, Agile = false)]
struct ActivateFactory;

impl IClassFactory_Impl for ActivateFactory_Impl {
    fn CreateInstance(
        &self,
        outer: Ref<windows::core::IUnknown>,
        riid: *const GUID,
        ppv: *mut *mut c_void,
    ) -> windows::core::Result<()> {
        guard(|| {
            if ppv.is_null() {
                return Err(Error::from_hresult(E_POINTER));
            }
            // SAFETY: non-null out pointer from the caller.
            unsafe { *ppv = std::ptr::null_mut() };
            if !outer.is_null() {
                return Err(Error::from_hresult(CLASS_E_NOAGGREGATION));
            }
            let activate = ComObject::new(check!(Activate::new()));
            let unk: windows::core::IUnknown = activate.to_interface();
            // SAFETY: valid object; `riid` and `ppv` are the caller's.
            unsafe { unk.query(riid, ppv) }.ok()
        })
    }

    fn LockServer(&self, lock: BOOL) -> windows::core::Result<()> {
        guard(|| {
            SERVER_LOCKS.fetch_add(if lock.as_bool() { 1 } else { -1 }, Ordering::SeqCst);
            Ok(())
        })
    }
}

/// Runs an exported function; a panic becomes `E_FAIL` instead of unwinding into the caller.
fn export_guard(f: impl FnOnce() -> HRESULT) -> HRESULT {
    match std::panic::catch_unwind(std::panic::AssertUnwindSafe(f)) {
        Ok(hr) => hr,
        Err(_) => {
            vcam_log("panic caught at an exported function");
            E_FAIL
        }
    }
}

// Stay loaded: the stream worker thread runs our code, and unloading under it would crash Frame
// Server. The cost is one resident DLL.
#[unsafe(no_mangle)]
pub extern "system" fn DllCanUnloadNow() -> HRESULT {
    export_guard(|| S_FALSE)
}

/// # Safety
/// `clsid` and `riid` are null or point at GUIDs; `ppv` is null or points at a pointer to write.
#[unsafe(no_mangle)]
pub unsafe extern "system" fn DllGetClassObject(
    clsid: *const GUID,
    riid: *const GUID,
    ppv: *mut *mut c_void,
) -> HRESULT {
    export_guard(|| {
        if ppv.is_null() {
            return E_POINTER;
        }
        // SAFETY: non-null out pointer from the caller.
        unsafe { *ppv = std::ptr::null_mut() };
        // SAFETY: a non-null CLSID pointer is the caller's GUID.
        if clsid.is_null() || unsafe { *clsid } != CLSID_CAM_STUDIO_VCAM {
            return CLASS_E_CLASSNOTAVAILABLE;
        }
        let factory: IClassFactory = ActivateFactory.into();
        let unk: windows::core::IUnknown = factory.into();
        // SAFETY: valid object; `riid` and `ppv` are the caller's.
        unsafe { unk.query(riid, ppv) }
    })
}

// ---- Registration ----------------------------------------------------------

/// The registry hive and key of the COM class: HKLM (Frame Server runs as LocalService and cannot
/// see per-user COM registrations) -- except in test builds, which use their own HKCU key.
fn clsid_key() -> (HKEY, Vec<u16>) {
    #[cfg(feature = "test-names")]
    let (hive, path) = (
        HKEY_CURRENT_USER,
        format!(
            "Software\\NiwCamTest_{}\\CLSID\\{}",
            common::test_suffix(),
            niwcam_shm::CAMSTUDIO_VCAM_CLSID_STRING
        ),
    );
    #[cfg(not(feature = "test-names"))]
    let (hive, path) = (
        HKEY_LOCAL_MACHINE,
        format!(
            "Software\\Classes\\CLSID\\{}",
            niwcam_shm::CAMSTUDIO_VCAM_CLSID_STRING
        ),
    );
    #[cfg(feature = "test-names")]
    let _ = HKEY_LOCAL_MACHINE;
    #[cfg(not(feature = "test-names"))]
    let _ = HKEY_CURRENT_USER;
    (hive, path.encode_utf16().chain(Some(0)).collect())
}

fn utf16z(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(Some(0)).collect()
}

fn utf16_bytes(w: &[u16]) -> Vec<u8> {
    w.iter().flat_map(|c| c.to_le_bytes()).collect()
}

fn hresult_from_win32(code: u32) -> HRESULT {
    HRESULT::from_win32(code)
}

/// The handle of this DLL (the C++ stored it in `DllMain`).
fn module_handle() -> Option<HMODULE> {
    let mut module = HMODULE::default();
    // SAFETY: `DllCanUnloadNow` lives in this DLL; the flags ask for the module containing it.
    let ok = unsafe {
        GetModuleHandleExW(
            GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS | GET_MODULE_HANDLE_EX_FLAG_UNCHANGED_REFCOUNT,
            PCWSTR(DllCanUnloadNow as *const () as *const u16),
            &mut module,
        )
    };
    ok.ok().map(|_| module)
}

// Registration goes to HKLM: Frame Server runs as LocalService and cannot
// see per-user (HKCU) COM registrations. Requires an elevated regsvr32.
#[unsafe(no_mangle)]
pub extern "system" fn DllRegisterServer() -> HRESULT {
    export_guard(|| {
        let Some(module) = module_handle() else {
            return hresult_from_win32(windows::Win32::Foundation::ERROR_MOD_NOT_FOUND.0);
        };
        let mut path = [0u16; MAX_PATH as usize];
        // SAFETY: `path` is a valid buffer of MAX_PATH wide chars.
        let n = unsafe { GetModuleFileNameW(Some(module), &mut path) };
        if n == 0 {
            // SAFETY: plain thread-local read.
            let err = unsafe { windows::Win32::Foundation::GetLastError() };
            return hresult_from_win32(err.0);
        }

        let (hive, key) = clsid_key();
        let mut clsid_key_handle = HKEY::default();
        // SAFETY: valid NUL-terminated key name; `clsid_key_handle` receives the new key.
        let st = unsafe {
            RegCreateKeyExW(
                hive,
                PCWSTR(key.as_ptr()),
                None,
                PCWSTR::null(),
                REG_OPTION_NON_VOLATILE,
                KEY_WRITE,
                None,
                &mut clsid_key_handle,
                None,
            )
        };
        if st != ERROR_SUCCESS {
            return hresult_from_win32(st.0);
        }
        let name = utf16_bytes(&utf16z("NiwCam Virtual Camera"));
        // SAFETY: valid key; the default value (null name) is set to the name.
        unsafe {
            let _ = RegSetValueExW(clsid_key_handle, PCWSTR::null(), None, REG_SZ, Some(&name));
        }

        let mut inproc = HKEY::default();
        let sub = utf16z("InprocServer32");
        // SAFETY: valid key and sub-key name; `inproc` receives the new key.
        let mut st: WIN32_ERROR = unsafe {
            RegCreateKeyExW(
                clsid_key_handle,
                PCWSTR(sub.as_ptr()),
                None,
                PCWSTR::null(),
                REG_OPTION_NON_VOLATILE,
                KEY_WRITE,
                None,
                &mut inproc,
                None,
            )
        };
        if st == ERROR_SUCCESS {
            let len = path.iter().position(|c| *c == 0).unwrap_or(path.len());
            let mut with_nul: Vec<u16> = path[..len].to_vec();
            with_nul.push(0);
            let path_bytes = utf16_bytes(&with_nul);
            let model = utf16_bytes(&utf16z("Both"));
            let model_name = utf16z("ThreadingModel");
            // SAFETY: valid key; the first call sets the default value, the second the model.
            unsafe {
                let _ = RegSetValueExW(inproc, PCWSTR::null(), None, REG_SZ, Some(&path_bytes));
                st = RegSetValueExW(
                    inproc,
                    PCWSTR(model_name.as_ptr()),
                    None,
                    REG_SZ,
                    Some(&model),
                );
                let _ = RegCloseKey(inproc);
            }
        }
        // SAFETY: the key we opened above.
        unsafe {
            let _ = RegCloseKey(clsid_key_handle);
        }
        hresult_from_win32(st.0)
    })
}

#[unsafe(no_mangle)]
pub extern "system" fn DllUnregisterServer() -> HRESULT {
    export_guard(|| {
        let (hive, key) = clsid_key();
        // SAFETY: valid NUL-terminated key name.
        let st = unsafe { RegDeleteTreeW(hive, PCWSTR(key.as_ptr())) };
        if st == ERROR_SUCCESS || st == ERROR_FILE_NOT_FOUND {
            S_OK
        } else {
            hresult_from_win32(st.0)
        }
    })
}
