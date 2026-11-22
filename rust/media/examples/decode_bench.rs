//! How long does the video decoder take per frame? (For finding why a high resolution or frame rate
//! cannot keep up.)  decode_bench <h264|hevc> <width> <height> <stream.bin> [rounds]
//! The stream comes from `xcheck/gen_stream.exe` (Windows' own encoder).

use std::io::Read;
use std::time::Instant;

use niwcam_media::video_decoder::VideoDecoder;
use windows::Win32::Media::MediaFoundation::{MFStartup, MFSTARTUP_FULL, MF_VERSION};
use windows::Win32::System::Com::{CoInitializeEx, COINIT_MULTITHREADED};

fn main() {
    let a: Vec<String> = std::env::args().collect();
    let hevc = a[1] == "hevc";
    let (w, h): (u32, u32) = (a[2].parse().unwrap(), a[3].parse().unwrap());
    let rounds: usize = a.get(5).and_then(|v| v.parse().ok()).unwrap_or(3);
    // SAFETY: process-wide COM / Media Foundation start.
    unsafe {
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
        let _ = MFStartup(MF_VERSION, MFSTARTUP_FULL);
    }
    let mut units: Vec<(i64, Vec<u8>)> = Vec::new();
    let mut f = std::fs::File::open(&a[4]).unwrap();
    loop {
        let mut hdr = [0u8; 13];
        if f.read_exact(&mut hdr).is_err() {
            break;
        }
        let size = u32::from_le_bytes(hdr[0..4].try_into().unwrap());
        let t = i64::from_le_bytes(hdr[4..12].try_into().unwrap());
        let mut data = vec![0u8; size as usize];
        f.read_exact(&mut data).unwrap();
        units.push((t, data));
    }
    let bytes: usize = units.iter().map(|u| u.1.len()).sum();
    println!(
        "{} {w}x{h}: {} access units, {:.1} KB each on average ({:.1} Mbps at 30 fps)",
        a[1],
        units.len(),
        bytes as f64 / units.len() as f64 / 1024.0,
        bytes as f64 / units.len() as f64 * 8.0 * 30.0 / 1e6
    );
    let mut dec = VideoDecoder::new();
    println!("init {:08x}", dec.init(hevc, w, h).0 as u32);
    let dec_hw = dec.is_hardware();
    let mut hash: u64 = 0xcbf29ce484222325;
    println!("hardware: {}", dec.is_hardware());
    for round in 0..rounds {
        let (mut frames, mut worst) = (0u32, 0f64);
        let t = Instant::now();
        for (i, (ts, data)) in units.iter().enumerate() {
            let t0 = Instant::now();
            let _ = dec.decode(data, *ts + (round * units.len() + i) as i64 * 333_333, &mut |nv12, pitch, w, h| {
                frames += 1;
                if round == 0 {
                    if let Some(dir) = std::env::var_os("DUMP_DIR") {
                        if frames <= 4 {
                            let (p, w, h) = (pitch as usize, w as usize, h as usize);
                            let mut out = Vec::with_capacity(w * h * 3 / 2);
                            for r in 0..h + h / 2 {
                                out.extend_from_slice(&nv12[r * p..r * p + w]);
                            }
                            let tag = if dec_hw { "hw" } else { "sw" };
                            let _ = std::fs::write(std::path::Path::new(&dir).join(format!("{tag}-{frames}.nv12")), out);
                        }
                    }
                    let before = hash;
                    // Visible pixels only (the pitch padding is not part of the picture).
                    let (p, w, h) = (pitch as usize, w as usize, h as usize);
                    for r in 0..h {
                        for &b in &nv12[r * p..r * p + w] {
                            hash = (hash ^ b as u64).wrapping_mul(0x100000001b3);
                        }
                    }
                    for r in 0..h / 2 {
                        for &b in &nv12[(h + r) * p..(h + r) * p + w] {
                            hash = (hash ^ b as u64).wrapping_mul(0x100000001b3);
                        }
                    }
                    if std::env::var_os("PER_FRAME").is_some() {
                        println!("frame {frames}: {:016x} pts {}", hash ^ before, 0);
                    }
                }
            });
            worst = worst.max(t0.elapsed().as_secs_f64() * 1000.0);
        }
        let ms = t.elapsed().as_secs_f64() * 1000.0;
        if round == 0 {
            println!("frame hash {hash:016x}");
        }
        println!(
            "round {round}: {frames} frames in {ms:.0} ms = {:.2} ms per frame ({:.0} fps possible), worst {worst:.1} ms",
            ms / frames.max(1) as f64,
            frames as f64 * 1000.0 / ms
        );
    }
}
