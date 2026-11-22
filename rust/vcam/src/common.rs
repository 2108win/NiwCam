//! Port of `src/vcam/Common.h`: logging, error plumbing, the CLSID and attribute GUIDs, the kernel
//! object names the reader uses.

use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::{Mutex, MutexGuard};

use niwcam_shm::{
    GuidValues, Names, CAMSTUDIO_ATTR_NATIVE_FPS, CAMSTUDIO_ATTR_NATIVE_SIZE, CLSID_CAMSTUDIO_VCAM,
};
use windows::core::{Error, GUID, HRESULT};
use windows::Win32::Foundation::E_FAIL;
use windows::Win32::System::Diagnostics::Debug::OutputDebugStringW;

pub const fn guid(v: GuidValues) -> GUID {
    GUID::from_values(v.0, v.1, v.2, v.3)
}

pub const CLSID_CAM_STUDIO_VCAM: GUID = guid(CLSID_CAMSTUDIO_VCAM);
pub const ATTR_NATIVE_SIZE: GUID = guid(CAMSTUDIO_ATTR_NATIVE_SIZE);
pub const ATTR_NATIVE_FPS: GUID = guid(CAMSTUDIO_ATTR_NATIVE_FPS);

/// `VcamLog`: Frame Server runs us as LocalService in session 0, so file logging is not possible.
/// Read these with DebugView (enable "Capture Global Win32"). The C++ buffer is 512 wide chars.
pub fn vcam_log(msg: &str) {
    let text = format!("[CamStudioVCam] {msg}\n");
    let mut wide: Vec<u16> = text.encode_utf16().take(511).collect();
    wide.push(0);
    // SAFETY: `wide` is NUL-terminated and outlives the call.
    unsafe { OutputDebugStringW(windows::core::PCWSTR(wide.as_ptr())) };
    #[cfg(feature = "diag-file")]
    {
        // Diagnostic builds only: Frame Server has no debugger attached.
        use std::io::Write;
        if let Ok(mut f) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(r"C:\ProgramData\NiwCam-vcam-diag.log")
        {
            let _ = f.write_all(text.as_bytes());
        }
    }
}

macro_rules! vlog {
    ($($t:tt)*) => { $crate::common::vcam_log(&format!($($t)*)) };
}
pub(crate) use vlog;

/// `RETURN_IF_FAILED`: logs where and what failed, then returns the error.
macro_rules! check {
    ($e:expr) => {
        match $e {
            Ok(v) => v,
            Err(err) => {
                $crate::common::vlog!(
                    "{}:{} 0x{:08X} {}",
                    file!(),
                    line!(),
                    err.code().0 as u32,
                    stringify!($e)
                );
                return Err(err);
            }
        }
    };
}
pub(crate) use check;

/// A panic must never cross the COM boundary (it would take down Frame Server): it becomes E_FAIL.
pub fn guard<T>(f: impl FnOnce() -> windows::core::Result<T>) -> windows::core::Result<T> {
    match catch_unwind(AssertUnwindSafe(f)) {
        Ok(r) => r,
        Err(_) => {
            vlog!("panic caught at a COM boundary");
            Err(Error::from_hresult(E_FAIL))
        }
    }
}

/// The same for a call that answers with a bare HRESULT (the attribute delegations).
pub fn guard_hr(f: impl FnOnce() -> HRESULT) -> HRESULT {
    match catch_unwind(AssertUnwindSafe(f)) {
        Ok(h) => h,
        Err(_) => {
            vlog!("panic caught at a COM boundary");
            E_FAIL
        }
    }
}

/// Runs `f` and swallows a panic (for destructors, which run inside `IUnknown::Release`).
pub fn guard_drop(f: impl FnOnce()) {
    if catch_unwind(AssertUnwindSafe(f)).is_err() {
        vlog!("panic caught in a destructor");
    }
}

/// Locks ignoring poisoning (a panic in one call must not wedge every later call).
pub fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// The kernel-object names of the shared-memory channel: the real ones, except in test builds
/// (feature `test-names`) where `NIWCAM_TEST_SUFFIX` makes them unique and session-local.
pub fn channel_names() -> Names {
    #[cfg(feature = "test-names")]
    {
        Names::for_test(&test_suffix())
    }
    #[cfg(not(feature = "test-names"))]
    {
        Names::default()
    }
}

/// `NIWCAM_TEST_SUFFIX`, or a per-process fallback so a test build can never collide with the
/// real objects of the running app.
#[cfg(feature = "test-names")]
pub fn test_suffix() -> String {
    std::env::var("NIWCAM_TEST_SUFFIX").unwrap_or_else(|_| format!("noenv_{}", std::process::id()))
}

/// Diagnostic builds only: who is calling (process/thread token, privileges), to see why a
/// `Global\` object cannot be created inside Frame Server.
#[cfg(feature = "diag-file")]
pub fn diag_token() {
    use windows::Win32::Foundation::{CloseHandle, HANDLE, LUID};
    use windows::Win32::Security::{
        GetTokenInformation, LookupPrivilegeNameW, TokenPrivileges, TOKEN_PRIVILEGES, TOKEN_QUERY,
    };
    use windows::Win32::System::Threading::{
        GetCurrentProcess, GetCurrentThread, OpenProcessToken, OpenThreadToken,
    };
    // SAFETY: plain token queries on this thread/process with local buffers.
    unsafe {
        let mut th = HANDLE::default();
        let impersonating = OpenThreadToken(GetCurrentThread(), TOKEN_QUERY, true, &mut th).is_ok();
        vcam_log(&format!("thread impersonating: {impersonating}"));
        let mut tokens = vec![("process", HANDLE::default())];
        if impersonating {
            tokens.push(("thread", th));
        }
        if OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut tokens[0].1).is_err() {
            vcam_log("OpenProcessToken failed");
            return;
        }
        for (what, h) in tokens {
            let mut len = 0u32;
            let _ = GetTokenInformation(h, TokenPrivileges, None, 0, &mut len);
            let mut buf = vec![0u8; len as usize];
            if GetTokenInformation(
                h,
                TokenPrivileges,
                Some(buf.as_mut_ptr().cast()),
                len,
                &mut len,
            )
            .is_ok()
            {
                let tp = &*(buf.as_ptr() as *const TOKEN_PRIVILEGES);
                let items =
                    std::slice::from_raw_parts(tp.Privileges.as_ptr(), tp.PrivilegeCount as usize);
                for p in items {
                    let mut name = [0u16; 64];
                    let mut n = 64u32;
                    let luid: LUID = p.Luid;
                    let _ = LookupPrivilegeNameW(
                        None,
                        &luid,
                        Some(windows::core::PWSTR(name.as_mut_ptr())),
                        &mut n,
                    );
                    vcam_log(&format!(
                        "{what} privilege {} attrs 0x{:X}",
                        String::from_utf16_lossy(&name[..n as usize]),
                        p.Attributes.0
                    ));
                }
            }
            let _ = CloseHandle(h);
        }
    }
}

/// Diagnostic builds only: which kernel-object creations work in this process, and where it runs.
#[cfg(feature = "diag-file")]
pub fn diag_objects() {
    use windows::core::{w, PCWSTR};
    use windows::Win32::Foundation::{CloseHandle, LocalFree, HLOCAL, INVALID_HANDLE_VALUE};
    use windows::Win32::Security::Authorization::{
        ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1,
    };
    use windows::Win32::Security::{PSECURITY_DESCRIPTOR, SECURITY_ATTRIBUTES};
    use windows::Win32::System::LibraryLoader::GetModuleFileNameW;
    use windows::Win32::System::Memory::{CreateFileMappingW, PAGE_READWRITE};
    use windows::Win32::System::RemoteDesktop::ProcessIdToSessionId;
    use windows::Win32::System::Threading::{CreateEventW, GetCurrentProcessId};
    // SAFETY: local buffers and handles that are closed right away.
    unsafe {
        let mut name = [0u16; 260];
        let n = GetModuleFileNameW(None, &mut name) as usize;
        let mut session = 0u32;
        let _ = ProcessIdToSessionId(GetCurrentProcessId(), &mut session);
        vcam_log(&format!(
            "host exe: {} (session {session})",
            String::from_utf16_lossy(&name[..n])
        ));
        let mut sd = PSECURITY_DESCRIPTOR::default();
        let sddl: Vec<u16> = "D:(A;;GA;;;SY)(A;;GA;;;LS)(A;;GA;;;BA)(A;;GRGW;;;IU)(A;;GRGW;;;AU)\0"
            .encode_utf16()
            .collect();
        let _ = ConvertStringSecurityDescriptorToSecurityDescriptorW(
            PCWSTR(sddl.as_ptr()),
            SDDL_REVISION_1,
            &mut sd,
            None,
        );
        let sa = SECURITY_ATTRIBUTES {
            nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
            lpSecurityDescriptor: sd.0,
            bInheritHandle: false.into(),
        };
        let names = [
            w!(r"Global\NiwCamDiagA"),
            w!(r"Local\NiwCamDiagB"),
            w!("NiwCamDiagC"),
        ];
        for name in names {
            for (what, psa) in [
                ("no sd", None),
                ("sd", Some(&sa as *const SECURITY_ATTRIBUTES)),
            ] {
                let m =
                    CreateFileMappingW(INVALID_HANDLE_VALUE, psa, PAGE_READWRITE, 0, 4096, name);
                vcam_log(&format!(
                    "mapping {} [{what}]: {:?}",
                    name.display(),
                    m.as_ref()
                        .map(|_| "ok")
                        .map_err(|e| format!("0x{:08X}", e.code().0 as u32))
                ));
                if let Ok(h) = m {
                    let _ = CloseHandle(h);
                }
                let e = CreateEventW(psa, false, false, name);
                vcam_log(&format!(
                    "event   {} [{what}]: {:?}",
                    name.display(),
                    e.as_ref()
                        .map(|_| "ok")
                        .map_err(|e| format!("0x{:08X}", e.code().0 as u32))
                ));
                if let Ok(h) = e {
                    let _ = CloseHandle(h);
                }
            }
        }
        if !sd.0.is_null() {
            let _ = LocalFree(Some(HLOCAL(sd.0)));
        }
    }
}
