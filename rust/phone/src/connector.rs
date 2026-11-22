//! Keeps every paired phone connected in the background, so picking a phone (or switching to it
//! while an app uses the virtual camera) starts streaming at once: no handshake, caps known.
//! Port of `src/engine/Connector.{h,cpp}`.
//!
//! One thread, woken every 3 s and when the network list changes. It never runs on the UI thread.
//! For each phone it may use (Wi-Fi with a saved pairing key, or on a cable), it connects with
//! hello "auto" (a phone in use by another PC answers "busy" and keeps that PC), asks for caps
//! once, then keeps the idle link alive: reads what the phone sends and pings every 15 s
//! (docs/protocol.md 3.1, 4.1). A failure, "busy" or a dropped link waits 30 s before the next try.
//! While anyone else uses a link (streaming, a screen window, pairing) it leaves it alone.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

use niwcam_core::json::Value;

use crate::discovery::{known_phones, Endpoint};
use crate::phone_link::{
    load_pair_key, Kind, LeaseGuard, Message, PhoneLink, Recv, REQUEST_CAPS_DEFAULT_MS,
};
use crate::util::{tick_ms, LockExt};

const TICK: Duration = Duration::from_secs(3);
/// after a failure or "busy"
const RETRY_MS: u64 = 30000;
/// the phone wants pairing: only the user can do that
const PAIR_RETRY_MS: u64 = 300000;
/// docs/protocol.md 4.1
const PING_MS: u64 = 15000;
/// a link up this long resets the drop count
const STEADY_MS: u64 = 300000;

/// A phone source's connection, as the camera menu shows it (`CsLinkState` in EngineApi.h).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
#[repr(i32)]
pub enum LinkState {
    /// not connected, nothing to try (yet)
    #[default]
    Idle = 0,
    /// connected (idle and ready, or in use)
    Connected = 1,
    Connecting = 2,
    /// never paired with this PC (or the phone forgot it): pick it to pair
    NeedsPairing = 3,
    /// another PC has the phone
    Busy = 4,
    /// the last try failed (error says why); tried again later
    Unreachable = 5,
    /// the same phone is connected another way (Wi-Fi / cable)
    Elsewhere = 6,
}

/// "<deviceId>:screen" (the shared screen's TXT id) -> "<deviceId>": the pairing key's id.
fn base_id(id: &str) -> String {
    const SUFFIX: &str = ":screen";
    if id.len() > SUFFIX.len() && id.ends_with(SUFFIX) {
        id[..id.len() - SUFFIX.len()].to_string()
    } else {
        id.to_string()
    }
}

/// The phone server behind an endpoint: its device id and which server (camera or screen). Two
/// endpoints with the same identity reach the same server (over Wi-Fi and over a cable), and the
/// phone serves one connection per PC there. Empty while unknown (a cable phone never connected:
/// no TXT record, the id comes from hello).
fn identity(e: &Endpoint, link: &PhoneLink) -> String {
    let id = if e.device_id.is_empty() {
        link.device_id()
    } else {
        base_id(&e.device_id)
    };
    if id.is_empty() {
        return String::new();
    }
    format!("{}{}", id, if e.screen { "|screen" } else { "|camera" })
}

/// Stable id of a phone source: "usb:<serial>:<port>" on a cable, "<ip>:<port>" on the network.
pub fn endpoint_key(e: &Endpoint) -> String {
    let first = if e.usb() {
        format!("usb:{}", e.serial)
    } else {
        e.host.clone()
    };
    format!("{first}:{}", e.port)
}

#[derive(Default)]
struct Entry {
    endpoint: Endpoint,
    link: Option<Arc<PhoneLink>>,
    state: LinkState,
    error: String,
    /// GetTickCount64() before which no new attempt is made
    retry_at: u64,
    /// in the latest known_phones()
    listed: bool,
    /// caps asked for once in this run
    caps_fetched: bool,
    /// the idle link's current connection
    connected_since: u64,
    /// connections lost in a row (each one doubles the wait)
    drops: u32,
}

struct Locked {
    stop: bool,
    woken: bool,
    entries: BTreeMap<String, Entry>,
    /// being serviced: stop() cuts its attempt short
    current: Option<Arc<PhoneLink>>,
}

struct Inner {
    lock: Mutex<Locked>,
    wake: Condvar,
    generation: AtomicU32,
}

pub struct Connector {
    inner: Arc<Inner>,
    thread: Mutex<Option<JoinHandle<()>>>,
}

impl Default for Connector {
    fn default() -> Self {
        Self::new()
    }
}

impl Drop for Connector {
    fn drop(&mut self) {
        self.stop();
    }
}

impl Connector {
    pub fn new() -> Self {
        Connector {
            inner: Arc::new(Inner {
                lock: Mutex::new(Locked {
                    stop: false,
                    woken: false,
                    entries: BTreeMap::new(),
                    current: None,
                }),
                wake: Condvar::new(),
                generation: AtomicU32::new(1),
            }),
            thread: Mutex::new(None),
        }
    }

    /// `delay_ms`: the first look waits this long (started with Windows: let it boot first);
    /// `before_first` runs on the connector's thread just before it (starts discovery).
    pub fn start(&self, delay_ms: u32, before_first: Option<Box<dyn FnOnce() + Send>>) {
        let mut thread = self.thread.lk();
        if thread.is_some() {
            return;
        }
        self.inner.lock.lk().stop = false;
        let inner = Arc::clone(&self.inner);
        *thread = Some(std::thread::spawn(move || {
            inner.run(delay_ms, before_first)
        }));
    }

    pub fn stop(&self) {
        let current;
        {
            let mut l = self.inner.lock.lk();
            l.stop = true;
            current = l.current.clone();
        }
        if let Some(c) = current {
            c.abort(); // an attempt in progress ends at once
        }
        self.inner.wake.notify_all();
        let t = self.thread.lk().take();
        if let Some(t) = t {
            let _ = t.join();
        }
    }

    /// After `stop()`: forgets every phone, closing the idle links.
    pub fn clear(&self) {
        let entries;
        {
            let mut l = self.inner.lock.lk();
            entries = std::mem::take(&mut l.entries);
            self.inner.generation.fetch_add(1, Ordering::SeqCst);
        }
        // Links nobody else holds close here (PhoneLink's Drop).
        drop(entries);
    }

    /// The phone list may have changed: look now rather than at the next tick.
    pub fn wake(&self) {
        {
            self.inner.lock.lk().woken = true;
        }
        self.inner.wake.notify_all();
    }

    /// The link for this endpoint, the same object every time for the same `endpoint_key`.
    pub fn link(&self, e: &Endpoint) -> Arc<PhoneLink> {
        let mut l = self.inner.lock.lk();
        let entry = l.entries.entry(endpoint_key(e)).or_default();
        if entry.link.is_none() {
            entry.endpoint = e.clone();
            self.inner.generation.fetch_add(1, Ordering::SeqCst);
        }
        Arc::clone(entry.link.get_or_insert_with(|| Arc::new(PhoneLink::new())))
    }

    /// The state of a link. `error` receives the last error text unless the link is connected.
    pub fn state(&self, key: &str, error: Option<&mut String>) -> LinkState {
        let l = self.inner.lock.lk();
        let Some(entry) = l.entries.get(key) else {
            return LinkState::Idle;
        };
        if entry.link.as_ref().is_some_and(|k| k.connected()) {
            return LinkState::Connected;
        }
        if let Some(e) = error {
            *e = entry.error.clone();
        }
        // Connected by someone else and closed since: the next tick says what it is now.
        if entry.state == LinkState::Connected {
            LinkState::Idle
        } else {
            entry.state
        }
    }

    /// Changes whenever a state changes: the app refreshes its list then.
    pub fn generation(&self) -> u32 {
        self.inner.generation.load(Ordering::SeqCst)
    }
}

impl Inner {
    fn set(&self, key: &str, state: LinkState, error: &str, retry_at: u64) {
        let mut l = self.lock.lk();
        let Some(e) = l.entries.get_mut(key) else {
            return;
        };
        e.retry_at = retry_at;
        if e.state == state && e.error == error {
            return;
        }
        e.state = state;
        e.error = error.to_string();
        self.generation.fetch_add(1, Ordering::SeqCst);
    }

    fn run(&self, delay_ms: u32, before_first: Option<Box<dyn FnOnce() + Send>>) {
        if delay_ms != 0 {
            let l = self.lock.lk();
            let (l, _) = self
                .wake
                .wait_timeout_while(l, Duration::from_millis(u64::from(delay_ms)), |s| !s.stop)
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if l.stop {
                return;
            }
        }
        if let Some(f) = before_first {
            f();
        }
        loop {
            // The phones to look after: everything listed now, plus links still open from before.
            let phones = known_phones();
            let mut keys: Vec<String> = Vec::new();
            {
                let mut l = self.lock.lk();
                if l.stop {
                    break;
                }
                let mut before: BTreeMap<String, bool> = BTreeMap::new();
                for (key, entry) in l.entries.iter_mut() {
                    before.insert(key.clone(), entry.listed);
                    entry.listed = false;
                }
                let mut changed = false;
                for e in &phones {
                    let key = endpoint_key(e);
                    let was_listed = before.get(&key).copied().unwrap_or(false);
                    let entry = l.entries.entry(key.clone()).or_default();
                    if entry.link.is_none() {
                        entry.link = Some(Arc::new(PhoneLink::new()));
                    }
                    let back = !was_listed
                        || entry.endpoint.host != e.host
                        || entry.endpoint.port != e.port
                        || entry.endpoint.ready != e.ready;
                    if back && e.ready {
                        // The phone (re)appeared: its app was opened again, the cable plugged in or
                        // allowed. Try now, not after the wait from earlier failures.
                        entry.retry_at = 0;
                        entry.drops = 0;
                    }
                    changed |= back || entry.endpoint.name != e.name;
                    entry.endpoint = e.clone();
                    entry.listed = true;
                }
                for (key, entry) in &l.entries {
                    changed |= before.get(key).copied().unwrap_or(false) && !entry.listed; // gone from the list
                    keys.push(key.clone());
                }
                if changed {
                    self.generation.fetch_add(1, Ordering::SeqCst);
                }
            }

            for key in &keys {
                if self.lock.lk().stop {
                    break;
                }
                self.service(key);
            }

            let l = self.lock.lk();
            let (mut l, _) = self
                .wake
                .wait_timeout_while(l, TICK, |s| !(s.stop || s.woken))
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            l.woken = false;
            if l.stop {
                break;
            }
        }
    }

    fn service(&self, key: &str) {
        let e;
        let link;
        let listed;
        let retry_at;
        let state;
        {
            let l = self.lock.lk();
            let Some(entry) = l.entries.get(key) else {
                return;
            };
            let Some(k) = &entry.link else {
                return;
            };
            e = entry.endpoint.clone();
            link = Arc::clone(k);
            listed = entry.listed;
            retry_at = entry.retry_at;
            state = entry.state;
        }

        // Someone else reads this link now (streaming, a screen window, pairing): they keep it alive.
        if !link.try_lease(true) {
            return;
        }
        let _lease = LeaseGuard::new(&link);
        // Destroyed before the lease is given back.
        struct Current<'a> {
            inner: &'a Inner,
        }
        impl Drop for Current<'_> {
            fn drop(&mut self) {
                self.inner.lock.lk().current = None;
            }
        }
        self.lock.lk().current = Some(Arc::clone(&link));
        let _current = Current { inner: self };

        let now = tick_ms();
        if link.connected() {
            drain(&link);
            if link.connected() {
                if now.wrapping_sub(link.last_send_tick()) >= PING_MS {
                    let mut ping = Value::object();
                    ping.set("type", "ping");
                    link.send_control(&ping);
                }
                {
                    let mut l = self.lock.lk();
                    let entry = l.entries.entry(key.to_string()).or_default();
                    if entry.connected_since == 0 {
                        entry.connected_since = now;
                    }
                    if now.wrapping_sub(entry.connected_since) > STEADY_MS {
                        entry.drops = 0;
                    }
                }
                self.set(key, LinkState::Connected, "", 0);
                return;
            }
        }
        if link.preempted() {
            return; // given up for someone who needs it now: not a failure
        }

        if state == LinkState::Connected {
            // The link dropped (phone app closed, Wi-Fi gone, or another PC took the phone). Wait
            // before the next try, longer each time: a phone app from before "auto" existed hands
            // itself to every new connection, and two PCs would otherwise take it from each other.
            link.close();
            let drops;
            {
                let mut l = self.lock.lk();
                let entry = l.entries.entry(key.to_string()).or_default();
                entry.connected_since = 0;
                entry.drops = (entry.drops + 1).min(5);
                drops = entry.drops;
            }
            self.set(key, LinkState::Idle, "", now + (RETRY_MS << (drops - 1)));
            return;
        }

        if !listed || now < retry_at {
            return;
        }
        if !e.ready {
            self.set(key, LinkState::Idle, "", 0);
            return;
        }
        if self.covered_elsewhere(key, &e, &link) {
            self.set(key, LinkState::Elsewhere, "", 0);
            return;
        }
        if !e.usb() {
            // Wi-Fi: only phones this PC has paired with (the key is stored by the camera's id; the
            // shared screen uses the same one). A manual entry is known by id once it connected.
            let id = base_id(&if e.device_id.is_empty() {
                link.device_id()
            } else {
                e.device_id.clone()
            });
            if id.is_empty() {
                self.set(key, LinkState::Idle, "", 0);
                return;
            }
            if load_pair_key(&id).is_empty() {
                self.set(key, LinkState::NeedsPairing, "", 0);
                return;
            }
        }
        self.attempt(key, &e, &link);
    }

    fn attempt(&self, key: &str, e: &Endpoint, link: &PhoneLink) {
        self.set(key, LinkState::Connecting, "", 0);
        let fetch_caps = {
            let mut l = self.lock.lk();
            let entry = l.entries.entry(key.to_string()).or_default();
            !entry.caps_fetched || link.caps().is_empty()
        };
        // A connect (and the caps after it) may be cut short by anyone who needs the link now.
        link.set_attempting(true);
        let mut error = String::new();
        let mut ok = match link.connect_auto(e) {
            Ok(()) => true,
            Err(m) => {
                error = m;
                false
            }
        };
        // Caps once per run (and whenever the phone sends them again): enough to pick the source
        // at once. The phone may test-open hidden cameras before answering, so this can take a while.
        if ok && fetch_caps {
            match link.request_caps(REQUEST_CAPS_DEFAULT_MS) {
                Ok(_) => {
                    self.lock
                        .lk()
                        .entries
                        .entry(key.to_string())
                        .or_default()
                        .caps_fetched = true;
                }
                Err(m) => {
                    error = m;
                    ok = false;
                }
            }
        }
        link.set_attempting(false);
        let now = tick_ms();
        if link.preempted() {
            if !ok {
                link.close();
            }
            self.set(key, LinkState::Idle, "", 0);
            return;
        }
        if !ok {
            link.close();
            let code = link.last_error_code();
            if code == "busy" {
                self.set(key, LinkState::Busy, &error, now + RETRY_MS);
            } else if code == "pair" {
                self.set(key, LinkState::NeedsPairing, &error, now + PAIR_RETRY_MS);
            } else {
                self.set(key, LinkState::Unreachable, &error, now + RETRY_MS);
            }
            return;
        }
        self.lock
            .lk()
            .entries
            .entry(key.to_string())
            .or_default()
            .connected_since = now;
        self.set(key, LinkState::Connected, "", 0);
    }

    fn covered_elsewhere(&self, key: &str, e: &Endpoint, link: &PhoneLink) -> bool {
        let id = identity(e, link);
        let l = self.lock.lk();
        for (other_key, other) in &l.entries {
            let Some(other_link) = &other.link else {
                continue;
            };
            if other_key == key || other.endpoint.screen != e.screen {
                continue;
            }
            let other_id = identity(&other.endpoint, other_link);
            // A cable phone never connected has no id yet. It may be any Wi-Fi phone of the same OS
            // (names do not match on an iPhone: mDNS says "iPhone", the cable gives the user's
            // name), so while one of those is in use, wait: connecting would take it away.
            if id.is_empty()
                && e.usb()
                && !other.endpoint.usb()
                && other.endpoint.os == e.os
                && other_link.in_use()
            {
                return true;
            }
            let same = if !id.is_empty() {
                other_id == id
            } else {
                e.usb()
                    && !other.endpoint.usb()
                    && !e.name.is_empty()
                    && other.endpoint.name == e.name
            };
            if !same {
                continue;
            }
            // In use that way (streaming, a screen window): connecting here would take it away.
            if other_link.in_use() {
                return true;
            }
            // Connected that way already. The cable is preferred: it takes over an idle Wi-Fi link
            // (the phone lets the same PC replace its own session).
            if other_link.connected() && !(e.usb() && !other.endpoint.usb()) {
                return true;
            }
        }
        false
    }
}

/// Reads what an idle link has: the phone may send caps again, or the tail of a stream.
fn drain(link: &PhoneLink) {
    for _ in 0..64 {
        if !link.readable() {
            break;
        }
        let mut m = Message::default();
        if link.receive(&mut m, 1000) != Recv::Message {
            return; // closed (receive closed it), or a stall
        }
        if m.kind != Kind::Control {
            continue;
        }
        if let Ok(v) = m.json() {
            if v.get("type").str() == "caps" {
                link.set_caps(&v.dump());
            }
        }
    }
}
