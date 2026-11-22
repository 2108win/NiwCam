//! The folder where NiwCam keeps its files (port of `src/common/AppDataDir.h`).

use std::path::PathBuf;

/// What is appended to `%LOCALAPPDATA%` when `CAMSTUDIO_DATA_DIR` is not set.
///
/// C++ quirk, copied on purpose (rust/AGENTS.md: copy the behaviour, note it, do not fix it):
/// `AppDataDir.h` spells the default `L"\NiwCam"` with a single backslash, which MSVC reads as the
/// unknown escape `\N`, i.e. just `N` (warning C4129). So without `CAMSTUDIO_DATA_DIR` a C++ tool uses
/// `%LOCALAPPDATA%NiwCam` (no separator, e.g. `C:\Users\X\AppData\LocalNiwCam`), not the
/// `%LOCALAPPDATA%\NiwCam` the notes mention. The app always sets the variable, so that folder never
/// shows up on a real PC. The fix is `L"\\NiwCam"` in `src/common/AppDataDir.h`; make it on both sides
/// at once (and change this constant to `"\\NiwCam"`), otherwise C++ and Rust tools disagree.
pub const DEFAULT_SUFFIX: &str = "NiwCam";

/// `CAMSTUDIO_DATA_DIR` when set (and shorter than MAX_PATH, like the C++ `GetEnvironmentVariableW`
/// with a MAX_PATH buffer), else `%LOCALAPPDATA%` + [`DEFAULT_SUFFIX`].
pub fn app_data_dir() -> PathBuf {
    use std::os::windows::ffi::OsStrExt;
    if let Some(env) = std::env::var_os("CAMSTUDIO_DATA_DIR") {
        let n = env.encode_wide().count();
        if n > 0 && n < 260 {
            return PathBuf::from(env);
        }
    }
    let mut local = std::env::var_os("LOCALAPPDATA").unwrap_or_default();
    local.push(DEFAULT_SUFFIX);
    PathBuf::from(local)
}
