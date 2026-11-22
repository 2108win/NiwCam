//! Rust twin of `tools/usb_check.cpp` (niwcam-usb-check): same subcommands, same output.
//!
//!   usb_check list                       phones on a cable, from adb and usbmux
//!   usb_check watch [seconds]            the background watcher's list each time it changes
//!   usb_check hello <serial|udid> [port] open a tunnel to the app and exchange "hello"
//!   usb_check probe <serial|udid> [port] [close|silent|junk]
//!
//! Like the C++ tool it never starts or stops an adb server (the watcher gets no starter), and
//! `hello` is a real connection: do not run it against a phone that is streaming to a PC.
use std::io::Write as _;
use std::time::{Duration, Instant};

use niwcam_core::json::Value;
use niwcam_phone::sock::{self, Socket, NO_SOCKET};
use niwcam_phone::usb_transport as usb;

fn kind_name(k: usb::Kind) -> &'static str {
    if k == usb::Kind::Adb {
        "adb"
    } else {
        "usbmux"
    }
}

fn print(devices: &[usb::Device]) {
    if devices.is_empty() {
        println!("  (none)");
    }
    for d in devices {
        println!(
            "  {:<6} {:<28} {:<12} name=\"{}\" model={}{}",
            kind_name(d.kind),
            d.serial,
            d.state,
            d.name,
            d.model,
            if d.kind == usb::Kind::Usbmux {
                format!(" muxId={}", d.mux_id)
            } else {
                String::new()
            }
        );
    }
}

fn list() -> i32 {
    println!("adb:");
    match usb::list_adb() {
        Ok(mut adb) => {
            for d in &mut adb {
                if d.state == "device" {
                    d.name = usb::adb_device_name(&d.serial);
                }
            }
            print(&adb);
        }
        Err(e) => println!("  {e}"),
    }
    println!("usbmux:");
    match usb::list_usbmux() {
        Ok(mut mux) => {
            for d in &mut mux {
                let mut model = std::mem::take(&mut d.model);
                d.name = usb::iphone_name(d.mux_id, Some(&mut model));
                d.model = model;
            }
            print(&mux);
        }
        Err(e) => println!("  {e}"),
    }
    0
}

fn watch(seconds: u64) -> i32 {
    let watcher = usb::Watcher::new();
    watcher.start(None, None);
    let mut last = "?".to_string();
    let end = Instant::now() + Duration::from_secs(seconds);
    while Instant::now() < end {
        let devices = watcher.devices();
        let mut now = String::new();
        for d in &devices {
            now.push_str(&format!("{}/{}/{};", d.serial, d.state, d.name));
        }
        if now != last {
            println!(
                "[{}]",
                if now.is_empty() {
                    "no phones"
                } else {
                    "phones"
                }
            );
            print(&devices);
            let _ = std::io::stdout().flush();
            last = now;
        }
        std::thread::sleep(Duration::from_millis(200));
    }
    watcher.stop();
    println!("stopped");
    0
}

fn recv_exact_blocking(s: Socket, dst: &mut [u8]) -> bool {
    let mut got = 0;
    while got < dst.len() {
        let n = sock::recv(s, &mut dst[got..]);
        if n <= 0 {
            return false;
        }
        got += n as usize;
    }
    true
}

/// A tunnel to the app's port on the phone with this adb serial or UDID, or `NO_SOCKET`.
fn open_tunnel(id: &str, port: u16) -> Socket {
    let adb = usb::list_adb().unwrap_or_default();
    let mux = usb::list_usbmux().unwrap_or_default();
    let mut s = NO_SOCKET;
    let mut found = false;
    let mut error = String::new();
    for d in &adb {
        if d.serial == id {
            found = true;
            s = usb::open_adb(id, port, &mut error);
        }
    }
    for d in &mux {
        if d.serial == id {
            found = true;
            s = usb::open_usbmux(id, port, &mut error);
        }
    }
    if !found {
        println!("no phone {id} on a cable");
    } else if s == NO_SOCKET {
        println!("tunnel failed: {error}");
    } else {
        println!("tunnel to port {port} open");
    }
    let _ = std::io::stdout().flush();
    s
}

fn probe(id: &str, port: u16, mode: &str) -> i32 {
    let s = open_tunnel(id, port);
    if s == NO_SOCKET {
        return 1;
    }
    if mode == "close" {
        usb::close(s);
        println!("closed without a hello");
        return 0;
    }
    // "junk": the first 4 bytes read as a record length far above the handshake limit.
    if mode == "junk" && !sock::send_all(s, b"GET / HTTP/1.1\r\nHost: phone\r\n\r\n") {
        println!("send failed");
    }
    let start = Instant::now();
    sock::set_recv_timeout(s, 30_000);
    let mut byte = [0u8; 1];
    let n = sock::recv(s, &mut byte);
    let seconds = start.elapsed().as_secs_f64();
    usb::close(s);
    if n > 0 {
        println!("the phone sent data: it took this for a PC");
    } else if seconds >= 29.0 {
        println!("still open after {seconds:.0} s");
    } else {
        println!("the phone closed it after {seconds:.1} s");
    }
    i32::from(n > 0)
}

fn hello(id: &str, port: u16) -> i32 {
    let s = open_tunnel(id, port);
    if s == NO_SOCKET {
        return 1;
    }
    // A suspended app (phone locked, app in the background) may leave the connection unanswered.
    sock::set_recv_timeout(s, 10_000);

    // HANDSHAKE record: u32 length, type 0x01, JSON. The commitment is never checked: no "reveal" follows.
    let mut v = Value::object();
    v.set("type", "hello")
        .set("v", 1)
        .set("clientId", "usb-check")
        .set("clientName", "usb-check")
        .set("commit", "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=");
    let body = v.dump();
    let n = body.len() as u32 + 1;
    let mut record = n.to_be_bytes().to_vec();
    record.push(0x01);
    record.extend_from_slice(body.as_bytes());
    let mut head = [0u8; 5];
    if !sock::send_all(s, &record) || !recv_exact_blocking(s, &mut head) {
        println!("no reply from the app in 10 s (phone locked, or the app in the background?)");
        usb::close(s);
        return 1;
    }
    let length = u32::from_be_bytes([head[0], head[1], head[2], head[3]]);
    if head[4] != 0x01 || !(2..=65536).contains(&length) {
        println!("not a HANDSHAKE record");
        usb::close(s);
        return 1;
    }
    let mut reply = vec![0u8; (length - 1) as usize];
    let ok = recv_exact_blocking(s, &mut reply);
    usb::close(s);
    if !ok {
        println!("reply cut short");
        return 1;
    }
    let v = Value::parse_bytes(&reply).unwrap_or_default();
    println!(
        "hello: deviceId={} name=\"{}\" model=\"{}\" os={} paired={}",
        v.get("deviceId").str(),
        v.get("name").str(),
        v.get("model").str(),
        v.get("os").str(),
        if v.get("paired").bool_or(false) {
            "true"
        } else {
            "false"
        }
    );
    0
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let cmd = args.get(1).map_or("list", String::as_str);
    let port: u16 = args
        .get(3)
        .map_or(usb::CAMERA_PORT, |p| p.parse().unwrap_or(0));
    let code = match cmd {
        "list" => list(),
        "watch" => watch(args.get(2).map_or(30, |s| s.parse().unwrap_or(0))),
        "hello" if args.len() > 2 => hello(&args[2], port),
        "probe" if args.len() > 2 => {
            probe(&args[2], port, args.get(4).map_or("close", String::as_str))
        }
        _ => {
            eprintln!("usage: usb_check list | watch [seconds] | hello <serial|udid> [port] | probe <serial|udid> [port] [close|silent|junk]");
            2
        }
    };
    std::process::exit(code);
}
