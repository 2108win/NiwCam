//! Rust twin of xcheck/cpp_mf.cpp: same lines about this PC's webcams.
use niwcam_media::mf_devices::*;
use windows::core::GUID;
use windows::Win32::Media::MediaFoundation::*;
use windows::Win32::System::Com::{CoInitializeEx, COINIT_MULTITHREADED};

fn print_format(tag: &str, f: &NativeFormat) {
    println!(
        "{tag} idx={} {} {}x{} {}/{} fps={:.6} pixels={} rank={}",
        f.index,
        subtype_name(&f.subtype),
        f.width,
        f.height,
        f.fps_num,
        f.fps_den,
        f.fps(),
        f.pixels(),
        subtype_rank(&f.subtype)
    );
}

fn main() {
    // SAFETY: process-wide COM / Media Foundation start, as the C++ driver does.
    unsafe {
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
        let _ = MFStartup(MF_VERSION, MFSTARTUP_FULL);
    }
    let cams = enumerate_cameras();
    println!("cameras {}", cams.len());
    for (i, c) in cams.iter().enumerate() {
        println!(
            "camera {i} name=[{}] link=[{}] ours={} phone={} window={}",
            c.name,
            c.symbolic_link,
            is_our_virtual_camera(c) as i32,
            c.is_phone() as i32,
            c.is_pc_window() as i32
        );
        if is_our_virtual_camera(c) {
            println!("  formats skipped (our own virtual camera)");
            continue;
        }
        let Some(activate) = &c.activate else {
            continue;
        };
        // SAFETY: activate and shut the source down again, like the C++ driver.
        let source = match unsafe { activate.ActivateObject::<IMFMediaSource>() } {
            Ok(s) => s,
            Err(e) => {
                println!("  activate {:08x}", e.code().0 as u32);
                continue;
            }
        };
        let mut formats = Vec::new();
        let hr = get_native_formats(&source, &mut formats, None);
        // SAFETY: plain COM calls.
        unsafe {
            let _ = source.Shutdown();
            let _ = activate.ShutdownObject();
        }
        println!(
            "  GetNativeFormats {:08x} count {}",
            hr.0 as u32,
            formats.len()
        );
        for f in &formats {
            print_format("  fmt", f);
        }
        match pick_largest(&formats) {
            Some(b) => print_format("  largest", b),
            None => println!("  largest none"),
        }
        for o in build_format_options(&formats) {
            print_format(if o.largest { "  option*" } else { "  option " }, &o.native);
        }
    }
    for sel in [
        "",
        "0",
        "1",
        "99",
        "obsbot",
        "OBSBOT",
        "cam",
        "niwcam",
        "zzz-none",
        "Windows Virtual",
    ] {
        match select_camera(&cams, sel) {
            Some(c) => println!("select [{sel}] -> [{}]", c.name),
            None => println!("select [{sel}] -> none"),
        }
    }
    for (i, c) in cams.iter().enumerate() {
        match find_camera_again(c) {
            Some(f) => println!(
                "find-again {i} -> found [{}] link=[{}]",
                f.name, f.symbolic_link
            ),
            None => println!("find-again {i} -> none [] link=[]"),
        }
    }
    let ghost = CameraDevice {
        name: "No Such Camera".into(),
        symbolic_link: "\\\\?\\usb#vid_0000".into(),
        ..Default::default()
    };
    println!(
        "find-again ghost -> {}",
        if find_camera_again(&ghost).is_some() {
            "found"
        } else {
            "none"
        }
    );
    if let Some(c) = cams.iter().find(|c| !is_our_virtual_camera(c)) {
        let moved = CameraDevice {
            name: c.name.clone(),
            symbolic_link: "other-port".into(),
            ..Default::default()
        };
        println!(
            "find-again by-name [{}] -> {}",
            c.name,
            find_camera_again(&moved)
                .map(|f| f.name)
                .unwrap_or_else(|| "none".into())
        );
    }
    for n in [
        "NiwCam",
        "NiwCam (Windows Virtual Camera)",
        "Cam Studio",
        "Cam Studio (x)",
        "NiwCam2",
        "niwcam",
        "Cam Studio  (",
        "",
    ] {
        let d = CameraDevice {
            name: n.into(),
            ..Default::default()
        };
        println!("ours [{n}] -> {}", is_our_virtual_camera(&d) as i32);
    }
    for ch in [
        'A', 'Z', '\u{110}', '\u{C1}', '\u{416}', '\u{130}', '\u{1EA0}', '\u{D0}',
    ] {
        let l = to_lower(&ch.to_string());
        println!(
            "tolower {:04x} -> {:04x}",
            ch as u32,
            l.chars().next().unwrap() as u32
        );
    }
    let mut odd: GUID = MFVideoFormat_NV12;
    odd.data1 = 0x41424344;
    println!("subtype odd [{}]", subtype_name(&odd));
    odd.data1 = 0x00004142;
    println!("subtype odd2 [{}]", subtype_name(&odd));
    for g in [
        MFVideoFormat_NV12,
        MFVideoFormat_YUY2,
        MFVideoFormat_MJPG,
        MFVideoFormat_RGB24,
        MFVideoFormat_RGB32,
        MFVideoFormat_H264,
        MFVideoFormat_HEVC,
        MFVideoFormat_I420,
        MFVideoFormat_L8,
        MFVideoFormat_UYVY,
    ] {
        println!("subtype [{}] rank {}", subtype_name(&g), subtype_rank(&g));
    }
    let mut syn: Vec<NativeFormat> = Vec::new();
    let mut add = |sub: GUID, w: u32, h: u32, n: u32, d: u32| {
        syn.push(NativeFormat {
            index: syn.len() as u32,
            subtype: sub,
            width: w,
            height: h,
            fps_num: n,
            fps_den: d,
            ..Default::default()
        });
    };
    add(MFVideoFormat_MJPG, 1280, 720, 30, 1);
    add(MFVideoFormat_YUY2, 1280, 720, 30, 1);
    add(MFVideoFormat_NV12, 1280, 720, 30, 1);
    add(MFVideoFormat_H264, 1280, 720, 30, 1);
    add(MFVideoFormat_H264, 1920, 1080, 30, 1);
    add(MFVideoFormat_YUY2, 1920, 1080, 30, 1);
    add(MFVideoFormat_MJPG, 1920, 1080, 60, 1);
    add(MFVideoFormat_MJPG, 1920, 1080, 30000, 1001);
    add(MFVideoFormat_YUY2, 0, 0, 90, 1);
    add(MFVideoFormat_NV12, 640, 480, 0, 1);
    for o in build_format_options(&syn) {
        print_format(
            if o.largest {
                "syn option*"
            } else {
                "syn option "
            },
            &o.native,
        );
    }
    if let Some(b) = pick_largest(&syn) {
        print_format("syn largest", b);
    }
}
