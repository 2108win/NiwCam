//! Rust twin of xcheck/cpp_decode.cpp: same input file, same output lines.
use niwcam_media::video_decoder::VideoDecoder;
use std::io::Read;
use windows::Win32::Media::MediaFoundation::{MFStartup, MFSTARTUP_FULL, MF_VERSION};
use windows::Win32::System::Com::{CoInitializeEx, COINIT_MULTITHREADED};

fn fnv(data: &[u8], mut h: u64) -> u64 {
    for &b in data {
        h ^= b as u64;
        h = h.wrapping_mul(1099511628211);
    }
    h
}

fn main() {
    let a: Vec<String> = std::env::args().collect();
    let hevc = a[1] == "hevc";
    let (w, h): (u32, u32) = (a[2].parse().unwrap(), a[3].parse().unwrap());
    // SAFETY: process-wide COM / Media Foundation start, as the C++ driver does.
    unsafe {
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
        let _ = MFStartup(MF_VERSION, MFSTARTUP_FULL);
    }
    let mut dec = VideoDecoder::new();
    let junk = [1u8, 2, 3, 4, 5, 6, 7, 8];
    println!(
        "decode-before-init {:08x}",
        dec.decode(&junk, 0, &mut |_, _, _, _| {}).0 as u32
    );
    println!("init {:08x}", dec.init(hevc, w, h).0 as u32);
    let mut f = std::fs::File::open(&a[4]).unwrap();
    let (mut au, mut frames) = (0u32, 0u32);
    loop {
        let mut hdr = [0u8; 13];
        if f.read_exact(&mut hdr).is_err() {
            break;
        }
        let size = u32::from_le_bytes(hdr[0..4].try_into().unwrap());
        let t = i64::from_le_bytes(hdr[4..12].try_into().unwrap());
        let key = hdr[12];
        let mut data = vec![0u8; size as usize];
        f.read_exact(&mut data).unwrap();
        let hr = dec.decode(&data, t, &mut |nv12, pitch, fw, fh| {
            let mut hash = 1469598103934665603u64;
            let p = pitch as usize;
            for r in 0..fh as usize {
                hash = fnv(&nv12[r * p..r * p + fw as usize], hash);
            }
            let uv = &nv12[p * fh as usize..];
            for r in 0..(fh / 2) as usize {
                hash = fnv(&uv[r * p..r * p + fw as usize], hash);
            }
            println!("frame {frames} {fw}x{fh} pitch {pitch} {hash:016x}");
            frames += 1;
        });
        println!("au {au} size {size} key {key} hr {:08x}", hr.0 as u32);
        au += 1;
    }
    let mut garbage = [0u8; 64];
    let mut s = 12345u32;
    for g in garbage.iter_mut() {
        s = s.wrapping_mul(1664525).wrapping_add(1013904223);
        *g = (s >> 24) as u8;
    }
    println!(
        "garbage {:08x}",
        dec.decode(&garbage, 99, &mut |_, _, _, _| {}).0 as u32
    );
    dec.reset();
    println!(
        "after-reset {:08x}",
        dec.decode(&junk, 0, &mut |_, _, _, _| {}).0 as u32
    );
}
