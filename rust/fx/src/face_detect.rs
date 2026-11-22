//! Face detection for automatic framing: `Windows.Media.FaceAnalysis.FaceTracker` on its own
//! thread, fed a small grey copy of the picture a few times a second. Nothing runs (no thread,
//! no tracker) until the first frame is offered, so framing turned off costs nothing.
//! Port of `src/engine/FaceDetect.h/.cpp`.
//!
//! The worker runs in the multithreaded apartment, where blocking on an async call is allowed.
//! Frames go in as NV12 (what Microsoft's face tracking sample feeds it: our grey copy plus
//! neutral colour), or Gray8 where only that works.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::JoinHandle;

use niwcam_core::auto_frame::{detection_size, downscale_luma, FaceRect};
use windows::core::Interface;
use windows::Foundation::{IReference, PropertyValue, TimeSpan};
use windows::Graphics::Imaging::{BitmapBufferAccessMode, BitmapPixelFormat, SoftwareBitmap};
use windows::Media::FaceAnalysis::FaceTracker;
use windows::Media::VideoFrame;
use windows::Win32::System::Performance::{QueryPerformanceCounter, QueryPerformanceFrequency};
use windows::Win32::System::WinRT::{
    IMemoryBufferByteAccess, RoInitialize, RoUninitialize, RO_INIT_MULTITHREADED,
};

/// A frame is worth offering now when the last one is done and this many ms passed.
pub const K_INTERVAL_MS: f64 = 200.0;
/// Long side of the grey copy.
pub const K_MAX_SIDE: u32 = 640;
/// Frames in a row that may fail before the detector gives up.
pub const K_MAX_ERRORS: i32 = 10;

struct State {
    quit: bool,
    busy: bool, // a frame waits or is being looked at
    last_offer_ms: f64,
    job: Vec<u8>, // handed to the worker
    job_width: u32,
    job_height: u32,
    job_generation: u32,
    result: Vec<FaceRect>,
    result_generation: u32,
    has_result: bool,
}

struct Shared {
    state: Mutex<State>,
    wake: Condvar,
    unsupported: AtomicBool,
    gave_up: AtomicBool,
}

pub struct FaceDetector {
    shared: Arc<Shared>,
    thread: Mutex<Option<JoinHandle<()>>>,
    scratch: Mutex<Vec<u8>>, // capture thread: the grey copy being made
}

impl Default for FaceDetector {
    fn default() -> Self {
        Self::new()
    }
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

impl Drop for FaceDetector {
    fn drop(&mut self) {
        {
            lock(&self.shared.state).quit = true;
        }
        self.shared.wake.notify_all();
        let handle = self
            .thread
            .get_mut()
            .map(|t| t.take())
            .unwrap_or_else(|e| e.into_inner().take());
        if let Some(h) = handle {
            let _ = h.join();
        }
    }
}

impl FaceDetector {
    pub fn new() -> FaceDetector {
        FaceDetector {
            shared: Arc::new(Shared {
                state: Mutex::new(State {
                    quit: false,
                    busy: false,
                    last_offer_ms: -1e9,
                    job: Vec::new(),
                    job_width: 0,
                    job_height: 0,
                    job_generation: 0,
                    result: Vec::new(),
                    result_generation: 0,
                    has_result: false,
                }),
                wake: Condvar::new(),
                unsupported: AtomicBool::new(false),
                gave_up: AtomicBool::new(false),
            }),
            thread: Mutex::new(None),
            scratch: Mutex::new(Vec::new()),
        }
    }

    /// Capture thread. A frame is worth offering now: the last one is done and `K_INTERVAL_MS`
    /// passed.
    pub fn due(&self, now_ms: f64) -> bool {
        let st = lock(&self.shared.state);
        if self.shared.unsupported.load(Ordering::Relaxed)
            || self.shared.gave_up.load(Ordering::Relaxed)
            || now_ms - st.last_offer_ms < K_INTERVAL_MS
        {
            return false;
        }
        !st.busy
    }

    /// Copies a small grey version of this luma plane for the detector. `generation` tags the
    /// result: [`take_result`](Self::take_result) drops results of another generation (a frame
    /// from before a rotation).
    pub fn offer(
        &self,
        luma: &[u8],
        pitch: usize,
        width: u32,
        height: u32,
        generation: u32,
        now_ms: f64,
    ) {
        lock(&self.shared.state).last_offer_ms = now_ms;
        // A plane that is not all there would be read out of bounds in the C++: not offered.
        if width < 2 || height < 2 || luma.len() < (height as usize - 1) * pitch + width as usize {
            return;
        }
        let (w, h) = detection_size(width, height, K_MAX_SIDE);
        let mut scratch = lock(&self.scratch);
        scratch.resize(w as usize * h as usize, 0);
        downscale_luma(luma, pitch, width, height, &mut scratch, w, h);
        {
            let mut st = lock(&self.shared.state);
            if st.busy {
                return;
            }
            std::mem::swap(&mut st.job, &mut scratch);
            st.job_width = w;
            st.job_height = h;
            st.job_generation = generation;
            st.busy = true;
            let mut thread = lock(&self.thread);
            if thread.is_none() {
                let shared = Arc::clone(&self.shared);
                // The C++ std::thread throws if it cannot start; here the offer is lost and
                // the next due frame tries again.
                match std::thread::Builder::new()
                    .name("niwcam-face".into())
                    .spawn(move || worker(&shared))
                {
                    Ok(h) => *thread = Some(h),
                    Err(_) => st.busy = false,
                }
            }
        }
        self.shared.wake.notify_one();
    }

    /// The newest finished detection of this generation, once.
    pub fn take_result(&self, generation: u32, faces: &mut Vec<FaceRect>) -> bool {
        let mut st = lock(&self.shared.state);
        if !st.has_result {
            return false;
        }
        st.has_result = false;
        if st.result_generation != generation {
            return false;
        }
        std::mem::swap(faces, &mut st.result);
        true
    }

    /// 1 works (or not tried yet); 0 Windows has no face tracker (e.g. N editions without the
    /// Media Feature Pack); -1 it failed `K_MAX_ERRORS` frames in a row and was given up.
    pub fn state(&self) -> i32 {
        if self.shared.unsupported.load(Ordering::Relaxed) {
            0
        } else if self.shared.gave_up.load(Ordering::Relaxed) {
            -1
        } else {
            1
        }
    }
}

/// `std::chrono::steady_clock::now()` in 100 ns ticks, computed the way MSVC's steady_clock does
/// (the performance counter), for the frame's `RelativeTime`.
fn steady_now_100ns() -> i64 {
    let (mut freq, mut ctr) = (0i64, 0i64);
    // SAFETY: both calls only write the integers passed.
    unsafe {
        let _ = QueryPerformanceFrequency(&mut freq);
        let _ = QueryPerformanceCounter(&mut ctr);
    }
    let freq = freq.max(1);
    let whole = (ctr / freq) * 1_000_000_000;
    let part = (ctr % freq) * 1_000_000_000 / freq;
    (whole + part) / 100
}

/// Fills one SoftwareBitmap with the grey picture (plus neutral chroma for NV12), tracks faces
/// in it and returns them as fractions of the frame.
fn detect(
    tracker: &FaceTracker,
    nv12: bool,
    pixels: &[u8],
    w: u32,
    h: u32,
) -> windows::core::Result<Vec<FaceRect>> {
    let format = if nv12 {
        BitmapPixelFormat::Nv12
    } else {
        BitmapPixelFormat::Gray8
    };
    let bitmap = SoftwareBitmap::Create(format, w as i32, h as i32)?;
    {
        let buffer = bitmap.LockBuffer(BitmapBufferAccessMode::Write)?;
        let reference = buffer.CreateReference()?;
        let mut data: *mut u8 = std::ptr::null_mut();
        let mut capacity = 0u32;
        // SAFETY: GetBuffer hands out the locked buffer's memory; it stays valid until
        // `reference` / `buffer` are closed below, and every write is bounds-checked against
        // `capacity`.
        unsafe {
            reference
                .cast::<IMemoryBufferByteAccess>()?
                .GetBuffer(&mut data, &mut capacity)?;
            // Plane 0: the grey picture. NV12's plane 1 (interleaved U, V at half height): 128,
            // no colour.
            let luma = buffer.GetPlaneDescription(0)?;
            for y in 0..h as usize {
                let at = luma.StartIndex as usize + y * luma.Stride as usize;
                if at + w as usize > capacity as usize {
                    break;
                }
                std::ptr::copy_nonoverlapping(
                    pixels.as_ptr().add(y * w as usize),
                    data.add(at),
                    w as usize,
                );
            }
            if nv12 {
                let chroma = buffer.GetPlaneDescription(1)?;
                for y in 0..(h / 2) as usize {
                    let at = chroma.StartIndex as usize + y * chroma.Stride as usize;
                    if at + w as usize > capacity as usize {
                        break;
                    }
                    std::ptr::write_bytes(data.add(at), 128, w as usize);
                }
            }
        }
        reference.Close()?;
        buffer.Close()?;
    }
    let frame = VideoFrame::CreateWithSoftwareBitmap(&bitmap)?;
    // The tracker follows faces over time and needs each frame's time; without it,
    // ProcessNextFrameAsync fails with E_FAIL on every frame (measured 2026-10-19).
    let now = TimeSpan {
        Duration: steady_now_100ns(),
    };
    let boxed: IReference<TimeSpan> = PropertyValue::CreateTimeSpan(now)?.cast()?;
    frame.SetRelativeTime(&boxed)?;
    frame.SetSystemRelativeTime(&boxed)?;
    let mut found = Vec::new();
    for face in &tracker.ProcessNextFrameAsync(&frame)?.join()? {
        let b = face.FaceBox()?;
        found.push(FaceRect {
            x: b.X as f64 / w as f64,
            y: b.Y as f64 / h as f64,
            w: b.Width as f64 / w as f64,
            h: b.Height as f64 / h as f64,
        });
    }
    frame.Close()?;
    Ok(found)
}

fn worker(shared: &Shared) {
    let mut apartment = false;
    let mut tracker: Option<FaceTracker> = None;
    let mut nv12 = true;
    // SAFETY: plain apartment initialisation of this thread, undone at its end when it worked.
    let init = unsafe { RoInitialize(RO_INIT_MULTITHREADED) };
    let setup = (|| -> windows::core::Result<()> {
        init?;
        apartment = true;
        nv12 = FaceTracker::IsBitmapPixelFormatSupported(BitmapPixelFormat::Nv12)?;
        if !FaceTracker::IsSupported()?
            || (!nv12 && !FaceTracker::IsBitmapPixelFormatSupported(BitmapPixelFormat::Gray8)?)
        {
            shared.unsupported.store(true, Ordering::Relaxed);
        } else {
            tracker = Some(FaceTracker::CreateAsync()?.join()?);
        }
        Ok(())
    })();
    if setup.is_err() {
        shared.unsupported.store(true, Ordering::Relaxed);
    }
    let mut errors = 0;

    let mut pixels: Vec<u8> = Vec::new();
    loop {
        let (w, h, generation);
        {
            let mut st = lock(&shared.state);
            while !(st.quit || st.busy) {
                st = shared.wake.wait(st).unwrap_or_else(|e| e.into_inner());
            }
            if st.quit {
                break;
            }
            std::mem::swap(&mut pixels, &mut st.job);
            w = st.job_width;
            h = st.job_height;
            generation = st.job_generation;
        }

        let mut found = Vec::new();
        let mut ok = false;
        if !shared.unsupported.load(Ordering::Relaxed) && !shared.gave_up.load(Ordering::Relaxed) {
            let outcome = tracker.as_ref().map(|t| {
                std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    detect(t, nv12, &pixels, w, h)
                }))
            });
            match outcome {
                Some(Ok(Ok(f))) => {
                    found = f;
                    ok = true;
                    errors = 0;
                }
                _ => {
                    // One bad frame is skipped; a run of them gives up (framing then says so,
                    // and turning it off and on starts a new detector).
                    errors += 1;
                    if errors >= K_MAX_ERRORS {
                        shared.gave_up.store(true, Ordering::Relaxed);
                    }
                }
            }
        }

        let mut st = lock(&shared.state);
        st.busy = false;
        if ok {
            std::mem::swap(&mut st.result, &mut found);
            st.result_generation = generation;
            st.has_result = true;
        }
    }
    drop(tracker);
    if apartment {
        // SAFETY: balances the successful RoInitialize above, on the same thread.
        unsafe { RoUninitialize() };
    }
}
