//! Rust twin of xcheck/cpp_audio.cpp. `audio <output dir> [cable]`
use niwcam_media::audio_out::AudioOut;
use std::time::Duration;
use windows::Win32::System::Com::{CoInitializeEx, COINIT_MULTITHREADED};

fn sine(rate: u32, channels: u32, seconds: f64, freq: f64, phase0: f64) -> Vec<u8> {
    let frames = (rate as f64 * seconds) as usize;
    let mut v = Vec::with_capacity(frames * channels as usize * 2);
    for i in 0..frames {
        for c in 0..channels {
            let s = (12000.0
                * (phase0 + 2.0 * std::f64::consts::PI * freq * i as f64 / rate as f64).sin()
                * if c != 0 { 0.5 } else { 1.0 })
            .round() as i16;
            v.extend_from_slice(&s.to_le_bytes());
        }
    }
    v
}

fn file_hash(path: &str) -> (i64, u64) {
    match std::fs::read(path) {
        Ok(b) => {
            let mut h = 1469598103934665603u64;
            for &x in &b {
                h ^= x as u64;
                h = h.wrapping_mul(1099511628211);
            }
            (b.len() as i64, h)
        }
        Err(_) => (-1, 0),
    }
}

fn main() {
    let a: Vec<String> = std::env::args().collect();
    let dir = format!("{}\\", a[1]);
    // SAFETY: COM start like the C++ driver.
    unsafe {
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
    }

    {
        let au = AudioOut::new(&format!("wav:{dir}a.wav"));
        let pcm = sine(48000, 1, 1.0, 440.0, 0.0);
        println!(
            "A receiving-before {} problem [{}]",
            au.receiving() as i32,
            au.problem()
        );
        for chunk in pcm.chunks(480 * 2) {
            au.push(chunk, 48000, 1);
        }
        println!(
            "A receiving-after {} problem [{}]",
            au.receiving() as i32,
            au.problem()
        );
    }
    {
        let b = AudioOut::new(&format!("wav:{dir}b.wav"));
        let s1 = sine(44100, 2, 0.5, 1000.0, 0.0);
        let s2 = sine(48000, 1, 0.5, 300.0, 0.5);
        b.push(&[7], 44100, 2);
        b.push(&s1[..100], 0, 2);
        b.push(&s1[..100], 44100, 0);
        b.push(&s1[..1001], 44100, 2);
        b.push(&s1[1001..], 44100, 2);
        b.push(&s2, 48000, 1);
        println!("B problem [{}]", b.problem());
    }
    {
        let c = AudioOut::new(&format!("wav:{dir}no-such-dir\\c.wav"));
        let pcm = sine(48000, 1, 0.1, 440.0, 0.0);
        c.push(&pcm, 48000, 1);
        println!(
            "C receiving {} problem [{}]",
            c.receiving() as i32,
            c.problem()
        );
    }
    {
        let d = AudioOut::new(&format!("wav:{dir}d.wav"));
        println!("D receiving {}", d.receiving() as i32);
    }
    for n in ["a.wav", "b.wav", "c.wav", "d.wav"] {
        let (size, h) = file_hash(&format!("{dir}{n}"));
        println!("file {n} size {size} hash {h:016x}");
    }

    let outs = AudioOut::outputs();
    println!("outputs {}", outs.len());
    for (id, name) in &outs {
        println!("output id=[{id}] name=[{name}]");
    }

    for (tag, target) in [
        (
            "E",
            "{0.0.0.00000000}.{00000000-0000-0000-0000-000000000000}",
        ),
        ("F", "not-an-endpoint-id"),
    ] {
        let e = AudioOut::new(target);
        let pcm = sine(48000, 1, 0.1, 440.0, 0.0);
        e.push(&pcm, 48000, 1);
        for _ in 0..100 {
            if !e.problem().is_empty() {
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        println!("{tag} problem [{}]", e.problem());
    }
    if a.len() > 2 {
        let prefix = a.get(3).map(String::as_str).unwrap_or("CABLE Input");
        let cable = outs
            .iter()
            .filter(|(_, n)| n.starts_with(prefix))
            .map(|(i, _)| i.clone())
            .next_back();
        match cable {
            None => println!("G no cable device"),
            Some(id) => {
                let g = AudioOut::new(&id);
                let silence = vec![0u8; 480 * 2 * 2];
                for _ in 0..100 {
                    g.push(&silence, 48000, 2);
                    std::thread::sleep(Duration::from_millis(10));
                }
                println!(
                    "G cable problem [{}] receiving {}",
                    g.problem(),
                    g.receiving() as i32
                );
            }
        }
    }
}
