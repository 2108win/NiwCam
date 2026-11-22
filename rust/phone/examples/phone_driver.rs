//! Rust twin of `xcheck/cpp_phone.cpp`: same subcommands, same output lines, so `xcheck/run.ps1` can
//! diff them. Needs `CAMSTUDIO_DATA_DIR` (never touches the real data folder).
//!
//!   phone_driver datadir | clientid | clientname
//!   phone_driver keysave <deviceId> <hex> | keyload <deviceId> | keyhas <deviceId> | keyforget <deviceId>
//!   phone_driver connect <host> <port> known|auto|pair
//!   phone_driver stream <host> <port> <frames> [audio]
//!   phone_driver known <host> <port>
use std::collections::BTreeSet;
use std::time::{Duration, Instant};

use niwcam_core::crypto;
use niwcam_core::json::Value;
use niwcam_phone::discovery::{add_manual, known_phones, remove_manual, Endpoint};
use niwcam_phone::phone_link::{
    client_id, client_name, forget_pair_key, has_pair_key, load_pair_key, save_pair_key, Kind,
    Message, PhoneLink, Recv, REQUEST_CAPS_DEFAULT_MS,
};

fn make_endpoint(host: &str, port: &str) -> Endpoint {
    Endpoint {
        host: host.to_string(),
        port: port.parse().unwrap_or(0),
        name: host.to_string(),
        manual: true,
        ..Endpoint::default()
    }
}

fn nal_types(d: &[u8]) -> String {
    let mut out = String::new();
    let mut i = 0;
    while i + 3 < d.len() {
        let mut skip = 0;
        if d[i] == 0 && d[i + 1] == 0 && d[i + 2] == 1 {
            skip = 3;
        } else if i + 4 < d.len() && d[i] == 0 && d[i + 1] == 0 && d[i + 2] == 0 && d[i + 3] == 1 {
            skip = 4;
        }
        if skip == 0 {
            i += 1;
            continue;
        }
        if !out.is_empty() {
            out.push(',');
        }
        out.push_str(&(d[i + skip] & 0x1f).to_string());
        i += skip;
    }
    out
}

fn connect(host: &str, port: &str, mode: &str) -> i32 {
    let link = PhoneLink::new();
    let e = make_endpoint(host, port);
    let res = match mode {
        "pair" => link.connect(
            &e,
            Some(&|code: &str| {
                if !code.is_empty() {
                    println!("pairing code {code}");
                }
            }),
            Some(&|| true),
        ),
        "auto" => link.connect_auto(&e),
        _ => link.connect_known(&e),
    };
    if let Err(error) = res {
        println!("connect failed: {error} code={}", link.last_error_code());
        return 1;
    }
    println!(
        "connect ok deviceId={} name={}",
        link.device_id(),
        link.device_name()
    );
    0
}

fn stream(host: &str, port: &str, frames: i32, audio: bool) -> i32 {
    let link = PhoneLink::new();
    if let Err(error) = link.connect_known(&make_endpoint(host, port)) {
        println!("connect failed: {error} code={}", link.last_error_code());
        return 1;
    }
    let caps = match link.request_caps(REQUEST_CAPS_DEFAULT_MS) {
        Ok(c) => c,
        Err(error) => {
            println!("caps failed: {error}");
            return 1;
        }
    };
    let lens = &caps.get("lenses").items()[0];
    let mode = &caps.get("modes").items()[0];
    let join = |items: &[Value], f: &dyn Fn(&Value) -> String| {
        items.iter().map(f).collect::<Vec<_>>().join(",")
    };
    println!(
        "caps lens={} mode={}x{} fps={} codecs={} audio={}",
        lens.get("id").str(),
        mode.get("width").int(0),
        mode.get("height").int(0),
        join(mode.get("fps").items(), &|f| f.int(0).to_string()),
        join(mode.get("codecs").items(), &|c| c.str().to_string()),
        i32::from(caps.get("audio").is_object())
    );

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
    if !link.send_control(&start) {
        println!("start not sent");
        return 1;
    }

    let (mut video, mut config, mut audio_records, mut keyframes, mut other) = (0, 0, 0, 0, 0);
    let mut audio_bytes = 0usize;
    let mut first_nals = String::new();
    let mut started = String::new();
    let mut controls: BTreeSet<String> = BTreeSet::new();
    let begin = Instant::now();
    let mut m = Message::default();
    while video < frames && begin.elapsed() < Duration::from_secs(30) {
        match link.receive(&mut m, 1000) {
            Recv::Message => {}
            Recv::TimedOut => continue,
            Recv::Failed => {
                println!("receive failed");
                return 1;
            }
        }
        match m.kind {
            Kind::Control => {
                if let Ok(v) = m.json() {
                    controls.insert(v.get("type").str().to_string());
                    if v.get("type").str() == "started" {
                        started = format!(
                            "orientation={} fps={} audio.on={}",
                            v.get("orientation").int(0),
                            v.get("fps").int(0),
                            if v.get("audio").get("on").bool_or(false) {
                                "true"
                            } else {
                                "false"
                            }
                        );
                    }
                }
            }
            Kind::VideoConfig => config += 1,
            Kind::VideoFrame => {
                if video == 0 {
                    first_nals = nal_types(&m.payload);
                }
                if m.keyframe() {
                    keyframes += 1;
                }
                video += 1;
            }
            Kind::Audio => {
                audio_records += 1;
                audio_bytes += m.payload.len();
            }
            Kind::Other(_) => other += 1,
        }
    }
    let mut set = Value::object();
    set.set("type", "set").set("zoom", 2).set("torch", 0.5);
    let mut stats = Value::object();
    stats
        .set("type", "stats")
        .set("recvFps", 30)
        .set("decodeMs", 2.5)
        .set("queueMs", 12)
        .set("lost", 0);
    let mut kf = Value::object();
    kf.set("type", "keyframe");
    let mut stop = Value::object();
    stop.set("type", "stop");
    let sent = link.send_control(&set)
        && link.send_control(&stats)
        && link.send_control(&kf)
        && link.send_control(&stop);
    // Read what is still on its way: closing with unread data makes Windows send a reset, which can
    // drop the phone's not yet read messages (stats...) and make the phone log racy.
    while link.receive(&mut m, 400) == Recv::Message {}
    println!(
        "frames={video} keyframes>0={} config={config} audio_records_gt0={} other={other} first_nals={first_nals} controls={}",
        i32::from(keyframes > 0),
        i32::from(audio_records > 0),
        controls.iter().cloned().collect::<Vec<_>>().join(",")
    );
    println!(
        "started {started}\nsent set/stats/keyframe/stop={}",
        i32::from(sent)
    );
    if audio {
        println!(
            "audio_record_bytes_divisible_by_960={}",
            i32::from(audio_bytes.is_multiple_of(960))
        );
    }
    0
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let arg = |i: usize| args.get(i).map_or("", String::as_str);
    let code = match arg(1) {
        "datadir" => {
            println!("datadir={}", niwcam_phone::app_data_dir().display());
            0
        }
        "clientid" => {
            println!("clientid={}", client_id());
            0
        }
        "clientname" => {
            println!("clientname={}", client_name());
            0
        }
        "keysave" if args.len() > 3 => {
            save_pair_key(arg(2), &crypto::from_hex(arg(3)));
            println!("saved");
            0
        }
        "keyload" if args.len() > 2 => {
            println!("key={}", crypto::to_hex(&load_pair_key(arg(2))));
            0
        }
        "keyhas" if args.len() > 2 => {
            println!("has={}", i32::from(has_pair_key(arg(2))));
            0
        }
        "keyforget" if args.len() > 2 => {
            forget_pair_key(arg(2));
            println!("forgotten");
            0
        }
        "connect" if args.len() > 4 => connect(arg(2), arg(3), arg(4)),
        "stream" if args.len() > 4 => stream(
            arg(2),
            arg(3),
            arg(4).parse().unwrap_or(0),
            arg(5) == "audio",
        ),
        "known" if args.len() > 3 => {
            let port: u16 = arg(3).parse().unwrap_or(0);
            add_manual(arg(2), port);
            for e in known_phones() {
                println!(
                    "known host={} port={} name={} manual={}",
                    e.host,
                    e.port,
                    e.name,
                    i32::from(e.manual)
                );
            }
            remove_manual(arg(2), port);
            println!("after remove: {}", known_phones().len());
            0
        }
        _ => {
            eprintln!("usage: see the header of this file");
            2
        }
    };
    std::process::exit(code);
}
