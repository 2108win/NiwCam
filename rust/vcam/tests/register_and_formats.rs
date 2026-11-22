//! DllRegisterServer / DllUnregisterServer (into a test key under HKCU, never HKLM) and the
//! format list. Needs `--features test-names`; the media-source behaviour is checked by
//! `xcheck/run.ps1` against the C++ DLL.
#![cfg(feature = "test-names")]
#![allow(non_snake_case)]

use windows::core::{w, PCWSTR};
use windows::Win32::Foundation::{ERROR_SUCCESS, S_FALSE, S_OK};
use windows::Win32::System::Registry::{
    RegDeleteTreeW, RegGetValueW, HKEY_CURRENT_USER, RRF_RT_REG_SZ,
};
use CamStudioVCam::{
    build_formats, DllCanUnloadNow, DllRegisterServer, DllUnregisterServer, VideoFormat,
};

fn read_sz(subkey: &str, value: PCWSTR) -> Option<String> {
    let key: Vec<u16> = subkey.encode_utf16().chain(Some(0)).collect();
    let mut buf = vec![0u16; 1024];
    let mut bytes = (buf.len() * 2) as u32;
    // SAFETY: valid NUL-terminated key, buffer of the stated size.
    let st = unsafe {
        RegGetValueW(
            HKEY_CURRENT_USER,
            PCWSTR(key.as_ptr()),
            value,
            RRF_RT_REG_SZ,
            None,
            Some(buf.as_mut_ptr() as *mut _),
            Some(&mut bytes),
        )
    };
    if st != ERROR_SUCCESS {
        return None;
    }
    let n = (bytes as usize / 2).saturating_sub(1);
    Some(String::from_utf16_lossy(&buf[..n]))
}

#[test]
fn register_writes_the_class_key_and_unregister_removes_it() {
    let suffix = format!("reg_{}", std::process::id());
    // This test is the only one touching the variable.
    std::env::set_var("NIWCAM_TEST_SUFFIX", &suffix);
    let base = format!(r"Software\NiwCamTest_{suffix}");
    let clsid = format!(r"{base}\CLSID\{{72F5B2FD-C48E-449B-95CE-E8F13BF200CB}}");

    assert_eq!(DllCanUnloadNow(), S_FALSE);
    assert_eq!(DllRegisterServer(), S_OK);
    assert_eq!(
        read_sz(&clsid, PCWSTR::null()).as_deref(),
        Some("NiwCam Virtual Camera")
    );
    let exe = std::env::current_exe()
        .unwrap()
        .to_string_lossy()
        .to_string();
    let inproc = format!(r"{clsid}\InprocServer32");
    assert_eq!(
        read_sz(&inproc, PCWSTR::null()).as_deref(),
        Some(exe.as_str())
    );
    assert_eq!(
        read_sz(&inproc, w!("ThreadingModel")).as_deref(),
        Some("Both")
    );
    // Registering again is fine (same keys).
    assert_eq!(DllRegisterServer(), S_OK);

    assert_eq!(DllUnregisterServer(), S_OK);
    assert_eq!(read_sz(&clsid, PCWSTR::null()), None);
    // Nothing left to delete: still S_OK (ERROR_FILE_NOT_FOUND is accepted).
    assert_eq!(DllUnregisterServer(), S_OK);

    let b: Vec<u16> = base.encode_utf16().chain(Some(0)).collect();
    // SAFETY: valid NUL-terminated key of our own test hive.
    unsafe {
        let _ = RegDeleteTreeW(HKEY_CURRENT_USER, PCWSTR(b.as_ptr()));
    }
}

fn f(width: u32, height: u32, fps_num: u32, fps_den: u32) -> VideoFormat {
    VideoFormat {
        width,
        height,
        fps_num,
        fps_den,
    }
}

#[test]
fn format_lists() {
    // 1080p30: native first, then the common heights below it; no extra 30 fps entries.
    assert_eq!(
        build_formats(1920, 1080, 30, 1),
        vec![
            f(1920, 1080, 30, 1),
            f(1280, 720, 30, 1),
            f(960, 540, 30, 1),
            f(854, 480, 30, 1),
            f(640, 360, 30, 1)
        ]
    );
    // A native rate above 31 fps adds a 30 fps entry after every size.
    assert_eq!(
        build_formats(1280, 720, 60, 1),
        vec![
            f(1280, 720, 60, 1),
            f(1280, 720, 30, 1),
            f(960, 540, 60, 1),
            f(960, 540, 30, 1),
            f(854, 480, 60, 1),
            f(854, 480, 30, 1),
            f(640, 360, 60, 1),
            f(640, 360, 30, 1),
        ]
    );
    // 31 fps and 30000/1001 do not add one; 32 does.
    assert_eq!(build_formats(640, 360, 31, 1).len(), 1);
    assert_eq!(build_formats(640, 360, 30000, 1001).len(), 1);
    assert_eq!(build_formats(640, 360, 32, 1).len(), 2);
    // Too narrow targets are skipped (< 160 px wide); a zero denominator adds nothing.
    assert_eq!(build_formats(160, 2000, 30, 1)[0], f(160, 2000, 30, 1));
    assert_eq!(build_formats(640, 360, 60, 0).len(), 1);
}
