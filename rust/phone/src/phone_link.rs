//! PC side of docs/protocol.md: TCP connection to a phone, handshake and pairing (section 3), then
//! sealed records (section 2). Port of `src/engine/PhoneLink.{h,cpp}`.

use std::net::{SocketAddr, TcpStream, ToSocketAddrs};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Condvar, Mutex, OnceLock};
use std::time::{Duration, Instant};

use niwcam_core::crypto;
use niwcam_core::json::Value;

use crate::appdata::app_data_dir;
use crate::discovery::{known_phones, Endpoint, Transport};
use crate::sock::{self, Socket, NO_SOCKET};
use crate::usb_transport as usb;
use crate::util::{tick_ms, LockExt};

const HANDSHAKE: u8 = 0x01;
const SEALED: u8 = 0x02;
const MAX_HANDSHAKE: u32 = 64 * 1024;
const MAX_SEALED: u32 = 16 * 1024 * 1024;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Control,
    VideoConfig,
    VideoFrame,
    /// the phone's microphone, 16-bit PCM (docs/protocol.md 5.1), only when asked
    Audio,
    /// A kind this version does not know (the C++ `static_cast` keeps any byte).
    Other(u8),
}

impl Kind {
    pub fn from_u8(v: u8) -> Kind {
        match v {
            1 => Kind::Control,
            2 => Kind::VideoConfig,
            3 => Kind::VideoFrame,
            4 => Kind::Audio,
            v => Kind::Other(v),
        }
    }

    pub fn as_u8(self) -> u8 {
        match self {
            Kind::Control => 1,
            Kind::VideoConfig => 2,
            Kind::VideoFrame => 3,
            Kind::Audio => 4,
            Kind::Other(v) => v,
        }
    }
}

/// One record received from the phone. Reuse one `Message` for every `receive` call: the payload
/// buffer keeps its capacity (the C++ does `payload.assign`).
#[derive(Clone, Debug)]
pub struct Message {
    pub kind: Kind,
    pub pts_us: i64,
    pub flags: u32,
    pub payload: Vec<u8>,
}

impl Default for Message {
    fn default() -> Self {
        Message {
            kind: Kind::Control,
            pts_us: 0,
            flags: 0,
            payload: Vec::new(),
        }
    }
}

impl Message {
    pub fn keyframe(&self) -> bool {
        self.flags & 1 != 0
    }

    /// The payload as JSON (CONTROL records).
    pub fn json(&self) -> Result<Value, niwcam_core::json::JsonError> {
        Value::parse_bytes(&self.payload)
    }
}

/// What `PhoneLink::receive` produced (the C++ returns a bool and a `timedOut` out-parameter).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Recv {
    /// a record is in the message
    Message,
    /// nothing arrived within the timeout (the connection is fine)
    TimedOut,
    /// the connection is gone or tampered with (and closed)
    Failed,
}

/// Called during pairing with the 6-digit code to show, and with "" when pairing is over.
pub type PairingCallback<'a> = &'a dyn Fn(&str);
/// Blocks until the PC user accepts (true) or declines the code. Both sides must accept
/// (docs/protocol.md 3.2): otherwise a fake "phone" on the network could pair itself and feed the
/// virtual camera.
pub type PairingConfirm<'a> = &'a dyn Fn() -> bool;

// ---- identity & pairing store -------------------------------------------------------------------

fn data_dir() -> std::path::PathBuf {
    let dir = app_data_dir();
    let _ = std::fs::create_dir(&dir);
    let _ = std::fs::create_dir(dir.join("pairings"));
    dir
}

fn pair_file(device_id: &str) -> std::path::PathBuf {
    // File name from a hash: device ids are arbitrary text from the network.
    let h = crypto::to_hex(&crypto::sha256(&[device_id.as_bytes()]));
    data_dir()
        .join("pairings")
        .join(format!("{}.key", &h[..32]))
}

/// This PC's id, kept in `<data dir>\client-id.txt` (a lower-case GUID). Read once per process.
pub fn client_id() -> String {
    static ID: OnceLock<String> = OnceLock::new();
    ID.get_or_init(|| {
        let file = data_dir().join("client-id.txt");
        if let Ok(bytes) = std::fs::read(&file) {
            let text = String::from_utf8_lossy(&bytes).into_owned();
            if let Some(token) = text.split_whitespace().next() {
                if token.len() >= 32 {
                    return token.to_string();
                }
            }
        }
        // SAFETY: CoCreateGuid has no preconditions.
        let g = unsafe { windows::Win32::System::Com::CoCreateGuid() }.unwrap_or_default();
        let id = format!(
            "{:08x}-{:04x}-{:04x}-{:02x}{:02x}-{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}",
            g.data1,
            g.data2,
            g.data3,
            g.data4[0],
            g.data4[1],
            g.data4[2],
            g.data4[3],
            g.data4[4],
            g.data4[5],
            g.data4[6],
            g.data4[7]
        );
        let _ = std::fs::write(&file, &id);
        id
    })
    .clone()
}

/// The computer name (UTF-8), "PC" when Windows does not say.
pub fn client_name() -> String {
    use windows::core::PWSTR;
    use windows::Win32::System::WindowsProgramming::GetComputerNameW;
    let mut name = [0u16; 16]; // MAX_COMPUTERNAME_LENGTH (15) + 1
    let mut n = name.len() as u32;
    // SAFETY: `name` is writable for `n` wide chars; `n` is updated by the call.
    if unsafe { GetComputerNameW(Some(PWSTR(name.as_mut_ptr())), &mut n) }.is_err() {
        return "PC".to_string();
    }
    String::from_utf16_lossy(&name[..(n as usize).min(name.len())])
}

fn dpapi_unprotect(blob: &[u8]) -> Option<Vec<u8>> {
    use windows::Win32::Foundation::{LocalFree, HLOCAL};
    use windows::Win32::Security::Cryptography::{
        CryptUnprotectData, CRYPTPROTECT_UI_FORBIDDEN, CRYPT_INTEGER_BLOB,
    };
    let input = CRYPT_INTEGER_BLOB {
        cbData: blob.len() as u32,
        pbData: blob.as_ptr().cast_mut(),
    };
    let mut out = CRYPT_INTEGER_BLOB::default();
    // SAFETY: `input` points at `blob` for the call; `out` receives a LocalAlloc'ed buffer that is
    // copied, wiped and freed below.
    unsafe {
        CryptUnprotectData(
            &input,
            None,
            None,
            None,
            None,
            CRYPTPROTECT_UI_FORBIDDEN,
            &mut out,
        )
        .ok()?;
        let slice = std::slice::from_raw_parts_mut(out.pbData, out.cbData as usize);
        let key = slice.to_vec();
        slice.fill(0);
        let _ = LocalFree(Some(HLOCAL(out.pbData.cast())));
        Some(key)
    }
}

fn dpapi_protect(data: &[u8]) -> Option<Vec<u8>> {
    use windows::core::w;
    use windows::Win32::Foundation::{LocalFree, HLOCAL};
    use windows::Win32::Security::Cryptography::{
        CryptProtectData, CRYPTPROTECT_UI_FORBIDDEN, CRYPT_INTEGER_BLOB,
    };
    let input = CRYPT_INTEGER_BLOB {
        cbData: data.len() as u32,
        pbData: data.as_ptr().cast_mut(),
    };
    let mut out = CRYPT_INTEGER_BLOB::default();
    // SAFETY: `input` points at `data` for the call; `out` receives a LocalAlloc'ed buffer that is
    // copied and freed below.
    unsafe {
        CryptProtectData(
            &input,
            w!("NiwCam pairing"),
            None,
            None,
            None,
            CRYPTPROTECT_UI_FORBIDDEN,
            &mut out,
        )
        .ok()?;
        let blob = std::slice::from_raw_parts(out.pbData, out.cbData as usize).to_vec();
        let _ = LocalFree(Some(HLOCAL(out.pbData.cast())));
        Some(blob)
    }
}

/// The pairing key for this phone (DPAPI-protected file), empty when there is none.
pub fn load_pair_key(device_id: &str) -> Vec<u8> {
    let blob = std::fs::read(pair_file(device_id)).unwrap_or_default();
    if blob.is_empty() {
        return Vec::new();
    }
    dpapi_unprotect(&blob).unwrap_or_default()
}

pub fn save_pair_key(device_id: &str, key: &[u8]) {
    let Some(blob) = dpapi_protect(key) else {
        return;
    };
    let _ = std::fs::write(pair_file(device_id), blob);
}

pub fn has_pair_key(device_id: &str) -> bool {
    !device_id.is_empty() && std::fs::symlink_metadata(pair_file(device_id)).is_ok()
}

/// "Quên ghép đôi": the next connection pairs again (6-digit code on both sides).
pub fn forget_pair_key(device_id: &str) {
    if !device_id.is_empty() {
        let _ = std::fs::remove_file(pair_file(device_id));
    }
}

// ---- connection ---------------------------------------------------------------------------------

/// A connected, blocking socket to the phone app, or `NO_SOCKET` and a message for the user.
fn dial(endpoint: &Endpoint, error: &mut String) -> Socket {
    // A cable: the adb server or usbmuxd opens a tunnel to the app's port (docs/protocol.md 1.2).
    if endpoint.transport == Transport::Adb {
        return usb::open_adb(&endpoint.serial, endpoint.port, error);
    }
    if endpoint.transport == Transport::Usbmux {
        return usb::open_usbmux(&endpoint.serial, endpoint.port, error);
    }

    let addr = (endpoint.host.as_str(), endpoint.port)
        .to_socket_addrs()
        .ok()
        .and_then(|mut a| a.find(SocketAddr::is_ipv4));
    let Some(addr) = addr else {
        *error = format!("Địa chỉ không hợp lệ: {}", endpoint.host);
        return NO_SOCKET;
    };
    // Connect with a timeout: unreachable phones should not hang the app.
    match TcpStream::connect_timeout(&addr, Duration::from_secs(4)) {
        Ok(s) => sock::from_stream(s),
        Err(_) => {
            *error = format!(
                "Không kết nối được tới điện thoại {}:{}. Kiểm tra app NiwCam trên điện thoại đang mở và cùng mạng Wi\u{2011}Fi.",
                endpoint.host, endpoint.port
            );
            NO_SOCKET
        }
    }
}

struct SendState {
    up_key: Option<[u8; 32]>,
    counter: u64,
}

#[derive(Default)]
struct Info {
    device_id: String,
    device_name: String,
    os: String,
    caps: String,
}

/// One authenticated connection to a phone app's port (camera or shared screen).
///
/// Threads: one reader at a time holds the lease (`lease`/`try_lease`): the capture thread while it
/// streams this source, a screen viewer, `cs_load_formats` while it connects, or the background
/// connector while the link is idle. Only the lease holder connects, receives and closes.
/// `send_control` may run on any thread (the app's controls); `abort` too. Share a link as
/// `Arc<PhoneLink>`: every method takes `&self`.
pub struct PhoneLink {
    socket: AtomicUsize,
    aborted: AtomicBool,
    last_send: AtomicU64,
    /// last one `connect` was given
    endpoint: Mutex<Endpoint>,
    /// the send lock: keys and counter of the PC -> phone direction (`Close` waits for it)
    send: Mutex<SendState>,
    down_key: Mutex<Option<[u8; 32]>>,
    receive_counter: AtomicU64,
    error_code: Mutex<String>,
    /// device identity and caps, read from other threads. Also taken by `close` and `abort` so
    /// `abort` never uses a socket `close` is closing (the C++ `m_infoLock`).
    info: Mutex<Info>,
    /// 0 free, 1 the connector, 2 anyone else
    lease_state: Mutex<i32>,
    lease_free: Condvar,
    attempting: AtomicBool,
    preempted: AtomicBool,
    /// reused for the sealed body of each record
    scratch: Mutex<Vec<u8>>,
}

impl Default for PhoneLink {
    fn default() -> Self {
        PhoneLink {
            socket: AtomicUsize::new(NO_SOCKET),
            aborted: AtomicBool::new(false),
            last_send: AtomicU64::new(0),
            endpoint: Mutex::new(Endpoint::default()),
            send: Mutex::new(SendState {
                up_key: None,
                counter: 0,
            }),
            down_key: Mutex::new(None),
            receive_counter: AtomicU64::new(0),
            error_code: Mutex::new(String::new()),
            info: Mutex::new(Info::default()),
            lease_state: Mutex::new(0),
            lease_free: Condvar::new(),
            attempting: AtomicBool::new(false),
            preempted: AtomicBool::new(false),
            scratch: Mutex::new(Vec::new()),
        }
    }
}

impl Drop for PhoneLink {
    fn drop(&mut self) {
        self.close();
    }
}

fn to_key32(v: &[u8]) -> Option<[u8; 32]> {
    v.try_into().ok()
}

impl PhoneLink {
    pub fn new() -> Self {
        Self::default()
    }

    /// Connects, authenticates (pairing first if needed). Blocking; on failure the error is a message
    /// for the user (Vietnamese, UTF-8).
    pub fn connect(
        &self,
        endpoint: &Endpoint,
        on_pairing: Option<PairingCallback>,
        confirm: Option<PairingConfirm>,
    ) -> Result<(), String> {
        let mut first = String::new();
        if self.connect_impl(
            endpoint, on_pairing, confirm, true, false, &mut first, false,
        ) {
            return Ok(());
        }
        // Both sides have a key for each other, but not the same one (the phone app was reinstalled,
        // or one side was paired again elsewhere): the phone refused our auth. The user picked this
        // phone, so pair again now, with the 6-digit code on both sides.
        if *self.error_code.lk() != "pair" || self.aborted.load(Ordering::SeqCst) {
            return Err(first);
        }
        let mut second = String::new();
        if self.connect_impl(
            endpoint,
            on_pairing,
            confirm,
            true,
            false,
            &mut second,
            true,
        ) {
            Ok(())
        } else {
            Err(second)
        }
    }

    /// The background connector: hello says "auto", so a phone in use by another PC answers "busy"
    /// and keeps that PC (docs/protocol.md 3.1). Never pairs.
    pub fn connect_auto(&self, endpoint: &Endpoint) -> Result<(), String> {
        let mut e = String::new();
        if self.connect_impl(endpoint, None, None, false, true, &mut e, false) {
            Ok(())
        } else {
            Err(e)
        }
    }

    /// A connection the user asked for, to an already-paired phone: never pairs (a screen window).
    pub fn connect_known(&self, endpoint: &Endpoint) -> Result<(), String> {
        let mut e = String::new();
        if self.connect_impl(endpoint, None, None, false, false, &mut e, false) {
            Ok(())
        } else {
            Err(e)
        }
    }

    /// Connects again to the last endpoint (re-resolved by device id, the phone's IP may have
    /// changed). Only for an already-paired phone: never starts pairing, since nobody is there to
    /// compare codes.
    pub fn reconnect(&self) -> Result<(), String> {
        let last = self.endpoint.lk().clone();
        // A cable: the same phone (by serial) and port (camera or shared screen) on a cable again.
        // While it is unplugged, or no longer shares its screen, this fails and the engine keeps
        // waiting; it never moves to Wi-Fi by itself.
        let mut error = String::new();
        if last.usb() {
            return if self.connect_impl(&last, None, None, false, false, &mut error, false) {
                Ok(())
            } else {
                Err(error)
            };
        }

        let mut endpoint = last.clone();
        // Find it again by the TXT id it was found under: the iPhone's shared screen advertises
        // "<deviceId>:screen" (docs/protocol.md 1.1) while its hello carries the camera's deviceId,
        // so the hello id alone would bring a screen link back to the camera. A manual entry only
        // knows the hello id: it takes a discovered address on the same port only.
        let id = if last.device_id.is_empty() {
            self.device_id()
        } else {
            last.device_id.clone()
        };
        if !id.is_empty() {
            for e in known_phones() {
                if e.device_id == id && (!last.manual || e.port == last.port) {
                    endpoint = e;
                    break;
                }
            }
        }
        if endpoint.host.is_empty() {
            return Err("Chưa từng kết nối điện thoại này".to_string());
        }
        if self.connect_impl(&endpoint, None, None, false, false, &mut error, false) {
            Ok(())
        } else {
            Err(error)
        }
    }

    pub fn connected(&self) -> bool {
        self.socket.load(Ordering::SeqCst) != NO_SOCKET && !self.aborted.load(Ordering::SeqCst)
    }

    /// Why the last connect failed, for the connector: "busy" (another PC has the phone), "pair"
    /// (pairing needed), or "" (anything else).
    pub fn last_error_code(&self) -> String {
        self.error_code.lk().clone()
    }

    /// The endpoint the last `connect` was given.
    pub fn endpoint(&self) -> Endpoint {
        self.endpoint.lk().clone()
    }

    /// From the phone's hello; kept after the link closes. Any thread.
    pub fn device_id(&self) -> String {
        self.info.lk().device_id.clone()
    }

    pub fn device_name(&self) -> String {
        self.info.lk().device_name.clone()
    }

    /// `os` from the phone's hello ("ios" / "android").
    pub fn os(&self) -> String {
        self.info.lk().os.clone()
    }

    /// The phone's last caps (JSON text), kept across reconnects, so picking the source needs no
    /// round trip. Any thread.
    pub fn caps(&self) -> String {
        self.info.lk().caps.clone()
    }

    pub fn set_caps(&self, caps: &str) {
        self.info.lk().caps = caps.to_string();
    }

    /// `repair`: pair again although both sides have a key (they no longer match).
    #[allow(clippy::too_many_arguments)]
    fn connect_impl(
        &self,
        endpoint: &Endpoint,
        on_pairing: Option<PairingCallback>,
        confirm: Option<PairingConfirm>,
        allow_pairing: bool,
        automatic: bool,
        error: &mut String,
        repair: bool,
    ) -> bool {
        self.close();
        self.error_code.lk().clear();
        *self.endpoint.lk() = endpoint.clone();

        let s = dial(endpoint, error);
        if s == NO_SOCKET {
            return false;
        }
        sock::set_nodelay(s);
        sock::set_rcvbuf(s, 4 * 1024 * 1024);
        // A send never blocks for long (a dead peer with a full buffer): the caller may hold the
        // send lock that close() waits for.
        sock::set_send_timeout(s, 5000);
        if !endpoint.usb() {
            // An idle link to a phone that left the Wi-Fi is noticed within ~30 s (phones do not
            // answer "ping"): TCP keepalive after 10 s of silence, then every 2 s.
            sock::set_keepalive(s, 10000, 2000);
        }
        self.socket.store(s, Ordering::SeqCst);

        macro_rules! fail {
            ($msg:expr) => {{
                *error = $msg.to_string();
                self.close();
                return false;
            }};
        }
        if self.aborted.load(Ordering::SeqCst) {
            fail!("Đã hủy kết nối"); // abort() while dialling
        }

        // 1. hello with commitment to our ephemeral key (section 3.1). "auto": a connection the
        //    user did not ask for, which must not take the phone from another PC.
        let key = crypto::EcdhKey::generate();
        let client_id = client_id();
        let commit = crypto::sha256(&[key.public_key()]);
        let mut ours = Value::object();
        ours.set("type", "hello")
            .set("v", 1)
            .set("clientId", client_id.as_str())
            .set("clientName", client_name())
            .set("commit", crypto::base64(&commit));
        if automatic {
            ours.set("auto", true);
        }
        if !self.send_handshake(&ours) {
            fail!("Mất kết nối với điện thoại");
        }

        let mut hello = Value::Null;
        if !self.read_handshake(&mut hello, 8000, None) {
            // Over a cable a suspended iPhone app still accepts the connection, then never answers.
            fail!(if endpoint.usb() {
                "Điện thoại không trả lời qua cáp USB. Mở khóa điện thoại và mở app NiwCam."
            } else {
                "Điện thoại không trả lời đúng giao thức"
            });
        }
        if hello.get("type").str() == "error" {
            // In place of the phone's hello: "busy" (another PC has it, and this was an "auto" hello).
            if hello.get("code").str() == "busy" {
                *self.error_code.lk() = "busy".to_string();
            }
            let busy = *self.error_code.lk() == "busy";
            let fallback = if busy {
                "Điện thoại đang dùng với máy tính khác"
            } else {
                "Điện thoại từ chối kết nối"
            };
            let msg = hello.get("message").str_or(fallback);
            fail!(msg);
        }
        if hello.get("type").str() != "hello" {
            fail!("Điện thoại không trả lời đúng giao thức");
        }
        if hello.get("v").int(0) != 1 {
            fail!("Phiên bản app trên điện thoại không khớp với NiwCam trên PC");
        }
        let device_id = hello.get("deviceId").str().to_string();
        {
            let mut info = self.info.lk();
            info.device_id = device_id.clone();
            info.device_name = hello.get("name").str_or(&endpoint.name);
            info.os = hello.get("os").str().to_string();
        }
        let phone_pub = crypto::from_base64(hello.get("pub").str());
        if phone_pub.len() != 65 || device_id.is_empty() {
            fail!("Dữ liệu bắt tay không hợp lệ");
        }

        // 2. reveal our key, derive Z and the transcript
        let mut reveal = Value::object();
        reveal
            .set("type", "reveal")
            .set("pub", crypto::base64(key.public_key()));
        if !self.send_handshake(&reveal) {
            fail!("Mất kết nối với điện thoại");
        }
        let (z, t) = match key.agree(&phone_pub) {
            Ok(z) => (
                z,
                crypto::transcript(&client_id, &device_id, key.public_key(), &phone_pub),
            ),
            Err(_) => fail!("Khóa của điện thoại không hợp lệ"),
        };

        // 3. pairing (3.2) or straight to authentication (3.3)
        let mut k_pair = load_pair_key(&device_id);
        let mut new_pairing = false;
        if k_pair.is_empty() || !hello.get("paired").bool_or(false) || repair {
            if !allow_pairing {
                *self.error_code.lk() = "pair".to_string();
                // Picking the phone then connects and pairs (cs_load_formats goes by the caps).
                self.info.lk().caps.clear();
                fail!("Điện thoại cần ghép đôi lại: chọn lại điện thoại trong danh sách camera");
            }
            // The old key, if any, stays until a new pairing succeeds.
            let mut start = Value::object();
            start.set("type", "pairStart");
            if !self.send_handshake(&start) {
                fail!("Mất kết nối với điện thoại");
            }
            if let Some(cb) = on_pairing {
                cb(&crypto::sas(&t));
            }
            let mut reply = Value::Null;
            let got = self.read_handshake(&mut reply, 125000, None);
            macro_rules! done {
                ($msg:expr) => {{
                    if let Some(cb) = on_pairing {
                        cb("");
                    }
                    fail!($msg);
                }};
            }
            if !got {
                done!("Hết thời gian chờ xác nhận ghép đôi trên điện thoại");
            }
            if reply.get("type").str() == "error" {
                done!(reply.get("message").str_or("Điện thoại từ chối ghép đôi"));
            }
            if reply.get("type").str() != "pair" || !reply.get("accepted").bool_or(false) {
                done!("Đã từ chối ghép đôi trên điện thoại");
            }
            // The PC user must agree too (protocol 3.2).
            if !confirm.is_some_and(|c| c()) {
                done!("Đã từ chối ghép đôi trên máy tính");
            }
            if let Some(cb) = on_pairing {
                cb("");
            }
            k_pair = crypto::pair_key(&z, &t);
            new_pairing = true;
        }

        let mut auth_msg = Value::object();
        auth_msg
            .set("type", "auth")
            .set("mac", crypto::base64(&crypto::pc_mac(&k_pair, &t)));
        if !self.send_handshake(&auth_msg) {
            fail!("Mất kết nối với điện thoại");
        }
        let mut auth = Value::Null;
        let mut auth_timed_out = false;
        if !self.read_handshake(&mut auth, 8000, Some(&mut auth_timed_out)) {
            if new_pairing {
                fail!("Ghép đôi thất bại");
            }
            // A timeout or a mid-read network drop: just retry, do not force re-pairing.
            if auth_timed_out {
                fail!("Điện thoại không trả lời khi xác thực. Thử lại.");
            }
            // A clean close right after our auth: the phone has a key for this PC too, but a
            // different one, so it closed on our MAC (docs/protocol.md 3.3). Only a new pairing
            // mends that: the list says "Cần ghép đôi" and picking the phone pairs again (connect
            // retries with repair).
            *self.error_code.lk() = "pair".to_string();
            self.info.lk().caps.clear();
            fail!("Điện thoại không nhận ra máy tính này. Chọn điện thoại để ghép đôi lại.");
        }
        if auth.get("type").str() != "auth" {
            fail!(if new_pairing {
                "Ghép đôi thất bại"
            } else {
                "Điện thoại trả lời sai giao thức khi xác thực"
            });
        }
        if !crypto::constant_time_equal(
            &crypto::from_base64(auth.get("mac").str()),
            &crypto::phone_mac(&k_pair, &t),
        ) {
            fail!("Điện thoại xác thực sai: có thể không phải thiết bị đã ghép đôi");
        }
        if new_pairing {
            save_pair_key(&device_id, &k_pair);
        }

        // 4. session keys (under the send lock: the app's controls may send from another thread)
        let mut send = self.send.lk();
        send.up_key = to_key32(&crypto::up_key(&z, &k_pair, &t));
        *self.down_key.lk() = to_key32(&crypto::down_key(&z, &k_pair, &t));
        send.counter = 0;
        self.receive_counter.store(0, Ordering::SeqCst);
        true
    }

    /// Lease holder: closes the socket.
    pub fn close(&self) {
        let mut send = self.send.lk(); // no send in progress on another thread
        {
            let _info = self.info.lk(); // no abort() using the socket
            let s = self.socket.load(Ordering::SeqCst);
            if s != NO_SOCKET {
                sock::close(s);
                self.socket.store(NO_SOCKET, Ordering::SeqCst);
            }
            self.aborted.store(false, Ordering::SeqCst);
        }
        send.up_key = None;
        *self.down_key.lk() = None;
    }

    /// Any thread: ends the connection without closing the socket under its reader (shutdown). The
    /// reader wakes with an error and closes. Also cuts short a connect in progress.
    pub fn abort(&self) {
        // Not under the send lock: a send may be blocked right now. shutdown() wakes the reader's
        // select/recv with an error, and the reader (the lease holder) closes the socket.
        let _info = self.info.lk();
        let s = self.socket.load(Ordering::SeqCst);
        if s != NO_SOCKET {
            sock::shutdown(s);
        }
        self.aborted.store(true, Ordering::SeqCst);
    }

    /// Something can be read now (a record, or the connection closing).
    pub fn readable(&self) -> bool {
        let s = self.socket.load(Ordering::SeqCst);
        if s == NO_SOCKET {
            return false;
        }
        sock::select_readable(s, 0) > 0
    }

    /// Bytes received and not yet read: more of the stream already waits behind the current record.
    pub fn pending_bytes(&self) -> usize {
        let s = self.socket.load(Ordering::SeqCst);
        if s == NO_SOCKET {
            return 0;
        }
        sock::pending_bytes(s)
    }

    /// `GetTickCount64()` of the last record sent (the connector pings idle links).
    pub fn last_send_tick(&self) -> u64 {
        self.last_send.load(Ordering::SeqCst)
    }

    // ---- lease ----------------------------------------------------------------------------------

    /// The connector's `try_lease(true)` gives way: while it marks an attempt (`set_attempting`:
    /// connecting, asking for caps), a `lease()` from anyone else aborts that attempt, and
    /// `preempted()` then says so (not a failure). Reading an idle link is quick and is waited for,
    /// so the idle connection survives.
    pub fn try_lease(&self, background: bool) -> bool {
        let mut st = self.lease_state.lk();
        if *st != 0 {
            return false;
        }
        *st = if background { 1 } else { 2 };
        if background {
            self.preempted.store(false, Ordering::SeqCst);
        }
        true
    }

    pub fn lease(&self, timeout_ms: i32) -> bool {
        let mut st = self.lease_state.lk();
        let deadline = Instant::now() + Duration::from_millis(timeout_ms.max(0) as u64);
        while *st != 0 {
            if *st == 1 && self.attempting.load(Ordering::SeqCst) {
                // The connector is connecting: cut it short (we connect ourselves). Again on every
                // wake: an abort() landing just before its connect resets is lost.
                self.preempted.store(true, Ordering::SeqCst);
                drop(st);
                self.abort();
                st = self.lease_state.lk();
                if *st == 0 {
                    break;
                }
            }
            // Woken by unlease, or every 100 ms to see whether an attempt started meanwhile.
            let now = Instant::now();
            let wake = deadline.min(now + Duration::from_millis(100));
            let (g, _) = self
                .lease_free
                .wait_timeout(st, wake.saturating_duration_since(now))
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            st = g;
            if *st != 0 && Instant::now() >= deadline {
                return false;
            }
        }
        *st = 2;
        true
    }

    pub fn unlease(&self) {
        {
            *self.lease_state.lk() = 0;
        }
        self.lease_free.notify_all();
    }

    pub fn set_attempting(&self, on: bool) {
        self.attempting.store(on, Ordering::SeqCst);
    }

    pub fn preempted(&self) -> bool {
        self.preempted.load(Ordering::SeqCst)
    }

    /// Held by someone other than the connector (streaming, a viewer, pairing).
    pub fn in_use(&self) -> bool {
        *self.lease_state.lk() == 2
    }

    // ---- records --------------------------------------------------------------------------------

    fn send_record(&self, rtype: u8, body: &[u8]) -> bool {
        let socket = self.socket.load(Ordering::SeqCst);
        if socket == NO_SOCKET || self.aborted.load(Ordering::SeqCst) {
            return false;
        }
        let mut buf = Vec::with_capacity(5 + body.len());
        buf.extend_from_slice(&(1 + body.len() as u32).to_be_bytes());
        buf.push(rtype);
        buf.extend_from_slice(body);
        if !sock::send_all(socket, &buf) {
            return false;
        }
        self.last_send.store(tick_ms(), Ordering::SeqCst);
        true
    }

    /// Reads exactly `dst.len()` bytes. A timeout with nothing read is a plain timeout; part of a
    /// record read gives the rest 10 s before calling the connection dead.
    fn read_exact(&self, dst: &mut [u8], timeout_ms: i32, timed_out: &mut bool) -> bool {
        *timed_out = false;
        let mut deadline = tick_ms() + timeout_ms as u64;
        let mut extended = false;
        let mut got = 0;
        while got < dst.len() {
            let now = tick_ms();
            if now >= deadline {
                // Nothing arrived: a plain timeout. Part of a record arrived:
                // give the rest 10 s before calling the connection dead.
                if got == 0 {
                    *timed_out = true;
                    return false;
                }
                if extended {
                    return false;
                }
                extended = true;
                deadline = now + 10000;
                continue;
            }
            let socket = self.socket.load(Ordering::SeqCst);
            if socket == NO_SOCKET {
                return false;
            }
            let ready = sock::select_readable(socket, deadline - now);
            if ready == 0 {
                continue; // re-checked against the deadline above
            }
            if ready < 0 {
                return false;
            }
            let n = sock::recv(socket, &mut dst[got..]);
            if n <= 0 {
                return false;
            }
            got += n as usize;
        }
        true
    }

    fn send_handshake(&self, v: &Value) -> bool {
        let text = v.dump();
        self.send_record(HANDSHAKE, text.as_bytes())
    }

    fn read_handshake(&self, v: &mut Value, timeout_ms: i32, timed_out: Option<&mut bool>) -> bool {
        let mut sink = false;
        let timed_out = timed_out.unwrap_or(&mut sink);
        *timed_out = false;
        if self.socket.load(Ordering::SeqCst) == NO_SOCKET {
            return false;
        }
        let mut head = [0u8; 5];
        let mut to = false;
        if !self.read_exact(&mut head, timeout_ms, &mut to) {
            *timed_out = to;
            return false;
        }
        let length = u32::from_be_bytes([head[0], head[1], head[2], head[3]]);
        if !(1..=MAX_HANDSHAKE).contains(&length) || head[4] != HANDSHAKE {
            return false;
        }
        let mut body = vec![0u8; (length - 1) as usize];
        if !self.read_exact(&mut body, 8000, &mut to) {
            *timed_out = to;
            return false;
        }
        match Value::parse_bytes(&body) {
            Ok(parsed) => {
                *v = parsed;
                v.is_object()
            }
            Err(_) => false,
        }
    }

    /// Sends one CONTROL record (JSON). Any thread.
    pub fn send_control(&self, message: &Value) -> bool {
        let mut send = self.send.lk();
        let Some(key) = send.up_key else {
            return false;
        };
        let text = message.dump();
        let mut plain = vec![0u8; 13 + text.len()];
        plain[0] = Kind::Control.as_u8();
        plain[13..].copy_from_slice(text.as_bytes());
        let counter = send.counter;
        send.counter += 1;
        let Ok(sealed) = crypto::seal(&key, crypto::DIR_UP, counter, &plain) else {
            return false;
        };
        let mut body = Vec::with_capacity(8 + sealed.len());
        body.extend_from_slice(&counter.to_be_bytes());
        body.extend_from_slice(&sealed);
        if self.send_record(SEALED, &body) {
            return true;
        }
        // Dead connection: the reader notices, closes, and the next use reconnects. Not close()
        // here: this may be another thread than the one reading (the app's controls).
        self.abort();
        false
    }

    /// Waits up to `timeout_ms` for one record (lease holder). `Recv::TimedOut` when nothing came;
    /// `Recv::Failed` when the connection is gone or tampered with.
    pub fn receive(&self, message: &mut Message, timeout_ms: i32) -> Recv {
        let Some(key) = *self.down_key.lk() else {
            return Recv::Failed;
        };
        if self.socket.load(Ordering::SeqCst) == NO_SOCKET {
            return Recv::Failed;
        }
        if self.aborted.load(Ordering::SeqCst) {
            self.close();
            return Recv::Failed;
        }
        // Any failure other than a plain timeout ends the connection, so connected() turns false
        // and the next use reconnects.
        let broken = || {
            self.close();
            Recv::Failed
        };
        let mut head = [0u8; 13];
        let mut timed_out = false;
        if !self.read_exact(&mut head, timeout_ms, &mut timed_out) {
            return if timed_out { Recv::TimedOut } else { broken() };
        }
        let length = u32::from_be_bytes([head[0], head[1], head[2], head[3]]);
        if head[4] != SEALED || !(1 + 8 + 16..=MAX_SEALED).contains(&length) {
            return broken();
        }
        let counter = u64::from_be_bytes([
            head[5], head[6], head[7], head[8], head[9], head[10], head[11], head[12],
        ]);
        if counter != self.receive_counter.load(Ordering::SeqCst) {
            return broken(); // replayed or reordered
        }
        let mut sealed = self.scratch.lk();
        sealed.clear();
        sealed.resize((length - 9) as usize, 0);
        let mut t2 = false;
        if !self.read_exact(&mut sealed, 10000, &mut t2) {
            drop(sealed);
            return broken();
        }
        let plain = crypto::open(&key, crypto::DIR_DOWN, counter, &sealed);
        drop(sealed);
        let Some(plain) = plain else {
            return broken();
        };
        if plain.len() < 13 {
            return broken();
        }
        self.receive_counter.fetch_add(1, Ordering::SeqCst);
        message.kind = Kind::from_u8(plain[0]);
        message.pts_us = u64::from_be_bytes([
            plain[1], plain[2], plain[3], plain[4], plain[5], plain[6], plain[7], plain[8],
        ]) as i64;
        message.flags = u32::from_be_bytes([plain[9], plain[10], plain[11], plain[12]]);
        message.payload.clear();
        message.payload.extend_from_slice(&plain[13..]);
        Recv::Message
    }

    /// Asks for caps and waits for them (other control messages are skipped). Generous: the phone
    /// may test-open hidden cameras before answering. The C++ default timeout is 20000 ms.
    pub fn request_caps(&self, timeout_ms: i32) -> Result<Value, String> {
        let mut get = Value::object();
        get.set("type", "getCaps");
        if !self.send_control(&get) {
            return Err("Mất kết nối với điện thoại".to_string());
        }
        let deadline = tick_ms() + timeout_ms as u64;
        while tick_ms() < deadline {
            let mut m = Message::default();
            match self.receive(&mut m, 1000) {
                Recv::Message => {}
                Recv::TimedOut => continue,
                Recv::Failed => return Err("Mất kết nối với điện thoại".to_string()),
            }
            if m.kind != Kind::Control {
                continue;
            }
            if let Ok(v) = m.json() {
                if v.get("type").str() == "caps" {
                    self.set_caps(&v.dump());
                    return Ok(v);
                }
            }
        }
        Err("Điện thoại không gửi thông tin camera".to_string())
    }
}

/// `request_caps` with the C++ default timeout.
pub const REQUEST_CAPS_DEFAULT_MS: i32 = 20000;

/// Holds a link's lease for a scope (taken with `lease`/`try_lease` beforehand).
pub struct LeaseGuard<'a> {
    link: Option<&'a PhoneLink>,
}

impl<'a> LeaseGuard<'a> {
    pub fn new(link: &'a PhoneLink) -> Self {
        LeaseGuard { link: Some(link) }
    }
}

impl Drop for LeaseGuard<'_> {
    fn drop(&mut self) {
        if let Some(l) = self.link {
            l.unlease();
        }
    }
}
