//! `PhoneLink` and the `Connector` against the C++ stand-in phone `build\Release\niwcam-fake-phone.exe`
//! on loopback (port 47999, never 47321/47322), plus Rust stub phones for the paths the fake phone
//! cannot produce (a phone that goes silent, "busy").
//!
//! Needs the C++ build in `build\Release` (override with `NIWCAM_FAKE_PHONE`). The data folder is a
//! temp one (`CAMSTUDIO_DATA_DIR`), the fake phone runs in its own temp working directory (it writes
//! `fake-phone-<clientId>.key` there). Tests share one pairing store and one fake phone port, so they
//! run one at a time (a global lock). `NIWCAM_PHONE_LONG=1` adds the 40 s connector retry test.

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex, Once};
use std::time::{Duration, Instant};

use niwcam_core::crypto;
use niwcam_core::json::Value;
use niwcam_phone::connector::{endpoint_key, Connector, LinkState};
use niwcam_phone::discovery::{add_manual, remove_manual, Endpoint};
use niwcam_phone::phone_link::{
    client_id, forget_pair_key, has_pair_key, load_pair_key, save_pair_key, Kind, LeaseGuard,
    Message, PhoneLink, Recv, REQUEST_CAPS_DEFAULT_MS,
};

const PORT: u16 = 47999;
const FAKE_ID: &str = "FAKE-PHONE-0001";

static SERIAL: Mutex<()> = Mutex::new(());
static INIT: Once = Once::new();

fn lock() -> std::sync::MutexGuard<'static, ()> {
    SERIAL
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

fn scratch(name: &str) -> PathBuf {
    let dir = std::env::temp_dir()
        .join(format!("niwcam-phone-test-{}", std::process::id()))
        .join(name);
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// Sets `CAMSTUDIO_DATA_DIR` to a fresh temp folder before anything reads the data folder
/// (`client_id()` is cached for the process, like the C++ static).
fn init() {
    INIT.call_once(|| {
        let dir = scratch("data");
        std::env::set_var("CAMSTUDIO_DATA_DIR", &dir);
    });
    let dir = std::env::var("CAMSTUDIO_DATA_DIR").expect("data dir");
    assert!(
        dir.contains("niwcam-phone-test-"),
        "never use the real data folder: {dir}"
    );
}

fn fake_phone_exe() -> PathBuf {
    if let Ok(p) = std::env::var("NIWCAM_FAKE_PHONE") {
        return PathBuf::from(p);
    }
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../build/Release/niwcam-fake-phone.exe")
}

struct FakePhone {
    child: Option<Child>,
    cwd: PathBuf,
    log: PathBuf,
}

impl FakePhone {
    fn start(cwd: &Path, args: &[&str]) -> FakePhone {
        std::fs::create_dir_all(cwd).unwrap();
        let n = std::fs::read_dir(cwd).map(|d| d.count()).unwrap_or(0);
        let log = cwd.join(format!("fake-{n}.log"));
        let out = std::fs::File::create(&log).unwrap();
        let child = Command::new(fake_phone_exe())
            .args(["--port", &PORT.to_string()])
            .args(args)
            .current_dir(cwd)
            .stdout(Stdio::from(out))
            .stderr(Stdio::null())
            .spawn()
            .expect("start niwcam-fake-phone.exe (build it, or set NIWCAM_FAKE_PHONE)");
        let fp = FakePhone {
            child: Some(child),
            cwd: cwd.to_path_buf(),
            log,
        };
        assert!(
            wait_until(Duration::from_secs(10), || fp.log().contains("listening")),
            "fake phone did not start: {}",
            fp.log()
        );
        fp
    }

    fn log(&self) -> String {
        String::from_utf8_lossy(&std::fs::read(&self.log).unwrap_or_default()).into_owned()
    }

    fn key_file(&self) -> PathBuf {
        self.cwd.join(format!("fake-phone-{}.key", client_id()))
    }

    /// Kills it and returns its whole log.
    fn stop(&mut self) -> String {
        if let Some(mut c) = self.child.take() {
            let _ = c.kill();
            let _ = c.wait();
        }
        self.log()
    }
}

impl Drop for FakePhone {
    fn drop(&mut self) {
        self.stop();
    }
}

fn wait_until(timeout: Duration, mut f: impl FnMut() -> bool) -> bool {
    let end = Instant::now() + timeout;
    while Instant::now() < end {
        if f() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(25));
    }
    f()
}

fn log_has(fp: &FakePhone, what: &str) -> bool {
    wait_until(Duration::from_secs(3), || fp.log().contains(what))
}

fn endpoint(port: u16) -> Endpoint {
    Endpoint {
        host: "127.0.0.1".to_string(),
        port,
        name: "127.0.0.1".to_string(),
        manual: true,
        ..Endpoint::default()
    }
}

fn object(pairs: &[(&str, Value)]) -> Value {
    let mut v = Value::object();
    for (k, x) in pairs {
        v.set(k, x.clone());
    }
    v
}

fn pair(link: &PhoneLink, sas_out: &Mutex<Vec<String>>) -> Result<(), String> {
    link.connect(
        &endpoint(PORT),
        Some(&|c: &str| sas_out.lock().unwrap().push(c.to_string())),
        Some(&|| true),
    )
}

fn nal_types(d: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    let mut i = 0;
    while i + 3 < d.len() {
        let skip = if d[i] == 0 && d[i + 1] == 0 && d[i + 2] == 1 {
            3
        } else if i + 4 < d.len() && d[i] == 0 && d[i + 1] == 0 && d[i + 2] == 0 && d[i + 3] == 1 {
            4
        } else {
            0
        };
        if skip == 0 {
            i += 1;
            continue;
        }
        out.push(d[i + skip] & 0x1f);
        i += skip;
    }
    out
}

#[derive(Default)]
struct Collected {
    video: Vec<(Instant, i64, bool, usize)>,
    first_key: Vec<u8>,
    first_frame: Vec<u8>,
    config: usize,
    audio: Vec<(i64, Vec<u8>)>,
    controls: Vec<Value>,
    failed: bool,
}

/// Receives records until `until` says stop, the link fails or `max` passes.
fn collect(
    link: &PhoneLink,
    max: Duration,
    mut until: impl FnMut(&Collected) -> bool,
) -> Collected {
    let mut c = Collected::default();
    let end = Instant::now() + max;
    let mut m = Message::default();
    while Instant::now() < end && !until(&c) {
        match link.receive(&mut m, 500) {
            Recv::TimedOut => continue,
            Recv::Failed => {
                c.failed = true;
                break;
            }
            Recv::Message => {}
        }
        match m.kind {
            Kind::Control => c.controls.push(m.json().expect("control json")),
            Kind::VideoConfig => c.config += 1,
            Kind::VideoFrame => {
                if c.first_frame.is_empty() {
                    c.first_frame = m.payload.clone();
                }
                if m.keyframe() && c.first_key.is_empty() {
                    c.first_key = m.payload.clone();
                }
                c.video
                    .push((Instant::now(), m.pts_us, m.keyframe(), m.payload.len()));
            }
            Kind::Audio => c.audio.push((m.pts_us, m.payload.clone())),
            Kind::Other(k) => panic!("unknown record kind {k}"),
        }
    }
    c
}

fn start_message(audio: bool) -> Value {
    let mut start = Value::object();
    start
        .set("type", "start")
        .set("lens", "cam0")
        .set("width", 1280)
        .set("height", 720)
        .set("fps", 30)
        .set("codec", "h264")
        .set("bitrateKbps", 4000);
    if audio {
        start.set("audio", true);
    }
    start
}

fn ctl(kind: &str) -> Value {
    object(&[("type", Value::from(kind))])
}

/// Fresh state: no PC key, no fake phone key.
fn fresh_phone(name: &str, args: &[&str]) -> FakePhone {
    forget_pair_key(FAKE_ID);
    FakePhone::start(&scratch(name), args)
}

// ---------------------------------------------------------------------------------------------

#[test]
fn pair_stream_set_stop() {
    let _g = lock();
    init();
    let mut fp = fresh_phone("fp-basic", &[]);
    assert!(!has_pair_key(FAKE_ID));
    let link = PhoneLink::new();
    let sas = Mutex::new(Vec::new());
    pair(&link, &sas).expect("pairing with the fake phone");

    // the SAS the PC showed is the one the fake phone printed; the callback ends with ""
    let shown = sas.lock().unwrap().clone();
    assert_eq!(shown.len(), 2, "{shown:?}");
    assert_eq!(shown[0].len(), 6);
    assert_eq!(shown[1], "");
    assert_eq!(link.device_id(), FAKE_ID);
    assert_eq!(link.device_name(), "Fake Phone");
    assert_eq!(link.os(), "android");
    assert!(link.connected());
    assert!(has_pair_key(FAKE_ID));
    assert_eq!(load_pair_key(FAKE_ID).len(), 32);
    // the phone stored the same key (it keeps raw bytes in its working directory)
    assert_eq!(
        std::fs::read(fp.key_file()).unwrap(),
        load_pair_key(FAKE_ID)
    );

    // caps
    let caps = link.request_caps(REQUEST_CAPS_DEFAULT_MS).expect("caps");
    assert_eq!(caps.get("type").str(), "caps");
    assert_eq!(caps.get("lenses").items()[0].get("id").str(), "cam0");
    let mode = &caps.get("modes").items()[0];
    assert_eq!(
        (mode.get("width").int(0), mode.get("height").int(0)),
        (1280, 720)
    );
    assert_eq!(mode.get("fps").items().len(), 1);
    assert_eq!(link.caps(), caps.dump());
    // (the fake phone flushes its log after a message, so only now are the pairing lines there)
    assert!(
        log_has(&fp, &format!("Pairing code: {}", shown[0])),
        "{}",
        fp.log()
    );
    assert!(log_has(&fp, "Authenticated, channel encrypted"));
    assert!(caps.get("audio").is_null());

    // start, 100+ frames
    assert!(link.send_control(&start_message(false)));
    let t0 = Instant::now();
    let c = collect(&link, Duration::from_secs(20), |c| c.video.len() >= 100);
    let took = t0.elapsed();
    assert!(!c.failed);
    assert!(c.video.len() >= 100, "frames: {}", c.video.len());
    // 30 fps: 100 frames take about 3.3 s
    assert!(
        took > Duration::from_millis(2500) && took < Duration::from_secs(10),
        "{took:?}"
    );
    let started = c
        .controls
        .iter()
        .find(|v| v.get("type").str() == "started")
        .expect("started");
    assert_eq!(started.get("width").int(0), 1280);
    assert_eq!(started.get("orientation").int(0), 0);
    assert!(!started.get("audio").get("on").bool_or(true));
    // The fake phone never sends VIDEO_CONFIG (SPS/PPS come inline with the keyframe).
    assert_eq!(c.config, 0);
    assert!(c.video[0].2, "first frame is a key frame");
    let nals = nal_types(&c.first_key);
    assert!(
        nals.contains(&7) && nals.contains(&8) && nals.contains(&5),
        "NALs {nals:?}"
    );
    assert!(c.first_frame.starts_with(&[0, 0, 0, 1]) || c.first_frame.starts_with(&[0, 0, 1]));
    // pts follows the 30 fps clock: frame n carries n * 333333 / 10 microseconds. The encoder may
    // reorder or drop a frame, so sort and require every step to be a whole number of frames.
    let mut pts: Vec<i64> = c.video.iter().map(|v| v.1).collect();
    pts.sort_unstable();
    for w in pts.windows(2) {
        let step = w[1] - w[0];
        let frames = (step as f64 / 33_333.3).round();
        assert!(
            frames >= 1.0 && (step as f64 - frames * 33_333.3).abs() < 3.0,
            "pts step {step}"
        );
    }

    // set / stats go out as the JSON the C++ would write: the fake phone echoes what it parsed
    let set = object(&[
        ("type", "set".into()),
        ("zoom", 2.into()),
        ("torch", 0.5.into()),
    ]);
    let stats = object(&[
        ("type", "stats".into()),
        ("recvFps", 30.into()),
        ("decodeMs", 2.5.into()),
        ("queueMs", 12.into()),
        ("lost", 0.into()),
    ]);
    assert!(link.send_control(&set));
    assert!(link.send_control(&stats));
    assert!(link.send_control(&ctl("keyframe")));
    // a key frame follows the request (GOP is a second, so within 2 s)
    let after = collect(&link, Duration::from_secs(3), |c| {
        c.video.iter().skip(1).any(|v| v.2)
    });
    assert!(
        after.video.iter().skip(1).any(|v| v.2),
        "no key frame after keyframe request"
    );
    assert!(link.send_control(&ctl("stop")));
    // no frames after stop (a few in flight are fine; then silence)
    let _ = collect(&link, Duration::from_millis(600), |_| false);
    let quiet = collect(&link, Duration::from_millis(1200), |_| false);
    assert!(
        quiet.video.is_empty(),
        "frames after stop: {}",
        quiet.video.len()
    );
    assert!(!link.readable());

    link.close();
    assert!(!link.connected());
    assert!(wait_until(Duration::from_secs(3), || fp
        .log()
        .contains("PC disconnected")));
    let log = fp.stop();
    assert!(log.contains("start: lens=cam0 1280x720@30 h264"), "{log}");
    assert!(log.contains("start: audio off"));
    assert!(log.contains(&format!("set: {}", set.dump())), "{log}");
    assert!(log.contains(&format!("stats: {}", stats.dump())), "{log}");
}

#[test]
fn audio_records_are_the_440_hz_tone() {
    let _g = lock();
    init();
    let mut fp = fresh_phone("fp-audio", &["--audio"]);
    let link = PhoneLink::new();
    pair(&link, &Mutex::new(Vec::new())).expect("pair");
    let caps = link.request_caps(REQUEST_CAPS_DEFAULT_MS).expect("caps");
    let audio = caps.get("audio");
    assert_eq!(audio.get("format").str(), "pcm16");
    assert_eq!(audio.get("channels").int(0), 1);
    assert_eq!(audio.get("rates").items()[0].int(0), 48000);

    assert!(link.send_control(&start_message(true)));
    let c = collect(&link, Duration::from_secs(20), |c| c.audio.len() >= 200);
    assert!(c.audio.len() >= 200, "audio records: {}", c.audio.len());
    let started = c
        .controls
        .iter()
        .find(|v| v.get("type").str() == "started")
        .expect("started");
    assert!(started.get("audio").get("on").bool_or(false));
    assert_eq!(started.get("audio").get("rate").int(0), 48000);
    // 10 ms of 16-bit mono at 48 kHz = 960 bytes; pts is the sample clock after the record
    #[expect(
        clippy::approx_constant,
        reason = "the exact literal the C++ fake phone uses"
    )]
    let pi = 3.14159265358979_f64;
    let mut n: u64 = 0;
    let mut wrong = 0usize;
    let mut total = 0usize;
    for (k, (pts, data)) in c.audio.iter().enumerate() {
        assert_eq!(data.len(), 960);
        assert_eq!(*pts, (k as i64 + 1) * 10_000, "pts of record {k}");
        for s in data.as_chunks::<2>().0 {
            let want = (8000.0 * (2.0 * pi * 440.0 * (n as f64) / 48000.0).sin()) as i16;
            n += 1;
            let got = i16::from_le_bytes([s[0], s[1]]);
            if got != want {
                wrong += 1;
            }
            total += 1;
        }
    }
    assert_eq!(
        wrong, 0,
        "{wrong} of {total} samples differ from the 440 Hz tone"
    );
    // set audio off: the phone says so in `state` and stops
    assert!(link.send_control(&object(&[("type", "set".into()), ("audio", false.into())])));
    let off = collect(&link, Duration::from_secs(5), |c| {
        c.controls.iter().any(|v| v.get("type").str() == "state")
    });
    let st = off
        .controls
        .iter()
        .find(|v| v.get("type").str() == "state")
        .expect("state after set audio");
    assert!(!st.get("audio").get("on").bool_or(true));
    let quiet = collect(&link, Duration::from_secs(1), |_| false);
    assert!(
        quiet.audio.len() <= 1,
        "audio after off: {}",
        quiet.audio.len()
    );
    // and on again
    assert!(link.send_control(&object(&[("type", "set".into()), ("audio", true.into())])));
    let on = collect(&link, Duration::from_secs(5), |c| c.audio.len() >= 10);
    assert!(on.audio.len() >= 10);
    link.close();
    let log = fp.stop();
    assert!(log.contains("start: audio on"), "{log}");
    assert!(
        log.contains("set: {\"type\":\"set\",\"audio\":false}"),
        "{log}"
    );
}

#[test]
fn thermal_state_arrives() {
    let _g = lock();
    init();
    let mut fp = fresh_phone("fp-thermal", &["--thermal", "serious"]);
    let link = PhoneLink::new();
    pair(&link, &Mutex::new(Vec::new())).expect("pair");
    let caps = link.request_caps(REQUEST_CAPS_DEFAULT_MS).expect("caps");
    let fps: Vec<i32> = caps.get("modes").items()[0]
        .get("fps")
        .items()
        .iter()
        .map(|f| f.int(0))
        .collect();
    assert_eq!(fps, vec![30, 60]);
    assert!(link.send_control(&start_message(false)));
    let c = collect(&link, Duration::from_secs(15), |c| {
        c.controls.iter().any(|v| v.get("type").str() == "state")
    });
    let st = c
        .controls
        .iter()
        .find(|v| v.get("type").str() == "state")
        .expect("state");
    assert_eq!(st.get("thermal").str(), "serious");
    assert_eq!(st.get("battery").int(0), 80);
    assert!(!st.get("charging").bool_or(true));
    // ~5 s into the stream
    assert!(c.video.len() > 100);
    link.close();
    fp.stop();
}

#[test]
fn stall_makes_a_burst_and_stats_reach_the_phone() {
    let _g = lock();
    init();
    let mut fp = fresh_phone("fp-stall", &["--stall", "250"]);
    let link = PhoneLink::new();
    pair(&link, &Mutex::new(Vec::new())).expect("pair");
    link.request_caps(REQUEST_CAPS_DEFAULT_MS).expect("caps");
    assert!(link.send_control(&start_message(false)));
    let mut sent = 0;
    let mut all = Collected::default();
    let mut max_pending = 0usize;
    let begin = Instant::now();
    let mut m = Message::default();
    let mut last_stats = Instant::now();
    while begin.elapsed() < Duration::from_secs(9) {
        match link.receive(&mut m, 200) {
            Recv::Message => {
                if m.kind == Kind::VideoFrame {
                    all.video
                        .push((Instant::now(), m.pts_us, m.keyframe(), m.payload.len()));
                    max_pending = max_pending.max(link.pending_bytes());
                }
            }
            Recv::TimedOut => {}
            Recv::Failed => panic!("link failed"),
        }
        if last_stats.elapsed() >= Duration::from_secs(1) {
            last_stats = Instant::now();
            let v = object(&[
                ("type", "stats".into()),
                ("recvFps", 30.into()),
                ("decodeMs", 1.5.into()),
                ("queueMs", (260 + sent).into()),
                ("lost", 0.into()),
            ]);
            assert!(link.send_control(&v));
            sent += 1;
        }
    }
    assert!(all.video.len() >= 200, "frames {}", all.video.len());
    let gaps: Vec<u128> = all
        .video
        .windows(2)
        .map(|w| w[1].0.duration_since(w[0].0).as_millis())
        .collect();
    let max_gap = gaps.iter().copied().max().unwrap();
    assert!(
        max_gap >= 200,
        "a stall of 250 ms shows as a gap, max {max_gap} ms"
    );
    // after the stall the held frames arrive back to back
    assert!(
        gaps.iter().filter(|g| **g <= 5).count() >= 3,
        "no burst after the stall"
    );
    link.close();
    let log = fp.stop();
    let lines = log.lines().filter(|l| l.starts_with("stats: ")).count();
    assert_eq!(lines, sent as usize, "{log}");
    assert!(log.contains("stats: {\"type\":\"stats\",\"recvFps\":30,\"decodeMs\":1.5,\"queueMs\":260,\"lost\":0}"), "{log}");
    eprintln!(
        "stall test: {} frames, max gap {max_gap} ms, max pending bytes {max_pending}",
        all.video.len()
    );
}

#[test]
fn reconnect_is_silent_and_a_dead_phone_closes_the_link() {
    let _g = lock();
    init();
    let cwd = scratch("fp-reconnect");
    forget_pair_key(FAKE_ID);
    let mut fp = FakePhone::start(&cwd, &[]);
    let link = PhoneLink::new();
    pair(&link, &Mutex::new(Vec::new())).expect("pair");
    link.request_caps(REQUEST_CAPS_DEFAULT_MS).expect("caps");
    assert!(link.connected());

    // the phone dies under an open link: receive fails and closes it
    fp.stop();
    let mut m = Message::default();
    let r = link.receive(&mut m, 3000);
    assert_eq!(r, Recv::Failed);
    assert!(!link.connected());
    assert_eq!(
        link.device_id(),
        FAKE_ID,
        "identity is kept after the link closes"
    );
    assert!(!link.caps().is_empty(), "caps are kept across reconnects");

    // while it is down a reconnect fails with the user's message
    let err = link.reconnect().expect_err("phone is down");
    assert_eq!(
        err,
        "Không kết nối được tới điện thoại 127.0.0.1:47999. Kiểm tra app NiwCam trên điện thoại đang mở và cùng mạng Wi\u{2011}Fi."
    );
    assert_eq!(link.last_error_code(), "");

    // restart the phone (same working directory: it keeps its key): reconnect never pairs
    let fp2 = FakePhone::start(&cwd, &[]);
    link.reconnect().expect("silent reconnect");
    assert!(link.connected());
    let caps = link
        .request_caps(REQUEST_CAPS_DEFAULT_MS)
        .expect("caps after reconnect");
    assert_eq!(caps.get("type").str(), "caps");
    assert!(link.send_control(&start_message(false)));
    let c = collect(&link, Duration::from_secs(10), |c| c.video.len() >= 30);
    assert!(c.video.len() >= 30);
    link.close();
    assert!(wait_until(Duration::from_secs(3), || fp2
        .log()
        .contains("PC disconnected")));
    let log = fp2.log();
    assert!(!log.contains("Pairing code"), "reconnect paired: {log}");
    assert!(log.contains("Authenticated"));
}

#[test]
fn wrong_key_marks_pair_only_on_a_clean_close() {
    let _g = lock();
    init();
    let cwd = scratch("fp-wrongkey");
    forget_pair_key(FAKE_ID);
    let mut fp = FakePhone::start(&cwd, &[]);
    let link = PhoneLink::new();
    pair(&link, &Mutex::new(Vec::new())).expect("pair");
    link.set_caps("{\"x\":1}");
    link.close();
    fp.stop();

    // 1. the phone's key no longer matches ours (it stays non-empty, so it says paired=true)
    std::fs::write(fp.key_file(), [0xA5u8; 32]).unwrap();
    let fp = FakePhone::start(&cwd, &[]);
    let err = link.connect_known(&endpoint(PORT)).expect_err("wrong key");
    assert_eq!(
        err,
        "Điện thoại không nhận ra máy tính này. Chọn điện thoại để ghép đôi lại."
    );
    assert_eq!(link.last_error_code(), "pair");
    assert_eq!(
        link.caps(),
        "",
        "caps are cleared so picking the phone goes through pairing"
    );
    assert!(log_has(&fp, "PC authentication failed"), "{}", fp.log());
    assert!(!link.connected());
    assert!(
        has_pair_key(FAKE_ID),
        "the old key stays until a new pairing succeeds"
    );

    // 2. a user pick: the first try says "pair", the second repairs with the 6-digit code on both sides
    let sas = Mutex::new(Vec::new());
    pair(&link, &sas).expect("re-pairing after a mismatch");
    let shown = sas.lock().unwrap().clone();
    assert_eq!(shown.len(), 2, "{shown:?}");
    link.close();
    assert!(log_has(&fp, &format!("Pairing code: {}", shown[0])));
    assert_eq!(
        std::fs::read(fp.key_file()).unwrap(),
        load_pair_key(FAKE_ID)
    );
    link.connect_known(&endpoint(PORT))
        .expect("known connect works again");
    link.close();
    drop(fp);

    // 3. the phone lost its key (paired=false): known connects say "pair" without deleting ours
    let _ = std::fs::remove_file(cwd.join(format!("fake-phone-{}.key", client_id())));
    let fp = FakePhone::start(&cwd, &[]);
    let err = link
        .connect_known(&endpoint(PORT))
        .expect_err("phone forgot us");
    assert_eq!(
        err,
        "Điện thoại cần ghép đôi lại: chọn lại điện thoại trong danh sách camera"
    );
    assert_eq!(link.last_error_code(), "pair");
    assert!(has_pair_key(FAKE_ID));
    // 4. the PC user declines: nothing is saved, the callback ends with ""
    let sas = Mutex::new(Vec::new());
    let before = load_pair_key(FAKE_ID);
    let err = link
        .connect(
            &endpoint(PORT),
            Some(&|c: &str| sas.lock().unwrap().push(c.to_string())),
            Some(&|| false),
        )
        .expect_err("declined");
    assert_eq!(err, "Đã từ chối ghép đôi trên máy tính");
    assert_eq!(sas.lock().unwrap().last().unwrap(), "");
    assert_eq!(load_pair_key(FAKE_ID), before);
    drop(fp);
}

#[test]
fn readable_and_pending_bytes_see_unread_data() {
    let _g = lock();
    init();
    let mut fp = fresh_phone("fp-pending", &[]);
    let link = PhoneLink::new();
    pair(&link, &Mutex::new(Vec::new())).expect("pair");
    link.request_caps(REQUEST_CAPS_DEFAULT_MS).expect("caps");
    assert!(!link.readable());
    assert_eq!(link.pending_bytes(), 0);
    assert!(link.send_control(&start_message(false)));
    // do not read for a while: the frames pile up in the socket
    std::thread::sleep(Duration::from_millis(1500));
    assert!(link.readable());
    let pending = link.pending_bytes();
    assert!(pending > 1_000, "pending bytes {pending}");
    // reading one record leaves the rest of the stream pending (the "decode but do not publish" rule)
    let mut m = Message::default();
    assert_eq!(link.receive(&mut m, 1000), Recv::Message);
    assert!(link.pending_bytes() > 0);
    assert!(link.send_control(&ctl("stop")));
    let _ = collect(&link, Duration::from_millis(1500), |_| false);
    assert_eq!(link.pending_bytes(), 0);
    assert!(!link.readable());
    link.close();
    assert_eq!(link.pending_bytes(), 0);
    fp.stop();
}

// ---- stub phones: what the fake phone cannot do ---------------------------------------------

fn read_record(s: &mut TcpStream) -> Option<(u8, Vec<u8>)> {
    let mut head = [0u8; 5];
    s.read_exact(&mut head).ok()?;
    let len = u32::from_be_bytes([head[0], head[1], head[2], head[3]]) as usize;
    let mut body = vec![0u8; len - 1];
    s.read_exact(&mut body).ok()?;
    Some((head[4], body))
}

fn write_handshake(s: &mut TcpStream, v: &Value) {
    let text = v.dump();
    let mut rec = Vec::new();
    rec.extend_from_slice(&(text.len() as u32 + 1).to_be_bytes());
    rec.push(1);
    rec.extend_from_slice(text.as_bytes());
    s.write_all(&rec).unwrap();
}

/// A listener on an ephemeral loopback port; `handler` serves one connection.
fn stub(handler: impl FnOnce(TcpStream) + Send + 'static) -> (u16, std::thread::JoinHandle<()>) {
    let l = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = l.local_addr().unwrap().port();
    let t = std::thread::spawn(move || {
        if let Ok((s, _)) = l.accept() {
            handler(s);
        }
    });
    (port, t)
}

fn stub_hello(id: &str, key: &crypto::EcdhKey, paired: bool) -> Value {
    object(&[
        ("type", "hello".into()),
        ("v", 1.into()),
        ("deviceId", id.into()),
        ("name", "Stub".into()),
        ("model", "M".into()),
        ("os", "android".into()),
        ("pub", crypto::base64(key.public_key()).into()),
        ("paired", paired.into()),
    ])
}

#[test]
fn auto_hello_bytes_and_busy_answer() {
    let _g = lock();
    init();
    let seen: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let seen2 = Arc::clone(&seen);
    let (port, t) = stub(move |mut s| {
        let (ty, body) = read_record(&mut s).unwrap();
        assert_eq!(ty, 1);
        seen2.lock().unwrap().push(String::from_utf8(body).unwrap());
        write_handshake(
            &mut s,
            &object(&[
                ("type", "error".into()),
                ("code", "busy".into()),
                ("message", "Điện thoại đang kết nối với PC khác".into()),
            ]),
        );
    });
    let link = PhoneLink::new();
    let err = link.connect_auto(&endpoint(port)).expect_err("busy");
    assert_eq!(err, "Điện thoại đang kết nối với PC khác");
    assert_eq!(link.last_error_code(), "busy");
    t.join().unwrap();
    let hello = seen.lock().unwrap()[0].clone();
    // key order and values exactly as PhoneLink.cpp writes them, "auto" last
    let id = client_id();
    let v = Value::parse(&hello).unwrap();
    assert_eq!(v.get("type").str(), "hello");
    assert_eq!(v.get("v").int(0), 1);
    assert_eq!(v.get("clientId").str(), id);
    assert_eq!(
        v.get("clientName").str(),
        niwcam_phone::phone_link::client_name()
    );
    assert!(v.get("auto").bool_or(false));
    let commit = v.get("commit").str().to_string();
    assert_eq!(crypto::from_base64(&commit).len(), 32);
    let want = format!(
        "{{\"type\":\"hello\",\"v\":1,\"clientId\":\"{}\",\"clientName\":\"{}\",\"commit\":\"{}\",\"auto\":true}}",
        id,
        niwcam_phone::phone_link::client_name(),
        commit
    );
    assert_eq!(hello, want);

    // a busy answer without a message uses the default text; and a user connect has no "auto"
    let seen3: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let seen4 = Arc::clone(&seen3);
    let (port, t) = stub(move |mut s| {
        let (_, body) = read_record(&mut s).unwrap();
        seen4.lock().unwrap().push(String::from_utf8(body).unwrap());
        write_handshake(
            &mut s,
            &object(&[("type", "error".into()), ("code", "busy".into())]),
        );
    });
    let err = link.connect_known(&endpoint(port)).expect_err("busy");
    assert_eq!(err, "Điện thoại đang dùng với máy tính khác");
    t.join().unwrap();
    assert!(!seen3.lock().unwrap()[0].contains("auto"));
    // any other error code: not busy
    let (port, t) = stub(move |mut s| {
        read_record(&mut s).unwrap();
        write_handshake(
            &mut s,
            &object(&[("type", "error".into()), ("code", "notPaired".into())]),
        );
    });
    let err = link.connect_known(&endpoint(port)).expect_err("error");
    assert_eq!(err, "Điện thoại từ chối kết nối");
    assert_eq!(link.last_error_code(), "");
    t.join().unwrap();
}

#[test]
fn silent_phone_after_auth_is_a_timeout_not_a_pair_request() {
    let _g = lock();
    init();
    save_pair_key("STUB-1", &[7u8; 32]);
    let (port, t) = stub(|mut s| {
        let key = crypto::EcdhKey::generate();
        read_record(&mut s).unwrap(); // hello
        write_handshake(&mut s, &stub_hello("STUB-1", &key, true));
        read_record(&mut s).unwrap(); // reveal
        let (_, auth) = read_record(&mut s).unwrap(); // auth: ours is a valid JSON with a mac
        assert_eq!(Value::parse_bytes(&auth).unwrap().get("type").str(), "auth");
        std::thread::sleep(Duration::from_secs(10)); // never answers
    });
    let link = PhoneLink::new();
    let t0 = Instant::now();
    let err = link.connect_known(&endpoint(port)).expect_err("silent");
    let took = t0.elapsed();
    assert_eq!(err, "Điện thoại không trả lời khi xác thực. Thử lại.");
    assert_eq!(
        link.last_error_code(),
        "",
        "a timeout must not ask for pairing"
    );
    assert!(
        took >= Duration::from_millis(7500) && took < Duration::from_secs(10),
        "{took:?}"
    );
    drop(link);
    let _ = t.join();
    forget_pair_key("STUB-1");
}

#[test]
fn abort_and_lease_semantics() {
    let _g = lock();
    init();
    // a phone that accepts and never answers hello
    let (port, t) = stub(|s| {
        std::thread::sleep(Duration::from_secs(12));
        drop(s);
    });
    let link = Arc::new(PhoneLink::new());
    // the connector's attempt: lease 1, marked attempting
    assert!(link.try_lease(true));
    link.set_attempting(true);
    let l2 = Arc::clone(&link);
    let attempt = std::thread::spawn(move || {
        let r = l2.connect_auto(&endpoint(port));
        l2.set_attempting(false);
        l2.close();
        l2.unlease();
        r
    });
    std::thread::sleep(Duration::from_millis(400));
    assert!(!link.in_use(), "the connector's lease is not 'in use'");
    // anyone else's lease() cuts the attempt short and waits for it
    // NOTE (same in the C++, see the report): shutdown() does not wake the handshake's select on
    // Windows, so the cut-short attempt ends at its own 8 s read timeout.
    let t0 = Instant::now();
    assert!(link.lease(15000));
    let waited = t0.elapsed();
    assert!(waited < Duration::from_secs(10), "lease took {waited:?}");
    assert!(link.preempted());
    assert!(link.in_use());
    assert!(attempt.join().unwrap().is_err());
    // lease held by someone else: another lease() times out; try_lease says no
    assert!(!link.try_lease(true));
    let t0 = Instant::now();
    assert!(!link.lease(300));
    assert!(t0.elapsed() >= Duration::from_millis(280));
    {
        let _guard = LeaseGuard::new(&link);
    }
    assert!(!link.in_use());
    assert!(link.try_lease(true));
    link.unlease();
    drop(t);
}

// ---- the connector ---------------------------------------------------------------------------

fn wait_state(c: &Connector, key: &str, want: LinkState, secs: u64) -> bool {
    wait_until(Duration::from_secs(secs), || c.state(key, None) == want)
}

#[test]
fn connector_connects_pings_gives_way_and_notices_drops() {
    let _g = lock();
    init();
    let cwd = scratch("fp-connector");
    forget_pair_key(FAKE_ID);
    let mut fp = FakePhone::start(&cwd, &[]);
    // pair once with a plain link, so there is a key on both sides
    let first = PhoneLink::new();
    pair(&first, &Mutex::new(Vec::new())).expect("pair");
    first.close();

    let ep = endpoint(PORT);
    let key = endpoint_key(&ep);
    assert_eq!(key, "127.0.0.1:47999");
    let connector = Connector::new();
    let link = connector.link(&ep);
    assert!(
        Arc::ptr_eq(&link, &connector.link(&ep)),
        "same link for the same key"
    );
    // a manual entry is known by id only once it connected
    link.connect_known(&ep).expect("connect once");
    link.close();
    let gen0 = connector.generation();
    add_manual("127.0.0.1", PORT);
    connector.start(0, None);
    assert!(
        wait_state(&connector, &key, LinkState::Connected, 12),
        "state {:?}",
        connector.state(&key, None)
    );
    assert!(connector.generation() != gen0);
    assert!(link.connected());
    assert!(!link.caps().is_empty(), "the connector fetched the caps");

    // someone else takes the link: the connector leaves it alone
    assert!(link.lease(3000));
    assert!(link.in_use());
    assert!(link.send_control(&start_message(false)));
    let c = collect(&link, Duration::from_secs(10), |c| c.video.len() >= 30);
    assert!(c.video.len() >= 30);
    assert!(link.send_control(&ctl("stop")));
    let _ = collect(&link, Duration::from_millis(800), |_| false);
    link.unlease();
    std::thread::sleep(Duration::from_millis(3500));
    assert_eq!(connector.state(&key, None), LinkState::Connected);

    // the phone goes away: within a tick or two the connector sees the link closed
    fp.stop();
    assert!(
        wait_until(Duration::from_secs(10), || connector.state(&key, None)
            == LinkState::Idle),
        "state after the phone died: {:?}",
        connector.state(&key, None)
    );
    assert!(!link.connected());

    connector.stop();
    connector.clear();

    // a phone we paired with but whose key we forgot: picking it is needed (NeedsPairing)
    let fp = FakePhone::start(&cwd, &[]);
    let connector = Connector::new();
    let link = connector.link(&ep);
    link.connect_known(&ep).expect("connect once");
    link.close();
    forget_pair_key(FAKE_ID);
    connector.start(0, None);
    assert!(wait_state(&connector, &key, LinkState::NeedsPairing, 10));
    connector.stop();
    connector.clear();
    drop(fp);
    remove_manual("127.0.0.1", PORT);
}

#[test]
fn connector_retries_after_a_drop_long() {
    if std::env::var("NIWCAM_PHONE_LONG").is_err() {
        eprintln!("skipped (set NIWCAM_PHONE_LONG=1: takes ~45 s)");
        return;
    }
    let _g = lock();
    init();
    let cwd = scratch("fp-connector-long");
    forget_pair_key(FAKE_ID);
    let mut fp = FakePhone::start(&cwd, &[]);
    let first = PhoneLink::new();
    pair(&first, &Mutex::new(Vec::new())).expect("pair");
    first.close();
    let ep = endpoint(PORT);
    let key = endpoint_key(&ep);
    let connector = Connector::new();
    let link = connector.link(&ep);
    link.connect_known(&ep).expect("connect once");
    link.close();
    add_manual("127.0.0.1", PORT);
    connector.start(0, None);
    assert!(wait_state(&connector, &key, LinkState::Connected, 12));
    // idle and connected: the connector pings every 15 s (docs/protocol.md 4.1), so the last send moves
    let sent_at = link.last_send_tick();
    assert!(
        wait_until(Duration::from_secs(22), || link.last_send_tick() > sent_at),
        "no ping within 22 s of an idle link"
    );
    fp.stop();
    assert!(wait_state(&connector, &key, LinkState::Idle, 10));
    let fp2 = FakePhone::start(&cwd, &[]);
    // 30 s wait after the first drop (docs: kRetryMs << (drops - 1))
    let t0 = Instant::now();
    assert!(wait_state(&connector, &key, LinkState::Connected, 45));
    let waited = t0.elapsed();
    assert!(
        waited > Duration::from_secs(20),
        "reconnected too early: {waited:?}"
    );
    drop(fp2);
    connector.stop();
    connector.clear();
    remove_manual("127.0.0.1", PORT);
}
