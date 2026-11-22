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
