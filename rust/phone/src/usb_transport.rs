//! Phones on a USB cable (docs/protocol.md 1.2). Port of `src/engine/UsbTransport.{h,cpp}`.
//!
//! The protocol does not change: the platform's own USB service opens a TCP tunnel to the phone
//! app's port, and `PhoneLink` runs over that socket exactly as over Wi-Fi.
//!   Android: the adb server (127.0.0.1:5037): "host:transport:<serial>", then "tcp:<port>".
//!   iPhone:  usbmuxd: "Connect", on Windows the Apple Mobile Device Service (127.0.0.1:27015).
//!
//! Rules kept from the C++ (CLAUDE.md "USB cable"): the engine never runs `adb.exe` while a server
//! runs; it stops only a server it started itself; the lists come from background watchers.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

use niwcam_core::json::Value;

use crate::sock::{self, Socket, NO_SOCKET};
use crate::util::{strtod_prefix, strtoll10, strtou, tick_ms, LockExt};

pub use crate::sock::{Socket as UsbSocket, NO_SOCKET as NO_USB_SOCKET};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Adb,
    Usbmux,
}

/// The phone app's fixed ports (docs/protocol.md 1, 1.1), the only ones reachable over a cable.
pub const CAMERA_PORT: u16 = 47321;
/// Only while the phone shares its screen.
pub const SCREEN_PORT: u16 = 47322;

const ADB_PORT: u16 = 5037;
const LOCKDOWN_PORT: u16 = 62078;
const MAX_PLIST: u32 = 1024 * 1024;
/// Apple Mobile Device Service.
const USBMUX_PORT: u16 = 27015;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Device {
    pub kind: Kind,
    /// adb serial, or the iPhone's UDID
    pub serial: String,
    /// adb: "device", "unauthorized", "authorizing"; iPhones: "device"
    pub state: String,
    /// the name the phone app also shows (Android device_name, iPhone name); may be empty
    pub name: String,
    /// adb "model:", iPhone ProductType ("iPhone14,7")
    pub model: String,
    /// usbmux DeviceID: a new one each time the cable is plugged in
    pub mux_id: u32,
}

impl Default for Device {
    fn default() -> Self {
        Device {
            kind: Kind::Adb,
            serial: String::new(),
            state: String::new(),
            name: String::new(),
            model: String::new(),
            mux_id: 0,
        }
    }
}

// ---- helpers ---------------------------------------------------------------------------------

fn connect_loopback(port: u16) -> Socket {
    sock::connect_loopback(port)
}

fn connect_usbmuxd() -> Socket {
    connect_loopback(USBMUX_PORT)
}

fn contains(s: &str, what: &str) -> bool {
    s.contains(what)
}

fn put_le32(v: u32) -> [u8; 4] {
    v.to_le_bytes()
}

fn get_le32(p: &[u8]) -> u32 {
    u32::from_le_bytes([p[0], p[1], p[2], p[3]])
}

pub fn close(s: Socket) {
    sock::close(s);
}

// ---- adb (the adb server's "smart socket" protocol) --------------------------------------------

/// A block: 4 hex digits of length, then that many bytes.
fn adb_read_block(s: Socket, out: &mut String, timeout_ms: i32) -> bool {
    let mut hex = [0u8; 4];
    if !sock::recv_all(s, &mut hex, timeout_ms) {
        return false;
    }
    let (n, used) = strtou(&hex, 16);
    if used != 4 {
        return false;
    }
    let mut buf = vec![0u8; n as usize];
    if n != 0 && !sock::recv_all(s, &mut buf, timeout_ms) {
        out.clear();
        return false;
    }
    *out = String::from_utf8_lossy(&buf).into_owned();
    true
}

/// Sends one request; the server answers OKAY, or FAIL and a block with the reason.
fn adb_send(s: Socket, request: &str, failure: &mut String, timeout_ms: i32) -> bool {
    let head = format!("{:04x}", request.len());
    let mut status = [0u8; 4];
    if !sock::send_all(s, head.as_bytes())
        || !sock::send_all(s, request.as_bytes())
        || !sock::recv_all(s, &mut status, timeout_ms)
    {
        *failure = "closed".to_string();
        return false;
    }
    if &status == b"OKAY" {
        return true;
    }
    if &status != b"FAIL" || !adb_read_block(s, failure, timeout_ms) {
        *failure = "bad reply".to_string();
    }
    false
}

/// "host:transport:<serial>" on a fresh connection: the socket then talks to that phone's adbd.
fn adb_transport(serial: &str, failure: &mut String) -> Socket {
    let s = connect_loopback(ADB_PORT);
    if s == NO_SOCKET {
        *failure = "no adb server".to_string();
        return NO_SOCKET;
    }
    if !adb_send(s, &format!("host:transport:{serial}"), failure, 5000) {
        close(s);
        return NO_SOCKET;
    }
    s
}

fn adb_shell(serial: &str, command: &str) -> String {
    let mut failure = String::new();
    let mut out: Vec<u8> = Vec::new();
    let s = adb_transport(serial, &mut failure);
    if s == NO_SOCKET {
        return String::new();
    }
    if adb_send(s, &format!("shell:{command}"), &mut failure, 5000) {
        let mut buf = [0u8; 512];
        while out.len() < 4096 && sock::wait_readable(s, 3000) {
            let n = sock::recv(s, &mut buf);
            if n <= 0 {
                break;
            }
            out.extend_from_slice(&buf[..n as usize]);
        }
    }
    close(s);
    String::from_utf8_lossy(&out).into_owned()
}

/// Network adb ("192.168.1.5:5555", "adb-XXXX._adb-tls-connect._tcp") and emulators are not cables.
fn is_cable(serial: &str) -> bool {
    !serial.contains(':') && !contains(serial, "._adb-tls") && !serial.starts_with("emulator-")
}

fn trim(s: &str) -> String {
    s.trim_matches(|c| matches!(c, ' ' | '\t' | '\r' | '\n'))
        .to_string()
}

// ---- XML property lists (usbmuxd and lockdownd) ------------------------------------------------

fn xml_escape(s: &str) -> String {
    let mut out = String::new();
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            c => out.push(c),
        }
    }
    out
}

/// One dict of strings and integers, which is all the requests need.
fn plist_dict(strings: &[(&str, &str)], integers: &[(&str, i64)]) -> String {
    let mut x = String::from(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n\
         <!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n\
         <plist version=\"1.0\">\n<dict>\n",
    );
    for (k, v) in strings {
        x.push_str(&format!(
            "\t<key>{}</key>\n\t<string>{}</string>\n",
            xml_escape(k),
            xml_escape(v)
        ));
    }
    for (k, v) in integers {
        x.push_str(&format!(
            "\t<key>{}</key>\n\t<integer>{}</integer>\n",
            xml_escape(k),
            v
        ));
    }
    x.push_str("</dict>\n</plist>\n");
    x
}

fn append_utf8(out: &mut Vec<u8>, cp: u64) {
    // The C++ writes the 1-4 byte form of whatever number it got (no validity check).
    let cp = cp as u32;
    if cp < 0x80 {
        out.push(cp as u8);
    } else if cp < 0x800 {
        out.push((0xC0 | (cp >> 6)) as u8);
        out.push((0x80 | (cp & 0x3F)) as u8);
    } else if cp < 0x10000 {
        out.push((0xE0 | (cp >> 12)) as u8);
        out.push((0x80 | ((cp >> 6) & 0x3F)) as u8);
        out.push((0x80 | (cp & 0x3F)) as u8);
    } else {
        out.push((0xF0 | (cp >> 18)) as u8);
        out.push((0x80 | ((cp >> 12) & 0x3F)) as u8);
        out.push((0x80 | ((cp >> 6) & 0x3F)) as u8);
        out.push((0x80 | (cp & 0x3F)) as u8);
    }
}

fn find_byte(s: &[u8], from: usize, b: u8) -> Option<usize> {
    s.get(from..)?
        .iter()
        .position(|c| *c == b)
        .map(|i| i + from)
}

fn find_seq(s: &[u8], from: usize, pat: &[u8]) -> Option<usize> {
    s.get(from..)?
        .windows(pat.len())
        .position(|w| w == pat)
        .map(|i| i + from)
}

/// Reads the XML plists usbmuxd and lockdownd send into a `Value`: dict, array, string, integer,
/// real, true, false; data and date stay as their text.
struct PlistReader<'a> {
    s: &'a [u8],
    p: usize,
    depth: i32,
}

impl<'a> PlistReader<'a> {
    fn new(xml: &'a [u8]) -> Self {
        PlistReader {
            s: xml,
            p: 0,
            depth: 0,
        }
    }

    fn read(&mut self) -> Value {
        while let Some(tag) = self.next_tag() {
            if tag != "plist" && tag != "/plist" {
                return self.element(&tag);
            }
        }
        Value::Null
    }

    /// The next tag's name without attributes: "dict", "/dict", "true/" (self-closing)...
    fn next_tag(&mut self) -> Option<String> {
        loop {
            let open = find_byte(self.s, self.p, b'<')?;
            if self.s[open..].starts_with(b"<!--") {
                let end = find_seq(self.s, open, b"-->")?;
                self.p = end + 3;
                continue;
            }
            let close = find_byte(self.s, open, b'>')?;
            self.p = close + 1;
            if open + 1 < self.s.len() && (self.s[open + 1] == b'?' || self.s[open + 1] == b'!') {
                continue; // <?xml?>, <!DOCTYPE>
            }
            let mut t: Vec<u8> = self.s[open + 1..close].to_vec();
            let self_closing = t.last() == Some(&b'/');
            if self_closing {
                t.pop();
            }
            if let Some(space) = t
                .iter()
                .position(|c| matches!(c, b' ' | b'\t' | b'\r' | b'\n'))
            {
                t.truncate(space);
            }
            let mut name = String::from_utf8_lossy(&t).into_owned();
            if self_closing {
                name.push('/');
            }
            return Some(name);
        }
    }

    /// Text up to the next tag, entities decoded.
    fn text(&mut self) -> String {
        let end = find_byte(self.s, self.p, b'<');
        let raw = &self.s[self.p.min(self.s.len())..end.unwrap_or(self.s.len())];
        self.p = end.unwrap_or(self.s.len());
        let mut out: Vec<u8> = Vec::new();
        let mut i = 0;
        while i < raw.len() {
            if raw[i] != b'&' {
                out.push(raw[i]);
                i += 1;
                continue;
            }
            let Some(semi) = find_byte(raw, i, b';') else {
                out.push(raw[i]);
                i += 1;
                continue;
            };
            let e = &raw[i + 1..semi];
            match e {
                b"amp" => out.push(b'&'),
                b"lt" => out.push(b'<'),
                b"gt" => out.push(b'>'),
                b"quot" => out.push(b'"'),
                b"apos" => out.push(b'\''),
                _ if !e.is_empty() && e[0] == b'#' => {
                    let (cp, _) = if e.len() > 1 && (e[1] == b'x' || e[1] == b'X') {
                        strtou(&e[2..], 16)
                    } else {
                        strtou(&e[1..], 10)
                    };
                    append_utf8(&mut out, cp);
                }
                _ => {
                    out.push(b'&');
                    out.extend_from_slice(e);
                    out.push(b';');
                }
            }
            i = semi + 1;
        }
        String::from_utf8_lossy(&out).into_owned()
    }

    /// Text content of a <tag>...</tag> whose opening tag was just read.
    fn content(&mut self) -> String {
        let text = self.text();
        let _ = self.next_tag();
        text
    }

    fn element(&mut self, tag: &str) -> Value {
        self.depth += 1;
        if self.depth > 32 {
            return Value::Null; // malformed or hostile input (the C++ never counts this one back)
        }
        let mut v = Value::Null;
        match tag {
            "dict" => {
                v = Value::object();
                while let Some(mut t) = self.next_tag() {
                    if t == "/dict" {
                        break;
                    }
                    if t != "key" && t != "key/" {
                        continue;
                    }
                    let key = if t == "key" {
                        self.content()
                    } else {
                        String::new()
                    };
                    match self.next_tag() {
                        Some(n) => t = n,
                        None => break,
                    }
                    let el = self.element(&t);
                    v.set(&key, el);
                }
            }
            "array" => {
                v = Value::array();
                while let Some(t) = self.next_tag() {
                    if t == "/array" {
                        break;
                    }
                    let el = self.element(&t);
                    v.push(el);
                }
            }
            "dict/" => v = Value::object(),
            "array/" => v = Value::array(),
            "true/" => v = Value::Bool(true),
            "false/" => v = Value::Bool(false),
            "string/" | "data/" => v = Value::String(String::new()),
            "integer" => v = Value::from(strtoll10(&self.content())),
            "real" => v = Value::Number(strtod_prefix(&self.content())),
            "string" | "data" | "date" | "key" => v = Value::String(self.content()),
            _ => {}
        }
        self.depth -= 1;
        v
    }
}

fn parse_plist(xml: &[u8]) -> Value {
    PlistReader::new(xml).read()
}

// ---- usbmuxd ---------------------------------------------------------------------------------

/// Message: 16-byte little-endian header (length including header, version 1, type 8 = plist,
/// tag), then an XML plist.
fn usbmux_send(s: Socket, message_type: &str, integers: &[(&str, i64)]) -> bool {
    let mut fields: Vec<(&str, i64)> = integers.to_vec();
    fields.push(("kLibUSBMuxVersion", 3));
    let body = plist_dict(
        &[
            ("MessageType", message_type),
            ("ProgName", "NiwCam"),
            ("ClientVersionString", "NiwCam"),
        ],
        &fields,
    );
    let mut head = [0u8; 16];
    head[0..4].copy_from_slice(&put_le32(16 + body.len() as u32));
    head[4..8].copy_from_slice(&put_le32(1));
    head[8..12].copy_from_slice(&put_le32(8));
    head[12..16].copy_from_slice(&put_le32(1));
    sock::send_all(s, &head) && sock::send_all(s, body.as_bytes())
}

#[doc(hidden)]
/// The exact bytes `usbmux_send` puts on the wire (for tests against the C++ format strings).
pub fn usbmux_message_bytes(message_type: &str, integers: &[(&str, i64)]) -> Vec<u8> {
    let mut fields: Vec<(&str, i64)> = integers.to_vec();
    fields.push(("kLibUSBMuxVersion", 3));
    let body = plist_dict(
        &[
            ("MessageType", message_type),
            ("ProgName", "NiwCam"),
            ("ClientVersionString", "NiwCam"),
        ],
        &fields,
    );
    let mut out = Vec::new();
    out.extend_from_slice(&put_le32(16 + body.len() as u32));
    out.extend_from_slice(&put_le32(1));
    out.extend_from_slice(&put_le32(8));
    out.extend_from_slice(&put_le32(1));
    out.extend_from_slice(body.as_bytes());
    out
}

#[doc(hidden)]
/// `ParsePlist` for tests.
pub fn parse_plist_text(xml: &str) -> Value {
    parse_plist(xml.as_bytes())
}

fn usbmux_read(s: Socket, out: &mut Value, timeout_ms: i32) -> bool {
    let mut head = [0u8; 16];
    if !sock::recv_all(s, &mut head, timeout_ms) {
        return false;
    }
    let length = get_le32(&head);
    if length < 16 || length - 16 > MAX_PLIST {
        return false;
    }
    let mut body = vec![0u8; (length - 16) as usize];
    if !body.is_empty() && !sock::recv_all(s, &mut body, 5000) {
        return false;
    }
    *out = parse_plist(&body);
    out.is_object()
}

/// Asks usbmuxd to connect to `port` on the phone; on success the socket becomes the tunnel.
/// Returns usbmuxd's result code (0 = connected, 3 = refused), or -1 if usbmuxd is not there.
fn usbmux_connect(mux_id: u32, port: u16, tunnel: &mut Socket) -> i32 {
    *tunnel = NO_SOCKET;
    let s = connect_usbmuxd();
    if s == NO_SOCKET {
        return -1;
    }
    // PortNumber is in network byte order, read as a little-endian integer.
    let swapped = port.swap_bytes();
    let mut reply = Value::Null;
    if !usbmux_send(
        s,
        "Connect",
        &[
            ("DeviceID", i64::from(mux_id)),
            ("PortNumber", i64::from(swapped)),
        ],
    ) || !usbmux_read(s, &mut reply, 5000)
    {
        close(s);
        return -1;
    }
    let result = reply.get("Number").int(-1);
    if result == 0 {
        *tunnel = s;
    } else {
        close(s);
    }
    result
}

fn device_from_properties(props: &Value, mux_id: u32, d: &mut Device) -> bool {
    if props.get("ConnectionType").str_or("USB") != "USB" {
        return false; // Wi-Fi sync lists iPhones as "Network"
    }
    d.kind = Kind::Usbmux;
    d.serial = props.get("SerialNumber").str().to_string();
    d.state = "device".to_string();
    d.mux_id = mux_id;
    !d.serial.is_empty()
}

/// lockdownd frames: u32 big-endian length, then an XML plist. Like the C++, `value` is overwritten
/// even when the answer is empty.
fn lockdown_get(s: Socket, key: &str, value: &mut String) -> bool {
    let body = plist_dict(
        &[("Label", "NiwCam"), ("Request", "GetValue"), ("Key", key)],
        &[],
    );
    let n = body.len() as u32;
    let head = n.to_be_bytes();
    let mut len = [0u8; 4];
    if !sock::send_all(s, &head)
        || !sock::send_all(s, body.as_bytes())
        || !sock::recv_all(s, &mut len, 3000)
    {
        return false;
    }
    let size = u32::from_be_bytes(len);
    if size == 0 || size > MAX_PLIST {
        return false;
    }
    let mut reply = vec![0u8; size as usize];
    if !sock::recv_all(s, &mut reply, 3000) {
        return false;
    }
    *value = parse_plist(&reply).get("Value").str().to_string();
    !value.is_empty()
}

const APP_NOT_OPEN: &str =
    "Không kết nối được tới app NiwCam trên điện thoại qua cáp USB. Hãy mở app trên điện thoại.";
/// Nothing listens on the screen port unless the phone is sharing its screen.
const NOT_SHARING: &str = "Điện thoại chưa chia sẻ màn hình. Trong app NiwCam trên điện thoại, bấm \"Chia sẻ màn hình sang máy tính\", rồi chọn lại nguồn này.";

// ---- adb -------------------------------------------------------------------------------------

/// One-shot listing of phones on a cable (network adb and emulators left out). `Err` carries a
/// message for the user when the service is not running.
pub fn list_adb() -> Result<Vec<Device>, String> {
    let s = connect_loopback(ADB_PORT);
    if s == NO_SOCKET {
        return Err("adb chưa chạy".to_string());
    }
    let mut failure = String::new();
    let mut text = String::new();
    let ok =
        adb_send(s, "host:devices-l", &mut failure, 5000) && adb_read_block(s, &mut text, 5000);
    close(s);
    if !ok {
        return Err(format!("adb không trả lời: {failure}"));
    }
    // "<serial>   device product:lime_global model:M2010J19SG device:lime transport_id:1"
    let mut out = Vec::new();
    for line in text.split('\n') {
        let words: Vec<&str> = line
            .split([' ', '\t', '\r'])
            .filter(|w| !w.is_empty())
            .collect();
        if words.len() < 2 || !is_cable(words[0]) {
            continue;
        }
        let mut d = Device {
            kind: Kind::Adb,
            serial: words[0].to_string(),
            state: words[1].to_string(),
            ..Device::default()
        };
        if d.state != "device" && d.state != "unauthorized" && d.state != "authorizing" {
            continue; // offline, recovery...
        }
        for w in &words[2..] {
            if let Some(m) = w.strip_prefix("model:") {
                d.model = m.to_string();
            }
        }
        out.push(d);
    }
    Ok(out)
}

/// A connected, blocking socket that is a tunnel to the phone's TCP port, or `NO_SOCKET` and a
/// message for the user (Vietnamese, UTF-8). Takes a few milliseconds.
pub fn open_adb(serial: &str, port: u16, error: &mut String) -> Socket {
    let mut failure = String::new();
    let s = adb_transport(serial, &mut failure);
    if s == NO_SOCKET {
        *error = if failure == "no adb server" {
            "Chưa chạy được adb để kết nối điện thoại Android qua cáp USB.".to_string()
        } else if contains(&failure, "unauthorized") {
            "Trên điện thoại, chọn \"Cho phép\" trong hộp thoại \"Cho phép gỡ lỗi USB?\" rồi thử lại.".to_string()
        } else if contains(&failure, "offline") || contains(&failure, "authorizing") {
            "Điện thoại đang kết nối lại qua cáp USB. Thử lại sau giây lát.".to_string()
        } else {
            "Không thấy điện thoại trên cáp USB. Cắm lại cáp, và bật \"Gỡ lỗi USB\" trong Tùy chọn cho nhà phát triển.".to_string()
        };
        return NO_SOCKET;
    }
    // adbd connects to the port on the phone itself; FAIL ("closed") when nothing listens there.
    if !adb_send(s, &format!("tcp:{port}"), &mut failure, 8000) {
        close(s);
        *error = if port == SCREEN_PORT {
            NOT_SHARING
        } else {
            APP_NOT_OPEN
        }
        .to_string();
        return NO_SOCKET;
    }
    s
}

/// The phone's own name: Android "settings get global device_name" over adb. Empty when it
/// cannot be read.
pub fn adb_device_name(serial: &str) -> String {
    // The same name the Android app uses (Store.deviceName): Settings.Global.DEVICE_NAME.
    let name = trim(&adb_shell(serial, "settings get global device_name"));
    if name == "null" || contains(&name, "rror") || name.len() > 100 {
        String::new()
    } else {
        name
    }
}

// ---- usbmux ----------------------------------------------------------------------------------

pub fn list_usbmux() -> Result<Vec<Device>, String> {
    let s = connect_usbmuxd();
    if s == NO_SOCKET {
        return Err("Để dùng iPhone qua cáp USB, hãy cài app Apple Devices (Microsoft Store) hoặc iTunes trên máy tính.".to_string());
    }
    let mut reply = Value::Null;
    let ok = usbmux_send(s, "ListDevices", &[]) && usbmux_read(s, &mut reply, 5000);
    close(s);
    if !ok {
        return Err("Dịch vụ thiết bị Apple không trả lời".to_string());
    }
    let mut out = Vec::new();
    for entry in reply.get("DeviceList").items() {
        let mut d = Device::default();
        if device_from_properties(
            entry.get("Properties"),
            entry.get("DeviceID").int(0) as u32,
            &mut d,
        ) {
            out.push(d);
        }
    }
    Ok(out)
}

pub fn open_usbmux(udid: &str, port: u16, error: &mut String) -> Socket {
    // The DeviceID changes every time the cable is plugged in: look it up by UDID each time.
    let devices = match list_usbmux() {
        Ok(d) => d,
        Err(e) => {
            *error = e;
            return NO_SOCKET;
        }
    };
    let mut phone: Option<&Device> = None;
    for d in &devices {
        if d.serial == udid {
            phone = Some(d);
        }
    }
    let Some(phone) = phone else {
        *error = "Không thấy iPhone trên cáp USB. Cắm lại cáp và mở khóa iPhone.".to_string();
        return NO_SOCKET;
    };
    let mut tunnel = NO_SOCKET;
    let result = usbmux_connect(phone.mux_id, port, &mut tunnel);
    if result == 0 {
        return tunnel;
    }
    *error = if result == 3 && port == SCREEN_PORT {
        "iPhone chưa chia sẻ màn hình. Trong app NiwCam trên iPhone, bấm \"Chia sẻ màn hình sang máy tính\" rồi Bắt đầu phát, sau đó chọn lại nguồn này.".to_string()
    } else if result == 3 {
        "Không kết nối được tới app NiwCam trên iPhone qua cáp USB. Hãy mở app; nếu iPhone hỏi \"Tin cậy máy tính này?\", chọn Tin cậy.".to_string()
    } else if result < 0 {
        "Dịch vụ thiết bị Apple không trả lời".to_string()
    } else {
        format!("iPhone từ chối kết nối qua cáp USB (mã {result})")
    };
    NO_SOCKET
}

/// The iPhone's name from lockdownd, asked without a session (no pairing, nothing shown on the
/// phone). Empty when it cannot be read. `product_type` receives ProductType like the C++.
pub fn iphone_name(mux_id: u32, product_type: Option<&mut String>) -> String {
    let mut s = NO_SOCKET;
    if usbmux_connect(mux_id, LOCKDOWN_PORT, &mut s) != 0 {
        return String::new();
    }
    let mut name = String::new();
    lockdown_get(s, "DeviceName", &mut name);
    if let Some(pt) = product_type {
        lockdown_get(s, "ProductType", pt);
    }
    close(s);
    name
}

// ---- watcher ---------------------------------------------------------------------------------

/// A hook the watcher calls from its adb thread (see `Watcher::start`).
pub type AdbHook = Arc<dyn Fn() -> bool + Send + Sync>;

#[derive(Default)]
struct WatchState {
    stop: bool,
    /// the open watch sockets, shut down by `stop()`
    adb_socket: Option<Socket>,
    usbmux_socket: Option<Socket>,
    adb: Vec<Device>,
    usbmux: Vec<Device>,
}

struct WatcherInner {
    state: Mutex<WatchState>,
    wake: Condvar,
    start_adb: Mutex<Option<AdbHook>>,
    stop_idle_adb: Mutex<Option<AdbHook>>,
    /// serial -> name, looked up once
    adb_names: Mutex<BTreeMap<String, String>>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Slot {
    Adb,
    Usbmux,
}

/// Keeps the list of phones on a cable current in a background thread per service. adb
/// ("host:track-devices") and usbmuxd ("Listen") push every change, so nothing is polled while they
/// run; when a service is missing it is looked for again every few seconds.
pub struct Watcher {
    inner: Arc<WatcherInner>,
    threads: Mutex<Vec<JoinHandle<()>>>,
}

impl Default for Watcher {
    fn default() -> Self {
        Self::new()
    }
}

impl Drop for Watcher {
    fn drop(&mut self) {
        self.stop();
    }
}

impl Watcher {
    pub fn new() -> Self {
        Watcher {
            inner: Arc::new(WatcherInner {
                state: Mutex::new(WatchState::default()),
                wake: Condvar::new(),
                start_adb: Mutex::new(None),
                stop_idle_adb: Mutex::new(None),
                adb_names: Mutex::new(BTreeMap::new()),
            }),
            threads: Mutex::new(Vec::new()),
        }
    }

    /// `start_adb` runs when no adb server answers. It may start one (Windows: only while an Android
    /// phone with USB debugging is plugged in) and returns true if it did. `stop_idle_adb` runs
    /// after a minute with no phone on adb; it may stop the server (it scans USB every second) and
    /// returns true if it did.
    pub fn start(&self, start_adb: Option<AdbHook>, stop_idle_adb: Option<AdbHook>) {
        self.stop();
        self.inner.state.lk().stop = false;
        *self.inner.start_adb.lk() = start_adb;
        *self.inner.stop_idle_adb.lk() = stop_idle_adb;
        let a = Arc::clone(&self.inner);
        let b = Arc::clone(&self.inner);
        let mut threads = self.threads.lk();
        threads.push(std::thread::spawn(move || a.run_adb()));
        threads.push(std::thread::spawn(move || b.run_usbmux()));
    }

    pub fn stop(&self) {
        {
            let mut st = self.inner.state.lk();
            st.stop = true;
            // Wakes the blocking reads; each thread closes its own socket.
            if let Some(s) = st.adb_socket {
                sock::shutdown(s);
            }
            if let Some(s) = st.usbmux_socket {
                sock::shutdown(s);
            }
        }
        self.inner.wake.notify_all();
        let threads: Vec<_> = self.threads.lk().drain(..).collect();
        for t in threads {
            let _ = t.join();
        }
        let mut st = self.inner.state.lk();
        st.adb.clear();
        st.usbmux.clear();
    }

    /// Phones ready to list. A phone is added once its name is known, so the name never changes
    /// while it stays plugged in (the app finds cameras again by name).
    pub fn devices(&self) -> Vec<Device> {
        let st = self.inner.state.lk();
        let mut all = st.adb.clone();
        all.extend(st.usbmux.iter().cloned());
        all
    }
}

impl WatcherInner {
    /// False once `stop()` was called.
    fn pause(&self, ms: u64) -> bool {
        let st = self.state.lk();
        let (st, _) = self
            .wake
            .wait_timeout_while(st, Duration::from_millis(ms), |s| !s.stop)
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        !st.stop
    }

    fn register(&self, slot: Slot, s: Socket) -> bool {
        let mut st = self.state.lk();
        if st.stop {
            return false;
        }
        match slot {
            Slot::Adb => st.adb_socket = Some(s),
            Slot::Usbmux => st.usbmux_socket = Some(s),
        }
        true
    }

    fn unregister(&self, slot: Slot, s: Socket) {
        {
            let mut st = self.state.lk();
            match slot {
                Slot::Adb => st.adb_socket = None,
                Slot::Usbmux => st.usbmux_socket = None,
            }
        }
        close(s);
    }

    fn run_adb(&self) {
        loop {
            let s = connect_loopback(ADB_PORT);
            if s == NO_SOCKET {
                // No adb server: maybe start one (only worth it with a phone plugged in), else look again.
                let hook = self.start_adb.lk().clone();
                let started = hook.is_some_and(|f| f());
                if !self.pause(if started { 300 } else { 3000 }) {
                    return;
                }
                continue;
            }
            if !self.register(Slot::Adb, s) {
                close(s);
                return;
            }
            let mut failure = String::new();
            let mut block = String::new();
            // The server sends the device list now and again after every change.
            if adb_send(s, "host:track-devices", &mut failure, 5000) {
                loop {
                    let none = self.state.lk().adb.is_empty();
                    // No phone for a minute: let the server go if we started it. Its socket then closes.
                    if none {
                        let hook = self.stop_idle_adb.lk().clone();
                        if let Some(f) = hook {
                            if !sock::wait_readable(s, 60000) {
                                f();
                                continue;
                            }
                        }
                    }
                    if !adb_read_block(s, &mut block, -1) {
                        break;
                    }
                    self.update_adb();
                }
            }
            self.unregister(Slot::Adb, s);
            // the server went away (killed, replaced by another adb version...)
            self.state.lk().adb.clear();
            if !self.pause(1000) {
                return;
            }
        }
    }

    fn update_adb(&self) {
        let Ok(mut devices) = list_adb() else {
            return;
        };
        for d in &mut devices {
            if d.state != "device" {
                continue; // unauthorized: nothing can be asked yet
            }
            let known = self.adb_names.lk().get(&d.serial).cloned();
            let name = match known {
                Some(n) => n,
                None => {
                    let mut name = adb_device_name(&d.serial);
                    if name.is_empty() {
                        name = d.model.clone(); // "Redmi_9T" style
                    }
                    let name = name.replace('_', " ");
                    self.adb_names.lk().insert(d.serial.clone(), name.clone());
                    name
                }
            };
            d.name = name;
        }
        self.state.lk().adb = devices;
    }

    fn run_usbmux(&self) {
        loop {
            let s = connect_usbmuxd();
            if s == NO_SOCKET {
                // No Apple Mobile Device Service (Windows without Apple Devices/iTunes): look again later.
                if !self.pause(5000) {
                    return;
                }
                continue;
            }
            if !self.register(Slot::Usbmux, s) {
                close(s);
                return;
            }
            let mut attached: BTreeMap<u32, Device> = BTreeMap::new();
            let mut reply = Value::Null;
            let mut message = Value::Null;
            if usbmux_send(s, "Listen", &[])
                && usbmux_read(s, &mut reply, 5000)
                && reply.get("Number").int(-1) == 0
            {
                // "Attached" for every phone already there, then one message per change.
                while usbmux_read(s, &mut message, -1) {
                    let kind = message.get("MessageType").str().to_string();
                    let id = message.get("DeviceID").int(0) as u32;
                    if kind == "Attached" {
                        let mut d = Device::default();
                        if !device_from_properties(message.get("Properties"), id, &mut d) {
                            continue;
                        }
                        // lockdownd may not answer in the first moments after the cable goes in.
                        let mut attempt = 0;
                        while attempt < 3 && d.name.is_empty() {
                            let mut model = std::mem::take(&mut d.model);
                            d.name = iphone_name(id, Some(&mut model));
                            d.model = model;
                            if d.name.is_empty() && !self.pause(700) {
                                break;
                            }
                            attempt += 1;
                        }
                        attached.insert(id, d);
                    } else if kind == "Detached" {
                        attached.remove(&id);
                    } else {
                        continue;
                    }
                    let list: Vec<Device> = attached.values().cloned().collect();
                    self.state.lk().usbmux = list;
                }
            }
            self.unregister(Slot::Usbmux, s);
            self.state.lk().usbmux.clear();
            if !self.pause(2000) {
                return;
            }
        }
    }
}

// ---- starting adb (Windows) --------------------------------------------------------------------

mod start {
    use super::*;
    use std::path::{Path, PathBuf};
    use windows::core::{GUID, PCWSTR, PWSTR};
    use windows::Win32::Devices::DeviceAndDriverInstallation::{
        CM_Get_Device_Interface_List_SizeW, CM_GET_DEVICE_INTERFACE_LIST_PRESENT, CR_SUCCESS,
    };
    use windows::Win32::Foundation::{CloseHandle, HMODULE, MAX_PATH};
    use windows::Win32::Storage::FileSystem::SearchPathW;
    use windows::Win32::System::LibraryLoader::{
        GetModuleFileNameW, GetModuleHandleExW, GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS,
        GET_MODULE_HANDLE_EX_FLAG_UNCHANGED_REFCOUNT,
    };
    use windows::Win32::System::Threading::{
        CreateProcessW, WaitForSingleObject, CREATE_NO_WINDOW, PROCESS_INFORMATION, STARTUPINFOW,
    };

    /// The interface class every phone adb can see registers (AdbWinApi's ANDROID_USB_CLASS_ID).
    const ADB_INTERFACE: GUID = GUID::from_values(
        0xF72FE0D4,
        0xCBCB,
        0x407D,
        [0x88, 0x14, 0x9E, 0xD6, 0x73, 0xD0, 0xDD, 0x6B],
    );
    pub static STARTED_ADB: AtomicBool = AtomicBool::new(false);
    static LAST_START: AtomicU64 = AtomicU64::new(0);

    pub fn adb_phone_plugged() -> bool {
        let mut size = 0u32;
        // SAFETY: `size` and the class GUID outlive the call; no device id filter.
        let r = unsafe {
            CM_Get_Device_Interface_List_SizeW(
                &mut size,
                &ADB_INTERFACE,
                PCWSTR::null(),
                CM_GET_DEVICE_INTERFACE_LIST_PRESENT,
            )
        };
        if r != CR_SUCCESS {
            return false;
        }
        size > 1 // an empty list is just its terminating NUL
    }

    fn file_exists(p: &Path) -> bool {
        std::fs::symlink_metadata(p).is_ok()
    }

    fn wide(s: &std::ffi::OsStr) -> Vec<u16> {
        use std::os::windows::ffi::OsStrExt;
        s.encode_wide().chain(std::iter::once(0)).collect()
    }

    pub fn find_adb() -> Option<PathBuf> {
        // 1. Shipped with the installer: App\adb\adb.exe, next to NiwCamEngine.dll.
        let mut self_module = HMODULE::default();
        let mut path = [0u16; MAX_PATH as usize];
        // SAFETY: the address is that of a function in this module; the flags make Windows
        // look the module up by address without changing its reference count.
        let found = unsafe {
            GetModuleHandleExW(
                GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS
                    | GET_MODULE_HANDLE_EX_FLAG_UNCHANGED_REFCOUNT,
                PCWSTR(find_adb as *const () as *const u16),
                &mut self_module,
            )
            .is_ok()
        };
        if found {
            // SAFETY: `path` is a writable buffer of MAX_PATH wide chars.
            let n = unsafe { GetModuleFileNameW(Some(self_module), &mut path) } as usize;
            if n > 0 {
                let exe = PathBuf::from(String::from_utf16_lossy(&path[..n.min(path.len())]));
                if let Some(dir) = exe.parent() {
                    let adb = dir.join("adb").join("adb.exe");
                    if file_exists(&adb) {
                        return Some(adb);
                    }
                }
            }
        }
        // 2. The Android SDK (developer machines), 3. PATH.
        if let Some(local) = std::env::var_os("LOCALAPPDATA") {
            let sdk = PathBuf::from(local)
                .join("Android")
                .join("Sdk")
                .join("platform-tools")
                .join("adb.exe");
            if file_exists(&sdk) {
                return Some(sdk);
            }
        }
        let name = wide(std::ffi::OsStr::new("adb.exe"));
        let mut buf = [0u16; MAX_PATH as usize];
        // SAFETY: `name` is NUL terminated; `buf` is a writable MAX_PATH buffer.
        let n = unsafe {
            SearchPathW(
                PCWSTR::null(),
                PCWSTR(name.as_ptr()),
                PCWSTR::null(),
                Some(&mut buf),
                None,
            )
        } as usize;
        if n > 0 && n < buf.len() {
            return Some(PathBuf::from(String::from_utf16_lossy(&buf[..n])));
        }
        None
    }

    pub fn start_adb_server() -> bool {
        // Without a phone that has USB debugging on, adb would find nothing: don't run it.
        if !adb_phone_plugged() {
            return false;
        }
        let now = tick_ms();
        let last = LAST_START.load(Ordering::SeqCst);
        if last != 0 && now - last < 30000 {
            return false;
        }
        LAST_START.store(now, Ordering::SeqCst);
        let Some(adb) = find_adb() else {
            return false;
        };
        let adb_w = wide(adb.as_os_str());
        let mut command: Vec<u16> = format!("\"{}\" start-server", adb.display())
            .encode_utf16()
            .chain(std::iter::once(0))
            .collect();
        let dir = wide(adb.parent().unwrap_or(Path::new("")).as_os_str());
        let si = STARTUPINFOW {
            cb: std::mem::size_of::<STARTUPINFOW>() as u32,
            ..Default::default()
        };
        let mut pi = PROCESS_INFORMATION::default();
        // SAFETY: all wide strings are NUL terminated and outlive the call; `command` is mutable as
        // CreateProcessW requires; `si` and `pi` are valid.
        let created = unsafe {
            CreateProcessW(
                PCWSTR(adb_w.as_ptr()),
                Some(PWSTR(command.as_mut_ptr())),
                None,
                None,
                false,
                CREATE_NO_WINDOW,
                None,
                PCWSTR(dir.as_ptr()),
                &si,
                &mut pi,
            )
        };
        if created.is_err() {
            return false;
        }
        // "start-server" returns once the server answers; the server keeps running on its own.
        // SAFETY: both handles came from CreateProcessW just above and are closed once.
        unsafe {
            WaitForSingleObject(pi.hProcess, 15000);
            let _ = CloseHandle(pi.hThread);
            let _ = CloseHandle(pi.hProcess);
        }
        STARTED_ADB.store(true, Ordering::SeqCst);
        true
    }
}

/// Starts the adb server when an Android phone with USB debugging is plugged in and none runs:
/// adb.exe from the installer (App\adb), the Android SDK, or PATH. At most one try every 30 s.
pub fn start_adb_server() -> bool {
    start::start_adb_server()
}

/// Stops the server `start_adb_server` started, if any (never one that something else started).
pub fn stop_adb_server() {
    if !start::STARTED_ADB.swap(false, Ordering::SeqCst) {
        return;
    }
    let s = connect_loopback(ADB_PORT);
    if s == NO_SOCKET {
        return;
    }
    let mut failure = String::new();
    adb_send(s, "host:kill", &mut failure, 2000);
    close(s);
}

/// `stop_adb_server` once no phone with USB debugging is plugged in any more. True if it stopped it.
pub fn stop_idle_adb_server() -> bool {
    if !start::STARTED_ADB.load(Ordering::SeqCst) || start::adb_phone_plugged() {
        return false;
    }
    stop_adb_server();
    true
}

#[doc(hidden)]
/// True when this crate started the adb server it is talking to (for tests: it must stay false).
pub fn started_adb_server() -> bool {
    start::STARTED_ADB.load(Ordering::SeqCst)
}

#[doc(hidden)]
/// Where `find_adb` would look first and what it finds, without starting anything.
pub fn find_adb_path() -> Option<std::path::PathBuf> {
    start::find_adb()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn listen_message_matches_the_cpp_format_string() {
        // Built by hand from UsbTransport.cpp: PlistDict + 16-byte header, tag 1.
        let body = "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n\
<!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n\
<plist version=\"1.0\">\n<dict>\n\
\t<key>MessageType</key>\n\t<string>Listen</string>\n\
\t<key>ProgName</key>\n\t<string>NiwCam</string>\n\
\t<key>ClientVersionString</key>\n\t<string>NiwCam</string>\n\
\t<key>kLibUSBMuxVersion</key>\n\t<integer>3</integer>\n\
</dict>\n</plist>\n";
        let mut want = Vec::new();
        want.extend_from_slice(&(16 + body.len() as u32).to_le_bytes());
        want.extend_from_slice(&1u32.to_le_bytes());
        want.extend_from_slice(&8u32.to_le_bytes());
        want.extend_from_slice(&1u32.to_le_bytes());
        want.extend_from_slice(body.as_bytes());
        assert_eq!(usbmux_message_bytes("Listen", &[]), want);
    }

    #[test]
    fn connect_message_swaps_the_port() {
        let m = usbmux_message_bytes("Connect", &[("DeviceID", 7), ("PortNumber", 55736)]);
        let text = String::from_utf8_lossy(&m[16..]).into_owned();
        assert!(text.contains("<key>DeviceID</key>\n\t<integer>7</integer>\n\t<key>PortNumber</key>\n\t<integer>55736</integer>\n\t<key>kLibUSBMuxVersion</key>"));
        assert_eq!(47321u16.swap_bytes(), 55736);
    }

    #[test]
    fn plist_reader_reads_a_device_list() {
        let xml = "<?xml version=\"1.0\"?><!DOCTYPE plist PUBLIC \"x\" \"y\"><plist version=\"1.0\"><dict>\
<key>DeviceList</key><array><dict><key>DeviceID</key><integer>5</integer><key>MessageType</key><string>Attached</string>\
<key>Properties</key><dict><key>ConnectionType</key><string>USB</string><key>SerialNumber</key><string>00008110-AB&amp;CD</string>\
<key>ok</key><true/><key>no</key><false/><key>r</key><real>1.5</real><key>empty</key><string/></dict></dict></array></dict></plist>";
        let v = parse_plist(xml.as_bytes());
        let d = v.get("DeviceList").items()[0].clone();
        assert_eq!(d.get("DeviceID").int(0), 5);
        assert_eq!(
            d.get("Properties").get("SerialNumber").str(),
            "00008110-AB&CD"
        );
        assert!(d.get("Properties").get("ok").bool_or(false));
        assert!(!d.get("Properties").get("no").bool_or(true));
        assert_eq!(d.get("Properties").get("r").num(0.0), 1.5);
        assert!(d.get("Properties").get("empty").is_string());
        let mut dev = Device::default();
        assert!(device_from_properties(d.get("Properties"), 5, &mut dev));
        assert_eq!(dev.kind, Kind::Usbmux);
        assert_eq!(dev.mux_id, 5);
        let net = parse_plist(
            b"<plist><dict><key>ConnectionType</key><string>Network</string></dict></plist>",
        );
        assert!(!device_from_properties(&net, 1, &mut dev));
    }

    #[test]
    fn cable_filter_matches_the_cpp() {
        assert!(is_cable("1234567890ABCDEF"));
        assert!(!is_cable("192.168.1.5:5555"));
        assert!(!is_cable("adb-XXXX._adb-tls-connect._tcp"));
        assert!(!is_cable("emulator-5554"));
    }
}
