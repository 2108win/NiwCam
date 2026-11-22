//! Finds NiwCam phones on the home network (DNS-SD `_camstudio._tcp`, docs/protocol.md section 1),
//! remembers phones added by IP, and lists phones on a USB cable (adb, usbmuxd: 1.2).
//! Port of `src/engine/Discovery.{h,cpp}`.

use std::collections::BTreeMap;
use std::ffi::c_void;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::{Arc, Mutex};

use windows::core::{PCWSTR, PWSTR};
use windows::Win32::Foundation::DNS_REQUEST_PENDING;
use windows::Win32::NetworkManagement::Dns::{
    DnsFree, DnsFreeRecordList, DnsServiceBrowse, DnsServiceBrowseCancel, DnsServiceFreeInstance,
    DnsServiceResolve, DNS_QUERY_REQUEST_VERSION1, DNS_RECORDW, DNS_SERVICE_BROWSE_REQUEST,
    DNS_SERVICE_BROWSE_REQUEST_0, DNS_SERVICE_CANCEL, DNS_SERVICE_INSTANCE,
    DNS_SERVICE_RESOLVE_REQUEST, DNS_TYPE_PTR,
};

use crate::appdata::app_data_dir;
use crate::usb_transport as usb;
use crate::util::LockExt;

/// How the PC reaches a phone: TCP over the network, or a tunnel through a USB cable.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum Transport {
    #[default]
    Network,
    /// Android: through the adb server
    Adb,
    /// iPhone: through usbmuxd (Apple Mobile Device Service)
    Usbmux,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Endpoint {
    /// IPv4 address (network only)
    pub host: String,
    /// the phone app's port (USB: the fixed 47321)
    pub port: u16,
    /// TXT id (empty for manual entries until connected)
    pub device_id: String,
    /// UTF-8, TXT name or service instance name
    pub name: String,
    pub model: String,
    /// ios | android
    pub os: String,
    /// added by IP
    pub manual: bool,
    pub transport: Transport,
    /// USB: adb serial or the iPhone's UDID
    pub serial: String,
    /// the phone's shared screen (TXT src=screen, or the cable's port 47322)
    pub screen: bool,
    /// reachable now (false: Android on a cable waiting for "allow USB debugging")
    pub ready: bool,
}

impl Default for Endpoint {
    fn default() -> Self {
        Endpoint {
            host: String::new(),
            port: 0,
            device_id: String::new(),
            name: String::new(),
            model: String::new(),
            os: String::new(),
            manual: false,
            transport: Transport::Network,
            serial: String::new(),
            screen: false,
            ready: true,
        }
    }
}

impl Endpoint {
    pub fn usb(&self) -> bool {
        self.transport != Transport::Network
    }

    /// A manual entry: `{host, port, "", host, "", "", true}` in the C++.
    fn manual_entry(host: &str, port: u16) -> Endpoint {
        Endpoint {
            host: host.to_string(),
            port,
            name: host.to_string(),
            manual: true,
            ..Endpoint::default()
        }
    }
}

struct CancelHandle(DNS_SERVICE_CANCEL);
// SAFETY: the handle is an opaque token that DnsServiceBrowseCancel may be given from any thread.
unsafe impl Send for CancelHandle {}

#[derive(Default)]
struct State {
    /// by service instance name (UTF-16, like the C++ `std::wstring` ordering)
    found: BTreeMap<Vec<u16>, Endpoint>,
    manual: Vec<Endpoint>,
    browse_cancel: Option<CancelHandle>,
    browsing: bool,
    manual_loaded: bool,
    /// Its threads are joined in `stop_discovery`, never by a static destructor while the DLL unloads.
    usb: Option<Arc<usb::Watcher>>,
    on_change: Option<Arc<dyn Fn() + Send + Sync>>,
}

fn state() -> &'static Mutex<State> {
    static STATE: std::sync::OnceLock<Mutex<State>> = std::sync::OnceLock::new();
    STATE.get_or_init(|| Mutex::new(State::default()))
}

fn changed() {
    let f = state().lk().on_change.clone();
    if let Some(f) = f {
        f();
    }
}

fn narrow(w: PWSTR) -> String {
    if w.is_null() {
        return String::new();
    }
    // SAFETY: a non-null PWSTR from the DNS API is NUL terminated.
    let slice = unsafe { w.as_wide() };
    String::from_utf16_lossy(slice)
}

/// What `DnsServiceResolve` calls back with: the instance name it was asked about (no NUL, the
/// map key) and the same with a NUL for the request.
struct ResolveContext {
    key: Vec<u16>,
    query: Vec<u16>,
}

unsafe extern "system" fn on_resolved(
    status: u32,
    context: *const c_void,
    instance: *const DNS_SERVICE_INSTANCE,
) {
    // A panic must never cross into the DNS thread.
    let _ = catch_unwind(AssertUnwindSafe(|| {
        // SAFETY: `context` is the Box<ResolveContext> leaked in `resolve`; this callback runs once.
        let ctx = unsafe { Box::from_raw(context as *mut ResolveContext) };
        // SAFETY: when non-null, `instance` is a valid DNS_SERVICE_INSTANCE until freed below.
        let inst = unsafe { instance.as_ref() };
        if status == 0 {
            if let Some(inst) = inst {
                if !inst.ip4Address.is_null() {
                    // SAFETY: ip4Address is non-null, pointing at an IN_ADDR (4 bytes, network order).
                    let raw = unsafe { *inst.ip4Address };
                    let octets = raw.to_ne_bytes();
                    let mut e = Endpoint {
                        host: std::net::Ipv4Addr::from(octets).to_string(),
                        port: inst.wPort,
                        name: narrow(inst.pszInstanceName),
                        ..Endpoint::default()
                    };
                    if let Some(dot) = e.name.find("._camstudio") {
                        e.name.truncate(dot);
                    }
                    for i in 0..inst.dwPropertyCount as usize {
                        // SAFETY: keys/values are arrays of dwPropertyCount PWSTR.
                        let (kp, vp) = unsafe { (*inst.keys.add(i), *inst.values.add(i)) };
                        let k = if kp.is_null() {
                            String::new()
                        } else {
                            narrow(kp)
                        };
                        let v = narrow(vp);
                        match k.as_str() {
                            "id" => e.device_id = v,
                            "name" if !v.is_empty() => e.name = v,
                            "model" => e.model = v,
                            "os" => e.os = v,
                            "src" => e.screen = v == "screen",
                            _ => {}
                        }
                    }
                    let was_changed;
                    {
                        let mut st = state().lk();
                        // Phones re-announce themselves: only a new or moved phone is a change.
                        was_changed = match st.found.get(&ctx.key) {
                            None => true,
                            Some(old) => {
                                old.host != e.host
                                    || old.port != e.port
                                    || old.name != e.name
                                    || old.device_id != e.device_id
                            }
                        };
                        st.found.insert(ctx.key.clone(), e);
                    }
                    if was_changed {
                        changed();
                    }
                }
            }
        }
        if inst.is_some() {
            // SAFETY: the instance came from the DNS API and is freed exactly once.
            unsafe { DnsServiceFreeInstance(instance) };
        }
    }));
}

fn resolve(instance_name: &[u16]) {
    let mut query: Vec<u16> = instance_name.to_vec();
    query.push(0);
    let ctx = Box::into_raw(Box::new(ResolveContext {
        key: instance_name.to_vec(),
        query,
    }));
    // SAFETY: `ctx` stays alive until `on_resolved` (or the failure branch below) frees it.
    let query_ptr = unsafe { (*ctx).query.as_mut_ptr() };
    let req = DNS_SERVICE_RESOLVE_REQUEST {
        Version: DNS_QUERY_REQUEST_VERSION1.0,
        InterfaceIndex: 0,
        QueryName: PWSTR(query_ptr),
        pResolveCompletionCallback: Some(on_resolved),
        pQueryContext: ctx.cast(),
    };
    let mut cancel = DNS_SERVICE_CANCEL::default();
    // SAFETY: `req` and `cancel` are valid for the call; the context is owned by the callback.
    let r = unsafe { DnsServiceResolve(&req, &mut cancel) };
    if r != DNS_REQUEST_PENDING {
        // SAFETY: the API did not take the request, so the callback will not run: free the context.
        drop(unsafe { Box::from_raw(ctx) });
    }
}

unsafe extern "system" fn on_browse(
    status: u32,
    _context: *const c_void,
    records: *const DNS_RECORDW,
) {
    let _ = catch_unwind(AssertUnwindSafe(|| {
        if status == 0 {
            let mut r = records;
            while !r.is_null() {
                // SAFETY: the records form a valid linked list until DnsFree below.
                let rec = unsafe { &*r };
                r = rec.pNext;
                if rec.wType != DNS_TYPE_PTR.0 {
                    continue;
                }
                // SAFETY: wType is PTR, so the PTR member of the data union is the active one.
                let host = unsafe { rec.Data.PTR.pNameHost };
                if host.is_null() {
                    continue;
                }
                // SAFETY: a non-null name host is NUL terminated.
                let instance: Vec<u16> = unsafe { host.as_wide() }.to_vec();
                if rec.dwTtl == 0 {
                    // Goodbye packet: the phone app closed.
                    let erased = state().lk().found.remove(&instance).is_some();
                    if erased {
                        changed();
                    }
                } else {
                    resolve(&instance);
                }
            }
        }
        if !records.is_null() {
            // SAFETY: the list came from the DNS API; DnsRecordListFree is DnsFree(.., DnsFreeRecordList).
            unsafe { DnsFree(Some(records.cast()), DnsFreeRecordList) };
        }
    }));
}

fn manual_file() -> std::path::PathBuf {
    let dir = app_data_dir();
    let _ = std::fs::create_dir(&dir);
    dir.join("phones.txt")
}

fn load_manual_locked(st: &mut State) {
    if st.manual_loaded {
        return;
    }
    st.manual_loaded = true;
    // `while (f >> host >> port)`: whitespace separated tokens, a bad number ends the read.
    let text = std::fs::read(manual_file()).unwrap_or_default();
    let text = String::from_utf8_lossy(&text).into_owned();
    let mut tokens = text.split_whitespace();
    while let (Some(host), Some(port)) = (tokens.next(), tokens.next()) {
        let Ok(port) = port.parse::<i64>() else {
            break;
        };
        // `unsigned port` in the C++: a negative number wraps and is rejected by the range test.
        let port = port as u32;
        if port > 0 && port < 65536 {
            st.manual.push(Endpoint::manual_entry(host, port as u16));
        }
    }
}

/// Starts browsing in the background (idempotent), with the USB watcher (adb, usbmuxd) like the C++.
pub fn start_discovery() {
    start_discovery_with(true, true);
}

/// `start_discovery` with the pieces switchable, for tools and tests: `usb` starts the cable
/// watcher at all; `adb_starter` lets it start an adb server (`usb::start_adb_server`, which the
/// C++ always passes). Demos use `(false, false)` so nothing on the PC is started.
pub fn start_discovery_with(usb_watcher: bool, adb_starter: bool) {
    let mut st = state().lk();
    if st.browsing {
        return;
    }
    let query: Vec<u16> = "_camstudio._tcp.local\0".encode_utf16().collect();
    let req = DNS_SERVICE_BROWSE_REQUEST {
        Version: DNS_QUERY_REQUEST_VERSION1.0,
        InterfaceIndex: 0,
        QueryName: PCWSTR(query.as_ptr()),
        Anonymous: DNS_SERVICE_BROWSE_REQUEST_0 {
            pBrowseCallback: Some(on_browse),
        },
        pQueryContext: std::ptr::null_mut(),
    };
    let mut cancel = DNS_SERVICE_CANCEL::default();
    // SAFETY: `req` and the query string are valid for the call (the API copies the name).
    let r = unsafe { DnsServiceBrowse(&req, &mut cancel) };
    st.browsing = r == DNS_REQUEST_PENDING;
    st.browse_cancel = Some(CancelHandle(cancel));
    if usb_watcher && st.usb.is_none() {
        let w = Arc::new(usb::Watcher::new());
        if adb_starter {
            w.start(
                Some(Arc::new(usb::start_adb_server)),
                Some(Arc::new(usb::stop_idle_adb_server)),
            );
        } else {
            w.start(None, None);
        }
        st.usb = Some(w);
    }
}

pub fn stop_discovery() {
    let watcher;
    {
        let mut st = state().lk();
        if st.browsing {
            if let Some(c) = &st.browse_cancel {
                // SAFETY: the handle was filled in by DnsServiceBrowse.
                unsafe { DnsServiceBrowseCancel(&c.0) };
            }
        }
        st.browsing = false;
        watcher = st.usb.take();
    }
    if let Some(w) = watcher {
        w.stop();
    }
    usb::stop_adb_server(); // only if we started it
}

/// Phones currently visible on the network, manual ones, then phones on a USB cable. Cheap: the USB
/// list is kept current in the background, nothing is asked here.
pub fn known_phones() -> Vec<Endpoint> {
    let mut st = state().lk();
    load_manual_locked(&mut st);
    let mut out: Vec<Endpoint> = st.found.values().cloned().collect();
    for m in &st.manual {
        // A manual entry that discovery also found shows once.
        let dup = out.iter().any(|e| e.host == m.host && e.port == m.port);
        if !dup {
            out.push(m.clone());
        }
    }

    // Phones on a USB cable (docs/protocol.md 1.2). They have no TXT record: the name comes from the
    // phone (Android device_name, the iPhone's name) and the device id from "hello".
    let mut cable: Vec<Endpoint> = Vec::new();
    let mut ready: Vec<bool> = Vec::new(); // the app can be reached (adb: USB debugging allowed)
    let devices = st.usb.as_ref().map(|w| w.devices()).unwrap_or_default();
    for d in &devices {
        let adb = d.kind == usb::Kind::Adb;
        let mut e = Endpoint {
            transport: if adb {
                Transport::Adb
            } else {
                Transport::Usbmux
            },
            serial: d.serial.clone(),
            port: usb::CAMERA_PORT,
            os: if adb { "android" } else { "ios" }.to_string(),
            model: d.model.clone(),
            ready: d.state == "device",
            ..Endpoint::default()
        };
        e.name = if !e.ready {
            "Điện thoại Android – chờ cho phép gỡ lỗi USB".to_string()
        } else if !d.name.is_empty() {
            d.name.clone()
        } else if adb {
            "Điện thoại Android".to_string()
        } else {
            "iPhone".to_string()
        };
        cable.push(e);
        ready.push(d.state == "device");
    }
    // Two phones with the same name: the app tells cameras apart by name, so add the serial's end.
    let mut twin = vec![false; cable.len()];
    for i in 0..cable.len() {
        for j in 0..cable.len() {
            if i != j && cable[i].name == cable[j].name {
                twin[i] = true;
            }
        }
    }
    for i in 0..cable.len() {
        let mut camera = cable[i].clone();
        if twin[i] {
            // `serial.substr(size > 4 ? size - 4 : 0)`: bytes in C++, characters here
            let chars: Vec<char> = camera.serial.chars().collect();
            let from = if chars.len() > 4 { chars.len() - 4 } else { 0 };
            camera.name.push_str(" · ");
            camera.name.extend(&chars[from..]);
        }
        out.push(camera.clone());
        // The shared screen (docs/protocol.md 1.1) through the same cable, named like the Wi-Fi one.
        // Listed for every phone ready on the cable: whether it is sharing is not checked, because a
        // test connection makes older phone apps drop the session in progress. Picking it while the
        // phone does not share its screen says so.
        if !ready[i] {
            continue;
        }
        let mut screen = camera;
        screen.port = usb::SCREEN_PORT;
        screen.screen = true;
        screen.name.push_str(" · Màn hình");
        out.push(screen);
    }
    out
}

/// "Add by IP": remembered for this run and in `<data dir>\phones.txt`.
pub fn add_manual(host: &str, port: u16) {
    {
        let mut st = state().lk();
        load_manual_locked(&mut st);
        if st.manual.iter().any(|m| m.host == host && m.port == port) {
            return;
        }
        st.manual.push(Endpoint::manual_entry(host, port));
        use std::io::Write;
        if let Ok(mut f) = std::fs::OpenOptions::new()
            .append(true)
            .create(true)
            .open(manual_file())
        {
            let _ = writeln!(f, "{host} {port}");
        }
    }
    changed();
}

pub fn remove_manual(host: &str, port: u16) {
    {
        let mut st = state().lk();
        load_manual_locked(&mut st);
        let before = st.manual.len();
        st.manual.retain(|m| !(m.host == host && m.port == port));
        if st.manual.len() == before {
            return;
        }
        let mut text = String::new();
        for m in &st.manual {
            text.push_str(&format!("{} {}\n", m.host, m.port));
        }
        let _ = std::fs::write(manual_file(), text);
    }
    changed();
}

/// Called (on a discovery thread, keep it short) when a phone appears, changes or leaves the
/// network list. Cable changes are not reported: the connector looks every 3 s anyway.
pub fn set_on_change(on_change: Option<Arc<dyn Fn() + Send + Sync>>) {
    state().lk().on_change = on_change;
}
