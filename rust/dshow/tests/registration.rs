//! The registry half of `DllRegisterServer`, run against a private scratch key (never the real
//! `HKCU\Software\Classes\CLSID\{7D2B9C41-...}`, which a registered filter of the installed app may own):
//! `write_registration(root, path)` writes what `src/dshow/dllmain.cpp` writes, under `root`.
//! `DllRegisterServer` itself (which would also call the filter mapper, writing through HKCR) is not run.
#![allow(non_snake_case)]
#![allow(unsafe_code)]

use windows::core::PCWSTR;
use windows::Win32::Foundation::{ERROR_SUCCESS, S_OK};
use windows::Win32::System::Registry::{
    RegCloseKey, RegCreateKeyExW, RegDeleteTreeW, RegEnumKeyExW, RegGetValueW, RegOpenKeyExW, HKEY,
    HKEY_CURRENT_USER, KEY_READ, KEY_WRITE, REG_OPTION_NON_VOLATILE, RRF_RT_REG_SZ,
};
use CamStudioDShow::registration::{
    category_instance_key, clsid_key, delete_registration, unregister_roots, write_registration,
    CLSID_STRING,
};

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(Some(0)).collect()
}

fn read_sz(root: HKEY, key: &str, name: Option<&str>) -> Option<String> {
    let k = wide(key);
    let n = name.map(wide);
    let mut bytes = 0u32;
    // SAFETY: NUL-terminated strings; the size query and the read use valid out pointers.
    unsafe {
        let name_p = n.as_ref().map_or(PCWSTR::null(), |n| PCWSTR(n.as_ptr()));
        if RegGetValueW(
            root,
            PCWSTR(k.as_ptr()),
            name_p,
            RRF_RT_REG_SZ,
            None,
            None,
            Some(&mut bytes),
        ) != ERROR_SUCCESS
        {
            return None;
        }
        let mut buf = vec![0u16; bytes as usize / 2 + 1];
        if RegGetValueW(
            root,
            PCWSTR(k.as_ptr()),
            name_p,
            RRF_RT_REG_SZ,
            None,
            Some(buf.as_mut_ptr() as *mut _),
            Some(&mut bytes),
        ) != ERROR_SUCCESS
        {
            return None;
        }
        let len = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
        Some(String::from_utf16_lossy(&buf[..len]))
    }
}

fn subkeys(root: HKEY, key: &str) -> Vec<String> {
    let k = wide(key);
    let mut out = Vec::new();
    // SAFETY: NUL-terminated key name; the enumeration buffer is valid for its stated length.
    unsafe {
        let mut h = HKEY::default();
        if RegOpenKeyExW(root, PCWSTR(k.as_ptr()), None, KEY_READ, &mut h) != ERROR_SUCCESS {
            return out;
        }
        let mut i = 0;
        loop {
            let mut name = [0u16; 256];
            let mut len = name.len() as u32;
            if RegEnumKeyExW(
                h,
                i,
                Some(windows::core::PWSTR(name.as_mut_ptr())),
                &mut len,
                None,
                None,
                None,
                None,
            ) != ERROR_SUCCESS
            {
                break;
            }
            out.push(String::from_utf16_lossy(&name[..len as usize]));
            i += 1;
        }
        let _ = RegCloseKey(h);
    }
    out
}

#[test]
fn registration_matches_the_cpp_keys_and_values() {
    // A private scratch root: nothing here is the real CLSID or category key.
    let scratch = format!(r"Software\ZZ-niwcam-dshow-test-{}", std::process::id());
    let s = wide(&scratch);
    let mut root = HKEY::default();
    // SAFETY: NUL-terminated name, valid out pointer.
    let st = unsafe {
        RegCreateKeyExW(
            HKEY_CURRENT_USER,
            PCWSTR(s.as_ptr()),
            None,
            PCWSTR::null(),
            REG_OPTION_NON_VOLATILE,
            KEY_WRITE | KEY_READ,
            None,
            &mut root,
            None,
        )
    };
    assert_eq!(st, ERROR_SUCCESS);
    // Delete the scratch key even when an assertion fails.
    struct Cleanup(Vec<u16>, HKEY);
    impl Drop for Cleanup {
        fn drop(&mut self) {
            // SAFETY: the key was opened above; the name is NUL-terminated.
            unsafe {
                let _ = RegCloseKey(self.1);
                let _ = RegDeleteTreeW(HKEY_CURRENT_USER, PCWSTR(self.0.as_ptr()));
            }
        }
    }
    let _cleanup = Cleanup(s.clone(), root);

    assert_eq!(
        write_registration(root, r"C:\Test\CamStudioDShow.dll"),
        S_OK
    );

    // dllmain.cpp: CLSID key (default value), InprocServer32 (default = DLL path, ThreadingModel = Both)
    let clsid = clsid_key();
    assert_eq!(clsid, format!(r"Software\Classes\CLSID\{CLSID_STRING}"));
    assert_eq!(CLSID_STRING, "{7D2B9C41-3A6E-4F58-B0D1-9E4C2F6A8B13}");
    assert_eq!(
        read_sz(root, &clsid, None).as_deref(),
        Some("NiwCam DirectShow Camera")
    );
    let inproc = format!(r"{clsid}\InprocServer32");
    assert_eq!(
        read_sz(root, &inproc, None).as_deref(),
        Some(r"C:\Test\CamStudioDShow.dll")
    );
    assert_eq!(
        read_sz(root, &inproc, Some("ThreadingModel")).as_deref(),
        Some("Both")
    );
    // the capture-device entry apps enumerate: CLSID + FriendlyName under the video-input category
    let inst = category_instance_key();
    assert_eq!(
        inst,
        format!(
            r"Software\Classes\CLSID\{{860BB310-5D01-11d0-BD3B-00A0C911CE86}}\Instance\{CLSID_STRING}"
        )
    );
    assert_eq!(
        read_sz(root, &inst, Some("CLSID")).as_deref(),
        Some(CLSID_STRING)
    );
    assert_eq!(
        read_sz(root, &inst, Some("FriendlyName")).as_deref(),
        Some("NiwCam")
    );
    // nothing else was written
    let mut under_clsid = subkeys(root, r"Software\Classes\CLSID");
    under_clsid.sort();
    assert_eq!(
        under_clsid,
        vec![
            CLSID_STRING.to_string(),
            "{860BB310-5D01-11d0-BD3B-00A0C911CE86}".to_string()
        ]
    );
    assert_eq!(subkeys(root, &clsid), vec!["InprocServer32".to_string()]);

    // DllUnregisterServer's deletion under one root
    delete_registration(root);
    assert!(read_sz(root, &clsid, None).is_none());
    assert!(read_sz(root, &inst, Some("FriendlyName")).is_none());
}

#[test]
fn unregister_cleans_hklm_only_when_elevated() {
    use windows::Win32::System::Registry::HKEY_LOCAL_MACHINE;
    assert_eq!(unregister_roots(false), vec![HKEY_CURRENT_USER]);
    assert_eq!(
        unregister_roots(true),
        vec![HKEY_LOCAL_MACHINE, HKEY_CURRENT_USER]
    );
}
