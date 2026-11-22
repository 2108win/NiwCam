//! CaptureEngine end to end against the C++ stand-in phone (`build\Release\niwcam-fake-phone.exe`,
//! port 47996 on loopback), in the Windows-10 style channel (`CAMSTUDIO_FORCE_DSHOW=1`): frames go
//! to a session-local shared memory under a unique test name, so the installed app (which uses the
//! `Global\` names) is untouched. A `niwcam-shm` `FrameReader` plays the app on the camera.
//!
//! Needs the C++ build in `build\Release` (override with `NIWCAM_FAKE_PHONE`). Takes about a
//! minute. The pairing key store is a temp folder (`CAMSTUDIO_DATA_DIR`).

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::time::{Duration, Instant};

use niwcam_core::image_settings::ImageSettings;
use niwcam_media::mf_devices::{CameraDevice, LensFacing, NativeFormat};
use niwcam_phone::discovery::Endpoint;
use niwcam_phone::phone_link::{LeaseGuard, PhoneLink, REQUEST_CAPS_DEFAULT_MS};
use niwcam_shm::{FrameReader, Names};
use windows::Win32::Media::MediaFoundation::{
    MFStartup, MFVideoFormat_H264, MFVideoFormat_NV12, MFSTARTUP_FULL, MF_VERSION,
};
use windows::Win32::System::Com::{CoInitializeEx, COINIT_MULTITHREADED};
use NiwCamEngine::{BackgroundMode, CaptureEngine, PauseKind, State};

const PORT: u16 = 47996;

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

struct FakePhone {
    child: Option<Child>,
    log: PathBuf,
}

impl FakePhone {
    fn start(cwd: &Path, args: &[&str]) -> FakePhone {
        std::fs::create_dir_all(cwd).unwrap();
        let exe = std::env::var("NIWCAM_FAKE_PHONE")
            .map(PathBuf::from)
            .unwrap_or_else(|_| {
                Path::new(env!("CARGO_MANIFEST_DIR"))
                    .join("../../build/Release/niwcam-fake-phone.exe")
            });
        let log = cwd.join("fake.log");
        let out = std::fs::File::create(&log).unwrap();
        let child = Command::new(exe)
            .args(["--port", &PORT.to_string()])
            .args(args)
            .current_dir(cwd)
            .stdout(Stdio::from(out))
            .stderr(Stdio::null())
            .spawn()
            .expect("start niwcam-fake-phone.exe (build it, or set NIWCAM_FAKE_PHONE)");
        let fp = FakePhone {
            child: Some(child),
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
}

impl Drop for FakePhone {
    fn drop(&mut self) {
        if let Some(mut c) = self.child.take() {
            let _ = c.kill();
            let _ = c.wait();
        }
    }
}

/// Frames the host published in `secs` seconds, as seen by the reader.
fn frames_in(reader: &FrameReader, secs: f64) -> u32 {
    let start = Instant::now();
    let mut last = reader.latest_frame();
    let mut n = 0;
    while start.elapsed().as_secs_f64() < secs {
        let now = reader.latest_frame();
        if now != last {
            n += 1;
            last = now;
        }
        std::thread::sleep(Duration::from_millis(2));
    }
    n
}

fn status_line(e: &CaptureEngine) -> String {
    let s = e.get_status();
    format!(
        "state={:?} out={}x{} camera={}x{} fps={:.1} standby={} consumer={} shm={} noSource={} picture={}x{} err={:?}",
        s.state, s.width, s.height, s.camera_width, s.camera_height, s.fps, s.standby,
        s.consumer_active, s.shared_memory, s.no_source, s.picture_width, s.picture_height, s.error
    )
}

fn luma_varies(reader: &mut FrameReader, w: u32, h: u32) -> bool {
    let mut buf = vec![0u8; (w * h * 3 / 2) as usize];
    let now = niwcam_shm::mf_system_time();
    let live = reader.fill(&mut buf, w as usize, w, h, now);
    let luma = &buf[..(w * h) as usize];
    let (mn, mx) = luma
        .iter()
        .fold((255u8, 0u8), |(a, b), &v| (a.min(v), b.max(v)));
    println!("    reader.fill live={live} luma {mn}..{mx}");
    live && mn != mx
}

#[test]
fn engine_end_to_end() {
    let tmp = std::env::temp_dir().join(format!("niwcam-engine-e2e-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&tmp);
    std::fs::create_dir_all(&tmp).unwrap();
    // Before anything reads the data folder or looks up the virtual camera API.
    std::env::set_var("CAMSTUDIO_DATA_DIR", tmp.join("data"));
    std::env::set_var("CAMSTUDIO_FORCE_DSHOW", "1");
    // SAFETY: plain COM / Media Foundation start-up on the test thread.
    unsafe {
        CoInitializeEx(None, COINIT_MULTITHREADED).ok().unwrap();
        MFStartup(MF_VERSION, MFSTARTUP_FULL).unwrap();
    }

    let phone = FakePhone::start(&tmp.join("phone"), &[]);

    // A phone source, as cs_load_formats leaves it: connected, paired, caps known.
    let link = Arc::new(PhoneLink::new());
    let ep = Endpoint {
        host: "127.0.0.1".into(),
        port: PORT,
        manual: true,
        ..Endpoint::default()
    };
    assert!(link.lease(30000));
    let caps = {
        let _lease = LeaseGuard::new(&link);
        link.connect(
            &ep,
            Some(&|code: &str| println!("pairing code {code:?}")),
            Some(&|| true),
        )
        .expect("pairing with the stand-in phone");
        link.request_caps(REQUEST_CAPS_DEFAULT_MS).expect("caps")
    };
    let lens = caps["lenses"].items()[0]["id"].str().to_string();
    let mode = &caps["modes"].items()[0];
    let (mw, mh) = (
        mode["width"].int(1280) as u32,
        mode["height"].int(720) as u32,
    );
    let fps = mode["fps"].items()[0].int(30) as u32;
    println!("caps: lens {lens} mode {mw}x{mh}@{fps}");
    let format = NativeFormat {
        index: 0,
        subtype: MFVideoFormat_H264,
        width: mw,
        height: mh,
        fps_num: fps,
        fps_den: 1,
        phone_camera: lens,
        lens: LensFacing::Back,
    };
    let cam = CameraDevice {
        name: "Điện thoại giả".into(),
        phone: Some(link.clone()),
        ..CameraDevice::default()
    };

    let names = Names::for_test(&format!("e2e{}", std::process::id()));
    let engine = CaptureEngine::with_names(names.clone());
    engine.set_preview_size(1280, 720);
    engine.set_preview_enabled(true);

    // --- start: running, 1280x720 ---------------------------------------------------------
    let id = engine.start(&cam, &format, &ImageSettings::default());
    assert_eq!(id, 1);
    assert!(
        wait_until(Duration::from_secs(20), || {
            let s = engine.get_status();
            s.state == State::Running && s.width == mw && s.camera_width == mw
        }),
        "never running: {}",
        status_line(&engine)
    );
    println!("running: {}", status_line(&engine));
    let mut reader = FrameReader::with_names(names.clone());
    assert!(
        wait_until(Duration::from_secs(5), || reader.open_existing(true)),
        "no shared memory"
    );
    reader.set_active(true); // an app is on the camera
    std::thread::sleep(Duration::from_millis(1500));
    let n = frames_in(&reader, 3.0);
    println!("reader saw {n} frames in 3 s: {}", status_line(&engine));
    assert!(
        (70..=105).contains(&n),
        "expected ~90 frames in 3 s, got {n}"
    );
    let s = engine.get_status();
    assert_eq!((s.state, s.width, s.height), (State::Running, mw, mh));
    assert_eq!((s.camera_width, s.camera_height), (mw, mh));
    assert!(s.shared_memory && s.consumer_active && !s.standby && !s.no_source);
    assert!((20.0..=40.0).contains(&s.fps), "fps {}", s.fps);
    assert!(s.error.is_empty(), "{}", s.error);
    assert_eq!(s.source_id, 1);
    assert_eq!(reader.width(), mw);
    assert!(luma_varies(&mut reader, mw, mh), "the picture is flat");

    // --- preview ----------------------------------------------------------------------------
    let (mut pw, mut ph, mut pid) = (0u32, 0u32, 0u64);
    engine.copy_preview(None, &mut pw, &mut ph, &mut pid);
    assert_eq!((pw, ph), (mw, mh), "preview size");
    let mut px = vec![0u32; (pw * ph) as usize];
    assert!(engine.copy_preview(Some(&mut px), &mut pw, &mut ph, &mut pid));
    assert_eq!(px[0] >> 24, 0xFF);
    std::thread::sleep(Duration::from_millis(1000));
    let (mut pw2, mut ph2, mut pid2) = (0u32, 0u32, 0u64);
    engine.copy_preview(None, &mut pw2, &mut ph2, &mut pid2);
    println!("preview id {pid} -> {pid2} in 1 s");
    assert!(pid2 >= pid + 20, "preview should advance ~30 a second");

    // --- the phone's state reaches the engine --------------------------------------------------
    println!("phone state: {:?}", engine.phone_state());

    // --- snapshot -----------------------------------------------------------------------------
    let png = tmp.join("snap.png");
    let hr = engine.snapshot(png.to_str().unwrap());
    let bytes = std::fs::read(&png).unwrap_or_default();
    println!("snapshot hr={:#010x} {} bytes", hr.0 as u32, bytes.len());
    assert!(hr.is_ok());
    assert!(bytes.starts_with(&[0x89, b'P', b'N', b'G']) && bytes.len() > 5000);

    // --- recording ----------------------------------------------------------------------------
    let mp4 = tmp.join("rec.mp4");
    let hr = engine.start_recording(mp4.to_str().unwrap(), false);
    assert!(hr.is_ok(), "start_recording {:#x}", hr.0 as u32);
    std::thread::sleep(Duration::from_secs(3));
    assert!(engine.recording().active());
    let elapsed = engine.recording().elapsed_ms();
    let hr = engine.stop_recording();
    let rec = std::fs::read(&mp4).unwrap_or_default();
    println!(
        "recording {elapsed} ms, stop hr={:#x}, {} bytes",
        hr.0 as u32,
        rec.len()
    );
    assert!(hr.is_ok() && elapsed >= 2500);
    assert!(rec.len() > 20_000 && &rec[4..8] == b"ftyp");

    // --- effects on frames that go somewhere: framing, smoothing, background must not break it ----
    engine.set_auto_frame(true);
    engine.set_smoothing(50);
    engine.set_background(BackgroundMode::Blur, 60, "", "");
    std::thread::sleep(Duration::from_millis(2500));
    let n = frames_in(&reader, 2.0);
    println!(
        "with effects: {n} frames in 2 s: {} framing={:?} bg={:?}",
        status_line(&engine),
        engine.get_framing(),
        engine.get_background()
    );
    assert!(n >= 40, "effects must not stall the camera ({n})");
    engine.set_auto_frame(false);
    engine.set_smoothing(0);
    engine.set_background(BackgroundMode::Off, 60, "", "");

    // --- standby: the app and the preview leave, the source closes after ~5 s ----------------------
    reader.set_active(false);
    engine.set_preview_enabled(false);
    assert!(
        wait_until(Duration::from_secs(15), || engine.get_status().standby),
        "no standby: {}",
        status_line(&engine)
    );
    let s = engine.get_status();
    println!("standby: {}", status_line(&engine));
    assert_eq!(s.fps, 0.0);
    assert_eq!(s.state, State::Running);
    std::thread::sleep(Duration::from_millis(1500));
    let n = frames_in(&reader, 2.0);
    assert!(n <= 1, "nothing should be written in standby, saw {n}");

    // --- the app returns: the source wakes ------------------------------------------------------
    reader.set_active(true);
    assert!(
        wait_until(Duration::from_secs(12), || {
            let s = engine.get_status();
            !s.standby && s.state == State::Running
        }),
        "no wake: {}",
        status_line(&engine)
    );
    std::thread::sleep(Duration::from_millis(1500));
    let n = frames_in(&reader, 2.0);
    println!("after wake: {n} frames in 2 s: {}", status_line(&engine));
    assert!(n >= 40, "frames should flow again ({n})");
    engine.set_preview_enabled(true);

    // --- no source, and the pause pictures ---------------------------------------------------------
    let nv12 = NativeFormat {
        width: mw,
        height: mh,
        fps_num: 30,
        fps_den: 1,
        subtype: MFVideoFormat_NV12,
        ..NativeFormat::default()
    };
    let image_path = png.to_str().unwrap().to_string();
    for (kind, label, path) in [
        (PauseKind::Card, "card", ""),
        (PauseKind::Blur, "blur", ""),
        (PauseKind::Image, "image", image_path.as_str()),
    ] {
        engine.set_pause_picture(kind, "Quay lại ngay", path);
        let id = engine.switch(&CameraDevice::default(), &nv12);
        assert!(
            wait_until(Duration::from_secs(10), || {
                let s = engine.get_status();
                s.no_source && s.source_id == id & 0x7FFF_FFFF
            }),
            "no-source never reached ({label}): {}",
            status_line(&engine)
        );
        std::thread::sleep(Duration::from_millis(1500)); // rendered, then written twice a second
        let n = frames_in(&reader, 3.0);
        println!("pause {label}: {n} frames in 3 s: {}", status_line(&engine));
        assert!(
            (4..=9).contains(&n),
            "{label}: expected ~6 writes in 3 s, got {n}"
        );
        assert!(
            luma_varies(&mut reader, mw, mh),
            "{label}: the pause picture is flat"
        );
        let s = engine.get_status();
        assert!(s.no_source && s.state == State::Running && s.error.is_empty());
        assert_eq!(
            (s.width, s.height),
            (mw, mh),
            "the camera keeps its size with no source"
        );
    }
    // The camera's own "no signal": the engine writes nothing.
    engine.set_pause_picture(PauseKind::NoSignal, "", "");
    std::thread::sleep(Duration::from_millis(1500));
    let n = frames_in(&reader, 3.0);
    println!("no signal: {n} frames in 3 s");
    assert!(n <= 1, "NoSignal writes nothing ({n})");
    // A snapshot with no source: the camera's last picture is stale by now: ERROR_NO_DATA.
    let hr = engine.snapshot(tmp.join("none.png").to_str().unwrap());
    println!("snapshot with no source: {:#010x}", hr.0 as u32);

    // --- back to the phone ------------------------------------------------------------------------------
    let id = engine.switch(&cam, &format);
    assert!(
        wait_until(Duration::from_secs(25), || {
            let s = engine.get_status();
            !s.no_source && s.state == State::Running && s.source_id == id && s.camera_width == mw
        }),
        "phone did not come back: {}",
        status_line(&engine)
    );
    std::thread::sleep(Duration::from_millis(1500));
    let n = frames_in(&reader, 2.0);
    println!(
        "back on the phone: {n} frames in 2 s: {}",
        status_line(&engine)
    );
    assert!(n >= 40);

    // --- stop ---------------------------------------------------------------------------------------------
    engine.stop();
    let s = engine.get_status();
    println!("stopped: {}", status_line(&engine));
    assert_eq!(s.state, State::Stopped);
    assert_eq!(s.fps, 0.0);
    let log = phone.log();
    assert!(
        log.contains("start"),
        "the phone was asked to start:\n{log}"
    );
    reader.close();
    drop(engine);
    drop(phone);
    let _ = std::fs::remove_dir_all(&tmp);
}
