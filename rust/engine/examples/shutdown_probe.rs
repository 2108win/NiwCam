//! Loads a `NiwCamEngine.dll`, starts it (mDNS browse and the rest), waits, shuts it down and exits, to find out
//! whether the process dies at exit (it did: 0xC0000005 inside dnsapi.dll, Rust engine only, 2026-11-22).
//!
//!   shutdown_probe --dll <path> [--wait-ms 3000] [--no-shutdown] [--delay-ms 0]
//! Prints the steps; the exit code is what matters.

use std::time::Duration;

use windows::core::{PCSTR, PCWSTR};
use windows::Win32::System::LibraryLoader::{GetProcAddress, LoadLibraryW};

fn arg(name: &str) -> Option<String> {
    let a: Vec<String> = std::env::args().collect();
    a.iter()
        .position(|x| x == name)
        .and_then(|i| a.get(i + 1).cloned())
}

fn main() {
    let dll = arg("--dll").expect("--dll");
    let wait: u64 = arg("--wait-ms").and_then(|s| s.parse().ok()).unwrap_or(3000);
    let delay: i32 = arg("--delay-ms").and_then(|s| s.parse().ok()).unwrap_or(0);
    let wide: Vec<u16> = dll.encode_utf16().chain([0]).collect();
    // SAFETY: NUL-terminated path; exports are looked up by exact name and signature.
    unsafe {
        let m = LoadLibraryW(PCWSTR(wide.as_ptr())).expect("load dll");
        let f = |n: &str| {
            let c = std::ffi::CString::new(n).unwrap();
            GetProcAddress(m, PCSTR(c.as_ptr() as *const u8)).unwrap_or_else(|| panic!("missing {n}"))
        };
        let init: unsafe extern "C" fn(i32) -> i32 = std::mem::transmute(f("cs_init_delayed"));
        let shutdown: unsafe extern "C" fn() = std::mem::transmute(f("cs_shutdown"));
        println!("init={}", init(delay));
        std::thread::sleep(Duration::from_millis(wait));
        if std::env::args().any(|a| a == "--no-shutdown") {
            println!("skipping shutdown");
        } else {
            println!("shutdown...");
            shutdown();
            println!("shutdown done");
        }
    }
    println!("main returns");
}
