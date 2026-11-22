//! A window on this PC as the virtual camera's source (like OBS "Window Capture"), through
//! Windows.Graphics.Capture: a free-threaded frame pool (no dispatcher on the capture thread),
//! the newest frame copied into a staging texture and mapped for the CPU.
//! Port of `src/engine/WindowSource.h/.cpp`.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use windows::core::IInspectable;
use windows::core::{h, Interface, BOOL};
use windows::Foundation::Metadata::ApiInformation;
use windows::Foundation::TypedEventHandler;
use windows::Graphics::Capture::{
    Direct3D11CaptureFramePool, GraphicsCaptureAccess, GraphicsCaptureAccessKind,
    GraphicsCaptureItem, GraphicsCaptureSession,
};
use windows::Graphics::DirectX::Direct3D11::IDirect3DDevice;
use windows::Graphics::DirectX::DirectXPixelFormat;
use windows::Graphics::SizeInt32;
use windows::Win32::Foundation::{CloseHandle, HANDLE, HMODULE, HWND, LPARAM, RECT, WAIT_OBJECT_0};
use windows::Win32::Graphics::Direct3D::{D3D_DRIVER_TYPE_HARDWARE, D3D_DRIVER_TYPE_WARP};
use windows::Win32::Graphics::Direct3D11::{
    D3D11CreateDevice, ID3D11Device, ID3D11DeviceContext, ID3D11Texture2D, D3D11_BOX,
    D3D11_CPU_ACCESS_READ, D3D11_CREATE_DEVICE_BGRA_SUPPORT, D3D11_MAPPED_SUBRESOURCE,
    D3D11_MAP_READ, D3D11_SDK_VERSION, D3D11_TEXTURE2D_DESC, D3D11_USAGE_STAGING,
};
use windows::Win32::Graphics::Dwm::{
    DwmGetWindowAttribute, DWMWA_CLOAKED, DWMWA_EXTENDED_FRAME_BOUNDS,
};
use windows::Win32::Graphics::Dxgi::Common::DXGI_SAMPLE_DESC;
use windows::Win32::Graphics::Dxgi::IDXGIDevice;
use windows::Win32::System::Threading::{
    CreateEventW, GetCurrentProcessId, SetEvent, WaitForSingleObject,
};
use windows::Win32::System::WinRT::Direct3D11::{
    CreateDirect3D11DeviceFromDXGIDevice, IDirect3DDxgiInterfaceAccess,
};
use windows::Win32::System::WinRT::Graphics::Capture::IGraphicsCaptureItemInterop;
use windows::Win32::UI::WindowsAndMessaging::{
    EnumWindows, GetShellWindow, GetWindow, GetWindowLongPtrW, GetWindowRect, GetWindowTextLengthW,
    GetWindowTextW, GetWindowThreadProcessId, IsWindowVisible, GWL_EXSTYLE, GW_OWNER,
    WS_EX_TOOLWINDOW,
};

pub struct WindowInfo {
    pub hwnd: HWND,
    pub title: String,
}

/// Top-level windows worth offering: visible, titled, not cloaked (another virtual desktop, a
/// suspended Store app), not tool windows, and not NiwCam's own.
pub fn list_capture_windows() -> Vec<WindowInfo> {
    let mut out: Vec<WindowInfo> = Vec::new();
    unsafe extern "system" fn each(hwnd: HWND, param: LPARAM) -> BOOL {
        // SAFETY: `param` is the `&mut Vec<WindowInfo>` passed to EnumWindows below, which
        // outlives the (synchronous) enumeration.
        let list = unsafe { &mut *(param.0 as *mut Vec<WindowInfo>) };
        // SAFETY: plain window queries on a handle EnumWindows just gave us.
        unsafe {
            if !IsWindowVisible(hwnd).as_bool()
                || GetWindow(hwnd, GW_OWNER).is_ok_and(|w| !w.0.is_null())
                || hwnd == GetShellWindow()
            {
                return BOOL(1);
            }
            if GetWindowLongPtrW(hwnd, GWL_EXSTYLE) & WS_EX_TOOLWINDOW.0 as isize != 0 {
                return BOOL(1);
            }
            let mut pid = 0u32;
            GetWindowThreadProcessId(hwnd, Some(&mut pid));
            if pid == GetCurrentProcessId() {
                return BOOL(1); // NiwCam's own windows
            }
            let mut cloaked = BOOL(0);
            let _ = DwmGetWindowAttribute(
                hwnd,
                DWMWA_CLOAKED,
                &mut cloaked as *mut BOOL as *mut _,
                std::mem::size_of::<BOOL>() as u32,
            );
            if cloaked.as_bool() {
                return BOOL(1);
            }
            let length = GetWindowTextLengthW(hwnd);
            if length <= 0 {
                return BOOL(1);
            }
            let mut title = vec![0u16; length as usize + 1];
            let got = GetWindowTextW(hwnd, &mut title);
            title.truncate(got.max(0) as usize);
            if !title.is_empty() {
                list.push(WindowInfo {
                    hwnd,
                    title: String::from_utf16_lossy(&title),
                });
            }
        }
        BOOL(1)
    }
    // SAFETY: `each` only touches `out` through the pointer below, during this call.
    unsafe {
        let _ = EnumWindows(
            Some(each),
            LPARAM(&mut out as *mut Vec<WindowInfo> as isize),
        );
    }
    out
}

/// The window's size on screen without its shadow (what capture frames will be), even.
pub fn capture_window_size(window: HWND) -> (u32, u32) {
    let mut r = RECT::default();
    // SAFETY: both calls write the RECT passed.
    unsafe {
        if DwmGetWindowAttribute(
            window,
            DWMWA_EXTENDED_FRAME_BOUNDS,
            &mut r as *mut RECT as *mut _,
            std::mem::size_of::<RECT>() as u32,
        )
        .is_err()
        {
            let _ = GetWindowRect(window, &mut r);
        }
    }
    let width = 2.max(((r.right - r.left).max(0) as u32) & !1);
    let height = 2.max(((r.bottom - r.top).max(0) as u32) & !1);
    (width, height)
}

/// What the capture callbacks touch. They run on the thread pool and may still be running when
/// `close()` revokes them, so they share this instead of pointing into `Impl`.
struct CaptureSignal {
    frame_event: HANDLE,
    closed: AtomicBool,
}

// SAFETY: an event handle may be signalled from any thread, and `closed` is atomic.
unsafe impl Send for CaptureSignal {}
// SAFETY: see above; nothing in here is mutated except through the atomic and the kernel.
unsafe impl Sync for CaptureSignal {}

impl CaptureSignal {
    fn new() -> CaptureSignal {
        // SAFETY: creates an auto-reset, initially non-signalled, unnamed event.
        let frame_event = unsafe { CreateEventW(None, false, false, None) }.unwrap_or_default();
        CaptureSignal {
            frame_event,
            closed: AtomicBool::new(false),
        }
    }

    fn set(&self) {
        if !self.frame_event.is_invalid() {
            // SAFETY: the handle is ours and alive as long as `self`.
            let _ = unsafe { SetEvent(self.frame_event) };
        }
    }
}

impl Drop for CaptureSignal {
    fn drop(&mut self) {
        if !self.frame_event.is_invalid() {
            // SAFETY: closes the event created in `new`, once.
            let _ = unsafe { CloseHandle(self.frame_event) };
        }
    }
}

struct Impl {
    d3d: ID3D11Device,
    context: ID3D11DeviceContext,
    device: IDirect3DDevice,
    item: GraphicsCaptureItem,
    pool: Direct3D11CaptureFramePool,
    session: GraphicsCaptureSession,
    frame_token: i64,
    closed_token: i64,
    signal: Arc<CaptureSignal>,
    pool_size: SizeInt32,
    staging: Option<ID3D11Texture2D>,
    staging_w: u32,
    staging_h: u32,
}

pub struct WindowCapture {
    m: Option<Box<Impl>>,
}

impl Default for WindowCapture {
    fn default() -> Self {
        Self::new()
    }
}

/// BGRA rows, top-down, valid only during the call: (pixels, pitch, width, height).
pub type FrameCallback<'a> = dyn FnMut(&[u8], u32, u32, u32) + 'a;

/// Error: capture failed for now (device lost, a transient WinRT error): reopen and go on.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CaptureResult {
    Frame,
    Timeout,
    Closed,
    Error,
}

impl Drop for WindowCapture {
    fn drop(&mut self) {
        self.close();
    }
}

fn create_device(
    driver: windows::Win32::Graphics::Direct3D::D3D_DRIVER_TYPE,
) -> windows::core::Result<(ID3D11Device, ID3D11DeviceContext)> {
    let mut d3d: Option<ID3D11Device> = None;
    let mut context: Option<ID3D11DeviceContext> = None;
    // SAFETY: the out pointers are live locals; no adapter, no software module, no feature list.
    unsafe {
        D3D11CreateDevice(
            None::<&windows::Win32::Graphics::Dxgi::IDXGIAdapter>,
            driver,
            HMODULE::default(),
            D3D11_CREATE_DEVICE_BGRA_SUPPORT,
            None,
            D3D11_SDK_VERSION,
            Some(&mut d3d),
            None,
            Some(&mut context),
        )?;
    }
    match (d3d, context) {
        (Some(d), Some(c)) => Ok((d, c)),
        _ => Err(windows::Win32::Foundation::E_POINTER.into()),
    }
}

impl WindowCapture {
    pub fn new() -> WindowCapture {
        WindowCapture { m: None }
    }

    /// Starts capturing. False, with a message for the user in `error`, if Windows cannot
    /// capture it. Call on a thread with COM (MTA) initialised.
    pub fn open(&mut self, window: HWND, error: &mut String) -> bool {
        self.close();
        let attempt = || -> Result<Result<Box<Impl>, String>, windows::core::Error> {
            if !GraphicsCaptureSession::IsSupported()? {
                return Ok(Err(
                    "Windows trên máy này không quay được cửa sổ (cần Windows 10 1903 trở lên)"
                        .into(),
                ));
            }
            let (d3d, context) = match create_device(D3D_DRIVER_TYPE_HARDWARE)
                .or_else(|_| create_device(D3D_DRIVER_TYPE_WARP))
            {
                Ok(v) => v,
                Err(_) => return Ok(Err("Không tạo được Direct3D để quay cửa sổ".into())),
            };
            let dxgi: IDXGIDevice = d3d.cast()?;
            // SAFETY: wraps the DXGI device we just made; the result is an owned WinRT object.
            let inspectable = unsafe { CreateDirect3D11DeviceFromDXGIDevice(&dxgi) }?;
            let device: IDirect3DDevice = inspectable.cast()?;

            let interop =
                windows::core::factory::<GraphicsCaptureItem, IGraphicsCaptureItemInterop>()?;
            // SAFETY: `window` is whatever the caller passed; CreateForWindow fails cleanly on a bad one.
            let item: GraphicsCaptureItem = unsafe { interop.CreateForWindow(window) }?;

            let pool_size = item.Size()?;
            let pool = Direct3D11CaptureFramePool::CreateFreeThreaded(
                &device,
                DirectXPixelFormat::B8G8R8A8UIntNormalized,
                2,
                pool_size,
            )?;
            let signal = Arc::new(CaptureSignal::new());
            let s = Arc::clone(&signal);
            let frame_token = pool.FrameArrived(&TypedEventHandler::<
                Direct3D11CaptureFramePool,
                IInspectable,
            >::new(move |_, _| {
                // A panic must not unwind into the WinRT thread pool.
                let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| s.set()));
                Ok(())
            }))?;
            let s = Arc::clone(&signal);
            let closed_token = item.Closed(&TypedEventHandler::<
                GraphicsCaptureItem,
                IInspectable,
            >::new(move |_, _| {
                let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    s.closed.store(true, Ordering::SeqCst);
                    s.set();
                }));
                Ok(())
            }))?;
            let session = pool.CreateCaptureSession(&item)?;
            // No yellow border around the window where Windows allows it (Windows 11).
            if ApiInformation::IsPropertyPresent(
                h!("Windows.Graphics.Capture.GraphicsCaptureSession"),
                h!("IsBorderRequired"),
            )
            .unwrap_or(false)
            {
                let borderless = (|| -> windows::core::Result<()> {
                    GraphicsCaptureAccess::RequestAccessAsync(
                        GraphicsCaptureAccessKind::Borderless,
                    )?
                    .join()?;
                    session.SetIsBorderRequired(false)
                })();
                // On failure the border stays; capture works the same.
                let _ = borderless;
            }
            session.StartCapture()?;
            Ok(Ok(Box::new(Impl {
                d3d,
                context,
                device,
                item,
                pool,
                session,
                frame_token,
                closed_token,
                signal,
                pool_size,
                staging: None,
                staging_w: 0,
                staging_h: 0,
            })))
        };
        match attempt() {
            Ok(Ok(impl_)) => {
                self.m = Some(impl_);
                true
            }
            Ok(Err(message)) => {
                *error = message;
                false
            }
            Err(e) => {
                *error = format!("Không quay được cửa sổ này ({})", e.message());
                false
            }
        }
    }

    /// Waits up to `timeout_ms` for a new picture (a window sends one only when it changes).
    /// The newest frame is read back from the GPU and passed to `on_frame` only when `read` is
    /// true (nobody watching: released unread). `on_frame` gets BGRA rows, top-down, valid only
    /// during the call: (pixels, pitch, width, height).
    pub fn next(
        &mut self,
        timeout_ms: i32,
        read: bool,
        on_frame: &mut FrameCallback<'_>,
    ) -> CaptureResult {
        let Some(m) = self.m.as_mut() else {
            return CaptureResult::Closed;
        };
        if m.signal.closed.load(Ordering::SeqCst) {
            return CaptureResult::Closed;
        }
        // SAFETY: waits on the event owned by `m.signal`, alive for the call.
        let wait = unsafe { WaitForSingleObject(m.signal.frame_event, timeout_ms as u32) };
        if m.signal.closed.load(Ordering::SeqCst) {
            return CaptureResult::Closed;
        }
        if wait != WAIT_OBJECT_0 {
            return CaptureResult::Timeout;
        }
        match Self::next_frame(m, read, on_frame) {
            Ok(r) => r,
            // Only the item's Closed event means the window is gone.
            Err(_) => {
                if m.signal.closed.load(Ordering::SeqCst) {
                    CaptureResult::Closed
                } else {
                    CaptureResult::Error
                }
            }
        }
    }

    fn next_frame(
        m: &mut Impl,
        read: bool,
        on_frame: &mut FrameCallback<'_>,
    ) -> windows::core::Result<CaptureResult> {
        // Only the newest frame matters: release older ones at once (the pool holds two).
        let mut frame = None;
        loop {
            match m.pool.TryGetNextFrame() {
                Ok(next) => {
                    if let Some(old) = frame.replace(next) {
                        let old: windows::Graphics::Capture::Direct3D11CaptureFrame = old;
                        old.Close()?;
                    }
                }
                // A null frame comes back as an empty error (code 0): the queue is empty.
                Err(e) if e.code().is_ok() => break,
                Err(e) => return Err(e),
            }
        }
        let Some(frame) = frame else {
            return Ok(CaptureResult::Timeout);
        };
        let size = frame.ContentSize()?;
        if read && size.Width >= 2 && size.Height >= 2 {
            let access: IDirect3DDxgiInterfaceAccess = frame.Surface()?.cast()?;
            // SAFETY: the surface is a texture of the device we created.
            let texture: ID3D11Texture2D = unsafe { access.GetInterface() }?;
            let mut desc = D3D11_TEXTURE2D_DESC::default();
            // SAFETY: GetDesc writes the struct passed.
            unsafe { texture.GetDesc(&mut desc) };
            if m.staging.is_none() || m.staging_w != desc.Width || m.staging_h != desc.Height {
                let s = D3D11_TEXTURE2D_DESC {
                    Usage: D3D11_USAGE_STAGING,
                    BindFlags: 0,
                    CPUAccessFlags: D3D11_CPU_ACCESS_READ.0 as u32,
                    MiscFlags: 0,
                    MipLevels: 1,
                    ArraySize: 1,
                    SampleDesc: DXGI_SAMPLE_DESC {
                        Count: 1,
                        Quality: 0,
                    },
                    ..desc
                };
                m.staging = None;
                let mut staging: Option<ID3D11Texture2D> = None;
                // SAFETY: `s` and `staging` are live locals.
                unsafe { m.d3d.CreateTexture2D(&s, None, Some(&mut staging)) }?;
                m.staging = staging;
                m.staging_w = desc.Width;
                m.staging_h = desc.Height;
            }
            // The content may be smaller than the pool's texture (the window shrank).
            let w = (size.Width as u32).min(desc.Width);
            let h = (size.Height as u32).min(desc.Height);
            let bx = D3D11_BOX {
                left: 0,
                top: 0,
                front: 0,
                right: w,
                bottom: h,
                back: 1,
            };
            if let Some(staging) = m.staging.as_ref() {
                let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
                // SAFETY: copies within the textures' bounds (w, h are clamped to the source),
                // then maps the staging texture; the slice handed out covers the mapped rows
                // and is dropped before Unmap.
                unsafe {
                    m.context
                        .CopySubresourceRegion(staging, 0, 0, 0, 0, &texture, 0, Some(&bx));
                    if m.context
                        .Map(staging, 0, D3D11_MAP_READ, 0, Some(&mut mapped))
                        .is_ok()
                    {
                        let len = mapped.RowPitch as usize * h as usize;
                        let pixels = std::slice::from_raw_parts(mapped.pData as *const u8, len);
                        on_frame(pixels, mapped.RowPitch, w, h);
                        m.context.Unmap(staging, 0);
                    }
                }
            }
        }
        frame.Close()?;
        if size.Width >= 2
            && size.Height >= 2
            && (size.Width != m.pool_size.Width || size.Height != m.pool_size.Height)
        {
            // Resized: frames from now on at the window's new size.
            m.pool_size = size;
            m.pool.Recreate(
                &m.device,
                DirectXPixelFormat::B8G8R8A8UIntNormalized,
                2,
                size,
            )?;
        }
        Ok(CaptureResult::Frame)
    }

    pub fn close(&mut self) {
        let Some(m) = self.m.take() else {
            return;
        };
        // Errors end the sequence and are ignored, as in the C++ (the window or the device may
        // be gone already).
        let _ = (|| -> windows::core::Result<()> {
            m.pool.RemoveFrameArrived(m.frame_token)?;
            m.item.RemoveClosed(m.closed_token)?;
            m.session.Close()?;
            m.pool.Close()
        })();
        // The signal lives on while a callback still holds it.
    }
}
