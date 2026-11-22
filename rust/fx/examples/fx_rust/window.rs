//! WindowSource check, Rust side: the same test window as `xcheck/window_cpp.cpp` (a plain Win32
//! window of our own, drawn with GDI, never a user's window) captured with the Rust port.

use std::sync::atomic::{AtomicBool, AtomicI32, AtomicIsize, Ordering};
use std::time::Instant;

use niwcam_fx::window_source::{
    capture_window_size, list_capture_windows, CaptureResult, WindowCapture,
};
use windows::core::{w, PCWSTR};
use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, RECT, WPARAM};
use windows::Win32::Graphics::Gdi::{
    BeginPaint, CreateSolidBrush, DeleteObject, EndPaint, FillRect, InvalidateRect, UpdateWindow,
    HDC, PAINTSTRUCT,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW, GetClientRect, GetMessageW,
    LoadCursorW, PeekMessageW, PostMessageW, PostQuitMessage, RegisterClassW, SetTimer,
    SetWindowPos, ShowWindow, SystemParametersInfoW, TranslateMessage, HWND_BOTTOM, IDC_ARROW, MSG,
    PM_REMOVE, SPI_GETWORKAREA, SWP_NOACTIVATE, SWP_NOMOVE, SWP_NOSIZE, SWP_NOZORDER,
    SW_SHOWNOACTIVATE, SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS, WM_APP, WM_DESTROY, WM_ERASEBKGND,
    WM_PAINT, WM_SIZE, WM_TIMER, WNDCLASSW, WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW, WS_POPUP,
};

use super::Ctx;

static STEP: AtomicI32 = AtomicI32::new(0);
static ANIMATE: AtomicBool = AtomicBool::new(false);
static HWND_PTR: AtomicIsize = AtomicIsize::new(0);

fn rgb(r: i32, g: i32, b: i32) -> windows::Win32::Foundation::COLORREF {
    windows::Win32::Foundation::COLORREF((r as u32) | ((g as u32) << 8) | ((b as u32) << 16))
}

/// The test pattern: a function of the size and STEP only, so both languages draw the same pixels.
fn draw(dc: HDC, w: i32, h: i32, step: i32) {
    // SAFETY: GDI calls on a DC from BeginPaint and brushes created and deleted right here.
    unsafe {
        let mut cy = 0;
        while cy * 20 < h {
            let mut cx = 0;
            while cx * 20 < w {
                let b = CreateSolidBrush(rgb(
                    (cx * 37) & 255,
                    (cy * 53) & 255,
                    ((cx + cy) * 29) & 255,
                ));
                let r = RECT {
                    left: cx * 20,
                    top: cy * 20,
                    right: cx * 20 + 20,
                    bottom: cy * 20 + 20,
                };
                FillRect(dc, &r, b);
                let _ = DeleteObject(b.into());
                cx += 1;
            }
            cy += 1;
        }
        if step > 0 {
            let white = CreateSolidBrush(rgb(255, 255, 255));
            let x = (step * 12) % 1.max(w - 30);
            let r = RECT {
                left: x,
                top: h / 2 - 15,
                right: x + 30,
                bottom: h / 2 + 15,
            };
            FillRect(dc, &r, white);
            let _ = DeleteObject(white.into());
        }
    }
}

unsafe extern "system" fn proc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    // SAFETY: a window procedure of our own window; every call below is the documented use.
    unsafe {
        match msg {
            WM_PAINT => {
                let mut ps = PAINTSTRUCT::default();
                let dc = BeginPaint(hwnd, &mut ps);
                let mut rc = RECT::default();
                let _ = GetClientRect(hwnd, &mut rc);
                draw(dc, rc.right, rc.bottom, STEP.load(Ordering::SeqCst));
                let _ = EndPaint(hwnd, &ps);
                LRESULT(0)
            }
            WM_ERASEBKGND => LRESULT(1),
            WM_SIZE => {
                let _ = InvalidateRect(Some(hwnd), None, false);
                LRESULT(0)
            }
            WM_TIMER => {
                if ANIMATE.load(Ordering::SeqCst) {
                    STEP.fetch_add(1, Ordering::SeqCst);
                    let _ = InvalidateRect(Some(hwnd), None, false);
                }
                LRESULT(0)
            }
            m if m == WM_APP + 1 => {
                // From the capture thread: destroy (a window is destroyed on its own thread).
                let _ = DestroyWindow(hwnd);
                LRESULT(0)
            }
            WM_DESTROY => {
                PostQuitMessage(0);
                LRESULT(0)
            }
            _ => DefWindowProcW(hwnd, msg, wp, lp),
        }
    }
}

fn start_window(x: i32, y: i32, w: i32, h: i32) -> std::thread::JoinHandle<()> {
    let (tx, rx) = std::sync::mpsc::channel::<()>();
    let handle = std::thread::spawn(move || {
        // SAFETY: registers and creates one window on this thread, then pumps its messages here.
        unsafe {
            let hinstance = GetModuleHandleW(PCWSTR::null()).unwrap_or_default();
            let class = w!("NiwCamFxTestWindow");
            let wc = WNDCLASSW {
                lpfnWndProc: Some(proc),
                hInstance: hinstance.into(),
                lpszClassName: class,
                hCursor: LoadCursorW(None, IDC_ARROW).unwrap_or_default(),
                ..Default::default()
            };
            RegisterClassW(&wc);
            // No frame (pure content), never activated, off the taskbar: it must not disturb anyone.
            let hwnd = CreateWindowExW(
                WS_EX_NOACTIVATE | WS_EX_TOOLWINDOW,
                class,
                w!("NiwCam fx test window"),
                WS_POPUP,
                x,
                y,
                w,
                h,
                None,
                None,
                Some(hinstance.into()),
                None,
            )
            .unwrap_or_default();
            HWND_PTR.store(hwnd.0 as isize, Ordering::SeqCst);
            let _ = ShowWindow(hwnd, SW_SHOWNOACTIVATE);
            // Under everything else: windows in front of it hide it from the user, not from the capture.
            let _ = SetWindowPos(
                hwnd,
                Some(HWND_BOTTOM),
                0,
                0,
                0,
                0,
                SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE,
            );
            let _ = UpdateWindow(hwnd);
            SetTimer(Some(hwnd), 1, 33, None);
            let _ = tx.send(());
            let mut m = MSG::default();
            while GetMessageW(&mut m, None, 0, 0).0 > 0 {
                let _ = TranslateMessage(&m);
                DispatchMessageW(&m);
            }
        }
    });
    let _ = rx.recv_timeout(std::time::Duration::from_secs(5));
    handle
}

/// Dispatches the messages waiting for this thread (the WinEvent hook behind "Closed" needs it).
fn pump() {
    let mut m = MSG::default();
    // SAFETY: drains this thread's own queue.
    unsafe {
        while PeekMessageW(&mut m, None, 0, 0, PM_REMOVE).as_bool() {
            let _ = TranslateMessage(&m);
            DispatchMessageW(&m);
        }
    }
}

fn fnv(d: &[u8]) -> u64 {
    let mut h: u64 = 1469598103934665603;
    for &b in d {
        h = (h ^ b as u64).wrapping_mul(1099511628211);
    }
    h
}

fn name(r: CaptureResult) -> &'static str {
    match r {
        CaptureResult::Frame => "Frame",
        CaptureResult::Timeout => "Timeout",
        CaptureResult::Closed => "Closed",
        CaptureResult::Error => "Error",
    }
}

fn ms(t0: Instant) -> f64 {
    t0.elapsed().as_secs_f64() * 1000.0
}

struct Seen {
    last: Vec<u8>, // the newest frame, tightly packed BGRA
    last_w: u32,
    last_h: u32,
    sizes: Vec<String>, // sizes of the frames handed to the callback, consecutive duplicates merged
    callbacks: u64,
}

impl Seen {
    fn on_frame(&mut self, bgra: &[u8], pitch: u32, w: u32, h: u32) {
        self.callbacks += 1;
        self.last.resize(w as usize * h as usize * 4, 0);
        for row in 0..h as usize {
            self.last[row * w as usize * 4..(row + 1) * w as usize * 4].copy_from_slice(
                &bgra[row * pitch as usize..row * pitch as usize + w as usize * 4],
            );
        }
        self.last_w = w;
        self.last_h = h;
        let s = format!("{w}x{h}");
        if self.sizes.last() != Some(&s) {
            self.sizes.push(s);
        }
    }
}

/// Bottom right corner of the work area, room for the 520 x 340 the window grows to (a window that is
/// off screen is not composed: its frames are empty).
fn default_spot() -> (i32, i32) {
    let mut wa = RECT {
        left: 0,
        top: 0,
        right: 1920,
        bottom: 1080,
    };
    // SAFETY: SPI_GETWORKAREA writes one RECT.
    unsafe {
        let _ = SystemParametersInfoW(
            SPI_GETWORKAREA,
            0,
            Some(&mut wa as *mut RECT as *mut _),
            SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS(0),
        );
    }
    (wa.right - 560, wa.bottom - 380)
}

pub fn cmd_window(c: &mut Ctx, x: i32, y: i32) -> i32 {
    let (x, y) = if x == i32::MIN {
        default_spot()
    } else {
        (x, y)
    };
    let thread = start_window(x, y, 400, 300);
    let hwnd = HWND(HWND_PTR.load(Ordering::SeqCst) as *mut _);
    let mut timing = std::fs::File::create(format!("{}\\timing.txt", c.outdir)).unwrap();
    use std::io::Write as _;

    let (cw, ch) = capture_window_size(hwnd);
    c.line(&format!("capture_window_size {cw}x{ch}"));
    {
        let list = list_capture_windows();
        let mut all = String::new();
        for w in &list {
            all.push_str(&w.title);
            all.push('\n');
        }
        c.line(&format!(
            "list_capture_windows count={} titles_fnv={:016x}",
            list.len(),
            fnv(all.as_bytes())
        ));
    }

    let mut cap = WindowCapture::new();
    let mut error = String::new();
    if !cap.open(hwnd, &mut error) {
        c.line(&format!("open FAILED {error}"));
        return 1;
    }
    c.line("open ok");

    let mut seen = Seen {
        last: Vec::new(),
        last_w: 0,
        last_h: 0,
        sizes: Vec::new(),
        callbacks: 0,
    };
    let join = |v: &[String]| v.join(">");
    // Drain until the window has been quiet for `quiet_ms`.
    fn drain(cap: &mut WindowCapture, seen: &mut Seen, quiet_ms: i32) {
        for _ in 0..200 {
            if cap.next(quiet_ms, true, &mut |b, p, w, h| seen.on_frame(b, p, w, h))
                != CaptureResult::Frame
            {
                break;
            }
        }
    }
    fn save(c: &mut Ctx, seen: &Seen, name: &str) {
        let path = format!("{}\\{}.bgra", c.outdir, name);
        let mut data = Vec::new();
        data.extend_from_slice(&seen.last_w.to_le_bytes());
        data.extend_from_slice(&seen.last_h.to_le_bytes());
        data.extend_from_slice(&seen.last);
        let _ = std::fs::write(path, data);
        c.line(&format!(
            "{} size={}x{} fnv={:016x}",
            name,
            seen.last_w,
            seen.last_h,
            fnv(&seen.last)
        ));
    }

    // P1: a still window. The first frame arrives at once; after it nothing, until the window changes.
    let mut r = CaptureResult::Timeout;
    for _ in 0..40 {
        if r == CaptureResult::Frame {
            break;
        }
        r = cap.next(100, true, &mut |b, p, w, h| seen.on_frame(b, p, w, h));
    }
    c.line(&format!("first_frame result={}", name(r)));
    drain(&mut cap, &mut seen, 300);
    save(c, &seen, "win_static");
    let r = cap.next(300, true, &mut |b, p, w, h| seen.on_frame(b, p, w, h));
    c.line(&format!("still_next {}", name(r)));

    // P2: an animated window (a timer repaints it ~30 times a second) for 3 s, read.
    seen.sizes.clear();
    seen.callbacks = 0;
    ANIMATE.store(true, Ordering::SeqCst);
    let mut frames = 0u64;
    let t0 = Instant::now();
    while ms(t0) < 3000.0 {
        if cap.next(100, true, &mut |b, p, w, h| seen.on_frame(b, p, w, h)) == CaptureResult::Frame
        {
            frames += 1;
        }
    }
    c.line(&format!(
        "anim_3s_read sizes={} frames_ok={} callbacks_eq_frames={}",
        join(&seen.sizes),
        (frames >= 45) as i32,
        (seen.callbacks == frames) as i32
    ));
    let _ = writeln!(
        timing,
        "anim_3s_read frames={} callbacks={}",
        frames, seen.callbacks
    );
    // The same without reading: frames are released unread, the callback is never called.
    seen.callbacks = 0;
    frames = 0;
    let t0 = Instant::now();
    while ms(t0) < 1000.0 {
        if cap.next(100, false, &mut |b, p, w, h| seen.on_frame(b, p, w, h)) == CaptureResult::Frame
        {
            frames += 1;
        }
    }
    c.line(&format!(
        "anim_1s_unread frames_ok={} callbacks={}",
        (frames >= 15) as i32,
        seen.callbacks
    ));
    let _ = writeln!(timing, "anim_1s_unread frames={frames}");
    // Stop the animation and let the last frames through.
    ANIMATE.store(false, Ordering::SeqCst);
    STEP.store(0, Ordering::SeqCst);
    // SAFETY: invalidates our own window.
    unsafe {
        let _ = InvalidateRect(Some(hwnd), None, false);
    }
    drain(&mut cap, &mut seen, 300);

    // P3: resizes. The pool is recreated at the new size; frames in between are cut to what fits.
    for (label, w, h) in [
        ("resize_up", 520, 340),
        ("resize_down", 300, 200),
        ("resize_odd", 333, 221),
    ] {
        seen.sizes.clear();
        // SAFETY: resizes our own window without moving, activating or re-stacking it.
        unsafe {
            let _ = SetWindowPos(
                hwnd,
                None,
                0,
                0,
                w,
                h,
                SWP_NOMOVE | SWP_NOZORDER | SWP_NOACTIVATE,
            );
        }
        let (qw, qh) = capture_window_size(hwnd);
        let t0 = Instant::now();
        let mut guard = 0;
        while ms(t0) < 1500.0 && guard < 200 {
            guard += 1;
            // The window repaints all the time, so frames keep coming at the new size (after the
            // pool is recreated nothing is sent for a window that does not change).
            // SAFETY: invalidates our own window.
            unsafe {
                let _ = InvalidateRect(Some(hwnd), None, false);
            }
            cap.next(150, true, &mut |b, p, w, h| seen.on_frame(b, p, w, h));
        }
        c.line(&format!(
            "{label} capture_window_size={qw}x{qh} final={}x{}",
            seen.last_w, seen.last_h
        ));
        let _ = writeln!(timing, "{label} sizes={}", join(&seen.sizes));
        save(c, &seen, &format!("win_{label}"));
    }

    // P4: the window closes. The "Closed" notice (the item's Closed event) is raised through a WinEvent
    // hook, so it only arrives while the thread that opened the capture pumps messages: first 2 s
    // without a pump, then with one.
    let t0 = Instant::now();
    // SAFETY: posts a private message to our own window.
    unsafe {
        let _ = PostMessageW(Some(hwnd), WM_APP + 1, WPARAM(0), LPARAM(0));
    }
    let mut cr = CaptureResult::Frame;
    let mut closed_no_pump = false;
    while ms(t0) < 2000.0 {
        cr = cap.next(150, true, &mut |b, p, w, h| seen.on_frame(b, p, w, h));
        if cr == CaptureResult::Closed {
            closed_no_pump = true;
            break;
        }
    }
    c.line(&format!(
        "destroy_without_message_pump closed={}",
        closed_no_pump as i32
    ));
    let t0 = Instant::now();
    for _ in 0..100 {
        if cr == CaptureResult::Closed {
            break;
        }
        pump();
        cr = cap.next(150, true, &mut |b, p, w, h| seen.on_frame(b, p, w, h));
    }
    let _ = writeln!(timing, "closed_after_pump {:.0} ms", ms(t0));
    let again = cap.next(100, true, &mut |b, p, w, h| seen.on_frame(b, p, w, h));
    c.line(&format!(
        "after_destroy last={} again={}",
        name(cr),
        name(again)
    ));
    cap.close();
    let r = cap.next(50, true, &mut |b, p, w, h| seen.on_frame(b, p, w, h));
    c.line(&format!("after_close_next {}", name(r)));
    // Opening a window that is gone, and no window at all: the Vietnamese message.
    let (mut e1, mut e2) = (String::new(), String::new());
    let o1 = cap.open(hwnd, &mut e1);
    let o2 = cap.open(HWND(std::ptr::null_mut()), &mut e2);
    c.line(&format!("open_destroyed ok={} msg={}", o1 as i32, e1));
    c.line(&format!("open_null ok={} msg={}", o2 as i32, e2));
    let _ = thread.join();
    0
}

/// Captures another process's window (the C++ `windowhost`) and reports what `next` says when that
/// window is destroyed, with no message pump on this thread (the engine's capture thread has none).
pub fn cmd_closewatch(c: &mut Ctx, hwnd_value: i64) -> i32 {
    use std::io::Write as _;
    let mut timing = std::fs::File::create(format!("{}\\timing.txt", c.outdir)).unwrap();
    let hwnd = HWND(hwnd_value as isize as *mut _);
    let mut cap = WindowCapture::new();
    let mut error = String::new();
    if !cap.open(hwnd, &mut error) {
        c.line(&format!("closewatch open FAILED {error}"));
        return 1;
    }
    let mut frames = 0u64;
    let t0 = Instant::now();
    let mut r = CaptureResult::Frame;
    let mut seq: Vec<&str> = Vec::new();
    while ms(t0) < 14000.0 {
        r = cap.next(100, true, &mut |_, _, _, _| frames += 1);
        if seq.last() != Some(&name(r)) {
            seq.push(name(r));
        }
        if r == CaptureResult::Closed || r == CaptureResult::Error {
            break;
        }
    }
    c.line(&format!(
        "closewatch last={} frames_seen={}",
        name(r),
        (frames > 10) as i32
    ));
    let _ = writeln!(
        timing,
        "closewatch after {:.0} ms seq={}",
        ms(t0),
        seq.join(">")
    );
    0
}
