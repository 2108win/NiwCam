//! Browses `_camstudio._tcp` with the port's `discovery` module and prints the phones it sees, in the
//! format of `xcheck/cpp_discovery.cpp`.
//!
//!   discovery_browse browse <seconds>
//!
//! Neither the USB watcher nor the adb starter is run (`start_discovery_with(false, false)`): this tool
//! only does the mDNS part, so nothing on the PC is started.
use std::time::Duration;

use niwcam_phone::discovery::{known_phones, start_discovery_with, stop_discovery};

fn main() {
    let seconds: u64 = std::env::args()
        .nth(2)
        .and_then(|s| s.parse().ok())
        .unwrap_or(5);
    // Only network entries are looked at below, the manual list needs a data folder.
    if std::env::var_os("CAMSTUDIO_DATA_DIR").is_none() {
        std::env::set_var(
            "CAMSTUDIO_DATA_DIR",
            std::env::temp_dir().join("niwcam-discovery-browse"),
        );
    }
    // `usb` as the third argument also runs the cable watcher (read-only: it lists phones through the adb
    // server and usbmuxd that already run; the adb starter is still off) and prints the cable entries.
    let usb = std::env::args().nth(3).as_deref() == Some("usb");
    start_discovery_with(usb, false);
    println!("DnsServiceBrowse started (pending: browsing)");
    std::thread::sleep(Duration::from_secs(seconds));
    let all = known_phones();
    let found: Vec<_> = all.iter().filter(|e| !e.manual && !e.usb()).collect();
    println!("found {} phone(s)", found.len());
    stop_discovery();
    for e in &found {
        println!(
            "  {}:{} name=\"{}\" id={} model={} os={} screen={}",
            e.host,
            e.port,
            e.name,
            e.device_id,
            e.model,
            e.os,
            i32::from(e.screen)
        );
    }
    if usb {
        println!("cable entries:");
        for e in all.iter().filter(|e| e.usb()) {
            println!(
                "  {:?} serial={} port={} name=\"{}\" os={} model={} screen={} ready={}",
                e.transport,
                e.serial,
                e.port,
                e.name,
                e.os,
                e.model,
                i32::from(e.screen),
                i32::from(e.ready)
            );
        }
    }
}
