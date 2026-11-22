//! Registration of the filter: ports the registry half of `src/dshow/dllmain.cpp`.
//!
//! Machine-wide (HKLM) from an elevated regsvr32 (the installer); for the current user (HKCU) otherwise,
//! which is enough to develop and test without elevation. The keys are created here first so the filter
//! mapper's writes (through HKCR) land in the same hive.

use core::ffi::c_void;
use core::mem::size_of;
use core::ptr::null;
use windows::core::{GUID, HRESULT, PCWSTR, PWSTR};
use windows::Win32::Foundation::{CloseHandle, ERROR_SUCCESS, HANDLE, S_OK};
use windows::Win32::Media::DirectShow::{
    IFilterMapper2, MERIT_DO_NOT_USE, REGFILTER2, REGFILTER2_0, REGFILTER2_0_0, REGFILTERPINS,
    REGPINTYPES,
};
use windows::Win32::Media::MediaFoundation::{
    CLSID_FilterMapper2, CLSID_VideoInputDeviceCategory, MEDIATYPE_Video,
};
use windows::Win32::Security::{GetTokenInformation, TokenElevation, TOKEN_ELEVATION, TOKEN_QUERY};
use windows::Win32::System::Com::{
    CoCreateInstance, CoInitializeEx, CoUninitialize, CLSCTX_INPROC_SERVER,
    COINIT_APARTMENTTHREADED,
};
use windows::Win32::System::Registry::{
    RegCloseKey, RegCreateKeyExW, RegDeleteTreeW, RegSetValueExW, HKEY, HKEY_CURRENT_USER,
    HKEY_LOCAL_MACHINE, KEY_WRITE, REG_OPTION_NON_VOLATILE, REG_SZ,
};
use windows::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};

use crate::pin::{fourcc_subtype, FOURCC_NV12};
use crate::CLSID_CAMSTUDIO_DSHOW;

/// `CAMSTUDIO_DSHOW_CLSID_STRING`.
pub const CLSID_STRING: &str = "{7D2B9C41-3A6E-4F58-B0D1-9E4C2F6A8B13}";
/// `CAMSTUDIO_DSHOW_FRIENDLY_NAME`: the same name as the Windows 11 camera.
pub const FRIENDLY_NAME: &str = "NiwCam";
/// The value of the CLSID key itself.
pub const CLSID_DESCRIPTION: &str = "NiwCam DirectShow Camera";

/// `kCategoryInstance` ({860BB310-5D01-11d0-BD3B-00A0C911CE86} is CLSID_VideoInputDeviceCategory).
pub fn category_instance_key() -> String {
    format!(
        r"Software\Classes\CLSID\{{860BB310-5D01-11d0-BD3B-00A0C911CE86}}\Instance\{CLSID_STRING}"
    )
}

/// `kClsidKey`.
pub fn clsid_key() -> String {
    format!(r"Software\Classes\CLSID\{CLSID_STRING}")
}

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(Some(0)).collect()
}

/// `Elevated()`.
pub fn elevated() -> bool {
    let mut token = HANDLE::default();
    // SAFETY: plain Win32 calls; the token is closed again.
    unsafe {
        if OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token).is_err() {
            return false;
        }
        let mut e = TOKEN_ELEVATION::default();
        let mut n = 0u32;
        let ok = GetTokenInformation(
            token,
            TokenElevation,
            Some(&mut e as *mut _ as *mut c_void),
            size_of::<TOKEN_ELEVATION>() as u32,
            &mut n,
        )
        .is_ok()
            && e.TokenIsElevated != 0;
        let _ = CloseHandle(token);
        ok
    }
}

/// `SetString`: a REG_SZ value (the unnamed one when `name` is None). Returns the LSTATUS.
fn set_string(key: HKEY, name: Option<&str>, value: &str) -> u32 {
    let name_w = name.map(wide);
    let value_w = wide(value);
    // SAFETY: the wide strings are NUL-terminated and outlive the call; the byte view covers the string and
    // its NUL exactly, as `(size() + 1) * sizeof(wchar_t)` does.
    unsafe {
        let bytes = core::slice::from_raw_parts(value_w.as_ptr() as *const u8, value_w.len() * 2);
        RegSetValueExW(
            key,
            name_w
                .as_ref()
                .map_or(PCWSTR::null(), |n| PCWSTR(n.as_ptr())),
            None,
            REG_SZ,
            Some(bytes),
        )
        .0
    }
}

fn create_key(root: HKEY, path: &str, out: &mut HKEY) -> u32 {
    let p = wide(path);
    // SAFETY: `p` is NUL-terminated and outlives the call; `out` is a valid out slot.
    unsafe {
        RegCreateKeyExW(
            root,
            PCWSTR(p.as_ptr()),
            None,
            PCWSTR::null(),
            REG_OPTION_NON_VOLATILE,
            KEY_WRITE,
            None,
            out,
            None,
        )
        .0
    }
}

/// The CLSID key, its `InprocServer32` and the capture-device entry apps enumerate (ICreateDevEnum), under
/// `root`: the part of `DllRegisterServer` before the filter mapper. `module_path` is the DLL's path.
pub fn write_registration(root: HKEY, module_path: &str) -> HRESULT {
    let mut clsid = HKEY::default();
    let mut st = create_key(root, &clsid_key(), &mut clsid);
    if st != ERROR_SUCCESS.0 {
        return HRESULT::from_win32(st);
    }
    set_string(clsid, None, CLSID_DESCRIPTION);
    let mut inproc = HKEY::default();
    st = create_key(clsid, "InprocServer32", &mut inproc);
    if st == ERROR_SUCCESS.0 {
        set_string(inproc, None, module_path);
        st = set_string(inproc, Some("ThreadingModel"), "Both");
        // SAFETY: the key was opened above.
        let _ = unsafe { RegCloseKey(inproc) };
    }
    // SAFETY: the key was opened above.
    let _ = unsafe { RegCloseKey(clsid) };
    if st != ERROR_SUCCESS.0 {
        return HRESULT::from_win32(st);
    }

    // The capture-device entry apps enumerate (ICreateDevEnum).
    let mut instance = HKEY::default();
    st = create_key(root, &category_instance_key(), &mut instance);
    if st != ERROR_SUCCESS.0 {
        return HRESULT::from_win32(st);
    }
    set_string(instance, Some("CLSID"), CLSID_STRING);
    st = set_string(instance, Some("FriendlyName"), FRIENDLY_NAME);
    // SAFETY: the key was opened above.
    let _ = unsafe { RegCloseKey(instance) };
    if st != ERROR_SUCCESS.0 {
        return HRESULT::from_win32(st);
    }
    S_OK
}

/// The part of `DllUnregisterServer` that deletes keys under one root.
pub fn delete_registration(root: HKEY) {
    let inst = wide(&category_instance_key());
    let clsid = wide(&clsid_key());
    // SAFETY: NUL-terminated strings that outlive the calls; results ignored as in the C++.
    unsafe {
        let _ = RegDeleteTreeW(root, PCWSTR(inst.as_ptr()));
        let _ = RegDeleteTreeW(root, PCWSTR(clsid.as_ptr()));
    }
}

/// `RegisterWithMapper`: the filter's merit, pin and media types for the filter mapper (FilterData under the
/// category instance, which intelligent-connect and some apps read).
pub fn register_with_mapper(unregister: bool) -> HRESULT {
    // SAFETY: COM initialisation is paired with CoUninitialize below; every pointer handed to the mapper
    // outlives the call.
    unsafe {
        let init = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
        let hr = match CoCreateInstance::<_, IFilterMapper2>(
            &CLSID_FilterMapper2,
            None,
            CLSCTX_INPROC_SERVER,
        ) {
            Err(e) => e.code(),
            Ok(mapper) => {
                let instance = wide(CLSID_STRING);
                if unregister {
                    mapper
                        .UnregisterFilter(
                            &CLSID_VideoInputDeviceCategory,
                            PCWSTR(instance.as_ptr()),
                            &CLSID_CAMSTUDIO_DSHOW,
                        )
                        .map_or_else(|e| e.code(), |_| S_OK)
                } else {
                    let subtype = fourcc_subtype(FOURCC_NV12);
                    let types = [REGPINTYPES {
                        clsMajorType: &MEDIATYPE_Video,
                        clsMinorType: &subtype,
                    }];
                    let mut pin_name = wide("Capture");
                    let null_clsid = GUID::zeroed();
                    let pins = [REGFILTERPINS {
                        strName: PWSTR(pin_name.as_mut_ptr()),
                        bRendered: false.into(),
                        bOutput: true.into(),
                        bZero: false.into(),
                        bMany: false.into(),
                        clsConnectsToFilter: &null_clsid, // CLSID_NULL
                        strConnectsToPin: PCWSTR(null()),
                        nMediaTypes: 1,
                        lpMediaType: types.as_ptr(),
                    }];
                    let rf = REGFILTER2 {
                        dwVersion: 1,
                        // a capture device: picked by name, never by intelligent connect
                        dwMerit: MERIT_DO_NOT_USE.0 as u32,
                        Anonymous: REGFILTER2_0 {
                            Anonymous1: REGFILTER2_0_0 {
                                cPins: 1,
                                rgPins: pins.as_ptr(),
                            },
                        },
                    };
                    let name = wide(FRIENDLY_NAME);
                    mapper
                        .RegisterFilter(
                            &CLSID_CAMSTUDIO_DSHOW,
                            PCWSTR(name.as_ptr()),
                            None,
                            &CLSID_VideoInputDeviceCategory,
                            PCWSTR(instance.as_ptr()),
                            &rf,
                        )
                        .map_or_else(|e| e.code(), |_| S_OK)
                }
            }
        };
        if init.is_ok() {
            CoUninitialize();
        }
        hr
    }
}

/// The roots `DllUnregisterServer` cleans: HKLM only when elevated.
pub fn unregister_roots(elevated: bool) -> Vec<HKEY> {
    let mut v = Vec::new();
    if elevated {
        v.push(HKEY_LOCAL_MACHINE);
    }
    v.push(HKEY_CURRENT_USER);
    v
}

/// `DllRegisterServer` (after the module path is known).
pub fn register_server(module_path: &str) -> HRESULT {
    let root = if elevated() {
        HKEY_LOCAL_MACHINE
    } else {
        HKEY_CURRENT_USER
    };
    let hr = write_registration(root, module_path);
    if hr.is_err() {
        return hr;
    }
    let _ = register_with_mapper(false); // FilterData; the entry above works without it
    S_OK
}

/// `DllUnregisterServer`.
pub fn unregister_server() -> HRESULT {
    let _ = register_with_mapper(true);
    for root in unregister_roots(elevated()) {
        delete_registration(root);
    }
    S_OK
}
