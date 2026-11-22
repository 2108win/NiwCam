//! Still pictures for the virtual camera while the picture is paused (`cs_set_pause_picture`):
//! a card with text, the last picture blurred with the text on it, or an image file. Rendered
//! once per setting and camera size (Direct2D, DirectWrite, WIC), never per frame.
//! Port of `src/engine/StillPicture.h/.cpp`; the same API calls with the same numbers, so the
//! pixels are the ones the C++ draws.
//!
//! Like the C++, nothing here initialises COM: the calling thread must have done it already.
//! (The "no signal" picture is not here: it is drawn by the virtual camera, `FrameReader.cpp`.)

use niwcam_core::nv12::{
    bgra_to_nv12, nv12_crop_scale_bilinear, nv12_scale, nv12_to_bgra, Nv12Rect,
};
use windows::core::{GUID, HRESULT, PCWSTR};
use windows::Win32::Foundation::{
    E_INVALIDARG, GENERIC_READ, GENERIC_WRITE, S_OK, WINCODEC_ERR_UNSUPPORTEDPIXELFORMAT,
};
use windows::Win32::Graphics::Direct2D::Common::{
    D2D1_ALPHA_MODE_PREMULTIPLIED, D2D1_COLOR_F, D2D1_GRADIENT_STOP, D2D1_PIXEL_FORMAT, D2D_RECT_F,
};
use windows::Win32::Graphics::Direct2D::{
    D2D1CreateFactory, ID2D1Factory, ID2D1RenderTarget, D2D1_BITMAP_INTERPOLATION_MODE_LINEAR,
    D2D1_DRAW_TEXT_OPTIONS_NONE, D2D1_ELLIPSE, D2D1_EXTEND_MODE_CLAMP,
    D2D1_FACTORY_TYPE_SINGLE_THREADED, D2D1_FEATURE_LEVEL_DEFAULT, D2D1_GAMMA_2_2,
    D2D1_LINEAR_GRADIENT_BRUSH_PROPERTIES, D2D1_RENDER_TARGET_PROPERTIES,
    D2D1_RENDER_TARGET_TYPE_SOFTWARE, D2D1_RENDER_TARGET_USAGE_NONE, D2D1_ROUNDED_RECT,
};
use windows::Win32::Graphics::DirectWrite::{
    DWriteCreateFactory, IDWriteFactory, DWRITE_FACTORY_TYPE_SHARED, DWRITE_FONT_STRETCH_NORMAL,
    DWRITE_FONT_STYLE_NORMAL, DWRITE_FONT_WEIGHT_SEMI_BOLD, DWRITE_MEASURING_MODE_NATURAL,
    DWRITE_PARAGRAPH_ALIGNMENT_NEAR, DWRITE_TEXT_ALIGNMENT_CENTER, DWRITE_WORD_WRAPPING_WRAP,
};
use windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT_B8G8R8A8_UNORM;
use windows::Win32::Graphics::Imaging::{
    CLSID_WICImagingFactory, GUID_ContainerFormatPng, GUID_WICPixelFormat32bppBGRA,
    GUID_WICPixelFormat32bppPBGRA, IWICBitmap, IWICImagingFactory, WICBitmapCacheOnLoad,
    WICBitmapDitherTypeNone, WICBitmapEncoderNoCache, WICBitmapLockRead, WICBitmapLockWrite,
    WICBitmapPaletteTypeCustom, WICDecodeMetadataCacheOnDemand, WICRect,
};
use windows::Win32::Storage::FileSystem::DeleteFileW;
use windows::Win32::System::Com::{CoCreateInstance, CLSCTX_INPROC_SERVER};
use windows_numerics::Vector2;

/// Bytes of an NV12 picture (`CamStudioNv12Bytes`).
fn nv12_bytes(w: u32, h: u32) -> usize {
    (w as u64 * h as u64 * 3 / 2) as usize
}

/// UTF-16 with a closing NUL, for the APIs that take a `PCWSTR`.
fn wide_z(s: &[u16]) -> Vec<u16> {
    let mut v = s.to_vec();
    v.push(0);
    v
}

/// A strong blur: average blocks of `block` pixels, then scale back up smoothly. Darkened, so
/// white text reads on any picture.
fn blur_nv12(src: &[u8], w: u32, h: u32, block: u32, out: &mut Vec<u8>) {
    let sw = 2.max((w / block) & !1);
    let sh = 2.max((h / block) & !1);
    let mut tiny = vec![0u8; nv12_bytes(sw, sh)];
    let (wz, swz) = (w as usize, sw as usize);
    let src_uv = &src[wz * h as usize..];
    let (tiny_y, tiny_uv) = tiny.split_at_mut(swz * sh as usize);
    for y in 0..sh {
        for x in 0..sw {
            let x0 = x * w / sw;
            let x1 = (x0 + 1).max((x + 1) * w / sw);
            let y0 = y * h / sh;
            let y1 = (y0 + 1).max((y + 1) * h / sh);
            let mut sum = 0u64;
            for yy in y0..y1 {
                for xx in x0..x1 {
                    sum += src[yy as usize * wz + xx as usize] as u64;
                }
            }
            let n = (x1 - x0) as u64 * (y1 - y0) as u64;
            // Darker (16 is black): 60 % of the way from black. The C++ does this in uint64_t:
            // a mean below 16 wraps around, and so does this.
            tiny_y[y as usize * swz + x as usize] =
                16u64.wrapping_add((sum / n).wrapping_sub(16).wrapping_mul(6) / 10) as u8;
        }
    }
    for y in 0..sh / 2 {
        for x in 0..sw / 2 {
            let x0 = x * (w / 2) / (sw / 2);
            let x1 = (x0 + 1).max((x + 1) * (w / 2) / (sw / 2));
            let y0 = y * (h / 2) / (sh / 2);
            let y1 = (y0 + 1).max((y + 1) * (h / 2) / (sh / 2));
            let (mut su, mut sv) = (0u64, 0u64);
            for yy in y0..y1 {
                for xx in x0..x1 {
                    su += src_uv[yy as usize * wz + 2 * xx as usize] as u64;
                    sv += src_uv[yy as usize * wz + 2 * xx as usize + 1] as u64;
                }
            }
            let n = (x1 - x0) as u64 * (y1 - y0) as u64;
            tiny_uv[y as usize * swz + 2 * x as usize] = (su / n) as u8;
            tiny_uv[y as usize * swz + 2 * x as usize + 1] = (sv / n) as u8;
        }
    }
    out.resize(nv12_bytes(w, h), 0);
    let all = Nv12Rect {
        x: 0.0,
        y: 0.0,
        w: sw as f64,
        h: sh as f64,
    };
    nv12_crop_scale_bilinear(&tiny, swz, sw, sh, &all, out, w, h);
}

/// `D2D1::ColorF(0xRRGGBB)`.
fn color_rgb(rgb: u32) -> D2D1_COLOR_F {
    D2D1_COLOR_F {
        r: ((rgb & 0xff0000) >> 16) as f32 / 255.0,
        g: ((rgb & 0xff00) >> 8) as f32 / 255.0,
        b: (rgb & 0xff) as f32 / 255.0,
        a: 1.0,
    }
}

/// `D2D1::ColorF(r, g, b, a)`.
fn color_rgba(r: f32, g: f32, b: f32, a: f32) -> D2D1_COLOR_F {
    D2D1_COLOR_F { r, g, b, a }
}

/// `D2D1::RectF`.
fn rect_f(left: f32, top: f32, right: f32, bottom: f32) -> D2D_RECT_F {
    D2D_RECT_F {
        left,
        top,
        right,
        bottom,
    }
}

/// A software Direct2D target drawing into a WIC bitmap.
struct Canvas {
    wic: IWICImagingFactory,
    bitmap: IWICBitmap,
    target: ID2D1RenderTarget,
}

impl Canvas {
    fn create(w: u32, h: u32) -> Option<Canvas> {
        // SAFETY: plain COM creation calls; every pointer passed is a live local, and the
        // interfaces returned are owned (released on drop).
        unsafe {
            let wic: IWICImagingFactory =
                CoCreateInstance(&CLSID_WICImagingFactory, None, CLSCTX_INPROC_SERVER).ok()?;
            let bitmap = wic
                .CreateBitmap(w, h, &GUID_WICPixelFormat32bppPBGRA, WICBitmapCacheOnLoad)
                .ok()?;
            let d2d: ID2D1Factory =
                D2D1CreateFactory(D2D1_FACTORY_TYPE_SINGLE_THREADED, None).ok()?;
            let props = D2D1_RENDER_TARGET_PROPERTIES {
                r#type: D2D1_RENDER_TARGET_TYPE_SOFTWARE,
                pixelFormat: D2D1_PIXEL_FORMAT {
                    format: DXGI_FORMAT_B8G8R8A8_UNORM,
                    alphaMode: D2D1_ALPHA_MODE_PREMULTIPLIED,
                },
                dpiX: 0.0,
                dpiY: 0.0,
                usage: D2D1_RENDER_TARGET_USAGE_NONE,
                minLevel: D2D1_FEATURE_LEVEL_DEFAULT,
            };
            let target = d2d.CreateWicBitmapRenderTarget(&bitmap, &props).ok()?;
            Some(Canvas {
                wic,
                bitmap,
                target,
            })
        }
    }

    /// The bitmap's pixels, as NV12.
    fn to_nv12(&self, w: u32, h: u32, nv12: &mut Vec<u8>) -> bool {
        let all = WICRect {
            X: 0,
            Y: 0,
            Width: w as i32,
            Height: h as i32,
        };
        // SAFETY: the lock keeps the pixel buffer alive and unchanged while `lock` lives; the
        // slice is built from the size WIC reports and dropped before the lock.
        unsafe {
            let Ok(lock) = self.bitmap.Lock(&all, WICBitmapLockRead.0 as u32) else {
                return false;
            };
            let stride = lock.GetStride().unwrap_or(0);
            let mut size = 0u32;
            let mut data: *mut u8 = std::ptr::null_mut();
            if lock.GetDataPointer(&mut size, &mut data).is_err() || data.is_null() {
                return false;
            }
            // The C++ trusts the stride; a buffer too small for the rows would be read out of
            // bounds there, so it is refused here.
            if (size as u64) < (stride as u64) * (h as u64 - 1) + w as u64 * 4 {
                return false;
            }
            let bgra = std::slice::from_raw_parts(data, size as usize);
            nv12.resize(nv12_bytes(w, h), 0);
            let (y, uv) = nv12.split_at_mut(w as usize * h as usize);
            bgra_to_nv12(bgra, stride as usize, w, h, y, uv, w as usize);
            true
        }
    }
}

/// NV12 w x h (even sizes). `background`: the last picture (NV12 bw x bh) to blur behind the
/// text, or `None` for a plain dark card. Same as [`render_card_w`] for a `&str`.
pub fn render_card(
    text: &str,
    w: u32,
    h: u32,
    background: Option<&[u8]>,
    bw: u32,
    bh: u32,
    nv12: &mut Vec<u8>,
) -> bool {
    let wide: Vec<u16> = text.encode_utf16().collect();
    render_card_w(&wide, w, h, background, bw, bh, nv12)
}

/// `still::RenderCard` with the text as UTF-16 (a `std::wstring`).
pub fn render_card_w(
    text: &[u16],
    w: u32,
    h: u32,
    background: Option<&[u8]>,
    bw: u32,
    bh: u32,
    nv12: &mut Vec<u8>,
) -> bool {
    if w < 2 || h < 2 {
        return false;
    }
    let Some(c) = Canvas::create(w, h) else {
        return false;
    };

    // The background: the last picture blurred (copied into the bitmap first), else a dark gradient.
    let mut has_background = background.is_some();
    if let Some(bg) = background {
        if bw >= 2 && bh >= 2 {
            if bg.len() < nv12_bytes(bw, bh) {
                return false;
            }
            let mut fitted = vec![0u8; nv12_bytes(w, h)];
            nv12_scale(bg, bw as usize, bw, bh, &mut fitted, w as usize, w, h);
            let mut blurred = Vec::new();
            blur_nv12(&fitted, w, h, 16.max(w / 40), &mut blurred);
            let all = WICRect {
                X: 0,
                Y: 0,
                Width: w as i32,
                Height: h as i32,
            };
            // SAFETY: the write lock gives exclusive access to the bitmap's pixels until it is
            // dropped (at the end of this block, before any drawing); rows are copied within
            // the `size` WIC reports.
            unsafe {
                match c.bitmap.Lock(&all, WICBitmapLockWrite.0 as u32) {
                    Ok(lock) => {
                        let stride = lock.GetStride().unwrap_or(0) as usize;
                        let mut size = 0u32;
                        let mut data: *mut u8 = std::ptr::null_mut();
                        if lock.GetDataPointer(&mut size, &mut data).is_ok() && !data.is_null() {
                            let mut bgra = vec![0u32; w as usize * h as usize];
                            let (by, buv) = blurred.split_at(w as usize * h as usize);
                            nv12_to_bgra(by, buv, w as usize, w, h, &mut bgra, w, h);
                            let rows =
                                (size as usize).saturating_sub(w as usize * 4) / stride.max(1) + 1;
                            for y in 0..(h as usize).min(rows) {
                                // Little-endian u32 B,G,R,A -> bytes B,G,R,A: a plain copy.
                                let row = &bgra[y * w as usize..(y + 1) * w as usize];
                                std::ptr::copy_nonoverlapping(
                                    row.as_ptr() as *const u8,
                                    data.add(y * stride),
                                    w as usize * 4,
                                );
                            }
                        }
                    }
                    Err(_) => has_background = false,
                }
            }
        }
    }

    let t = &c.target;
    let (fw, fh) = (w as f32, h as f32);
    // SAFETY: Direct2D drawing on a render target this function owns; all pointers passed are
    // to live locals and the target is single-threaded (only used here).
    unsafe {
        t.BeginDraw();
        if !has_background {
            let stops = [
                D2D1_GRADIENT_STOP {
                    position: 0.0,
                    color: color_rgb(0x23262E),
                },
                D2D1_GRADIENT_STOP {
                    position: 1.0,
                    color: color_rgb(0x111216),
                },
            ];
            let Ok(stops) =
                t.CreateGradientStopCollection(&stops, D2D1_GAMMA_2_2, D2D1_EXTEND_MODE_CLAMP)
            else {
                let _ = t.EndDraw(None, None);
                return false;
            };
            let props = D2D1_LINEAR_GRADIENT_BRUSH_PROPERTIES {
                startPoint: Vector2 { X: 0.0, Y: 0.0 },
                endPoint: Vector2 { X: fw, Y: fh },
            };
            let Ok(fill) = t.CreateLinearGradientBrush(&props, None, &stops) else {
                let _ = t.EndDraw(None, None);
                return false;
            };
            t.FillRectangle(&rect_f(0.0, 0.0, fw, fh), &fill);
        }

        // A pause sign (a ring with two bars) above the text, sized from the frame's shorter side.
        let unit = fw.min(fh) / 1080.0f32;
        let white = t.CreateSolidColorBrush(&color_rgba(1.0, 1.0, 1.0, 0.95), None);
        let soft = t.CreateSolidColorBrush(&color_rgba(1.0, 1.0, 1.0, 0.16), None);
        let (Ok(white), Ok(soft)) = (white, soft) else {
            let _ = t.EndDraw(None, None);
            return false;
        };
        let cx = fw / 2.0;
        let cy = fh / 2.0 - 70.0 * unit;
        let r = 64.0 * unit;
        t.FillEllipse(
            &D2D1_ELLIPSE {
                point: Vector2 { X: cx, Y: cy },
                radiusX: r,
                radiusY: r,
            },
            &soft,
        );
        let bar_w = 14.0 * unit;
        let bar_h = 50.0 * unit;
        let gap = 12.0 * unit;
        t.FillRoundedRectangle(
            &D2D1_ROUNDED_RECT {
                rect: rect_f(
                    cx - gap / 2.0 - bar_w,
                    cy - bar_h / 2.0,
                    cx - gap / 2.0,
                    cy + bar_h / 2.0,
                ),
                radiusX: 4.0 * unit,
                radiusY: 4.0 * unit,
            },
            &white,
        );
        t.FillRoundedRectangle(
            &D2D1_ROUNDED_RECT {
                rect: rect_f(
                    cx + gap / 2.0,
                    cy - bar_h / 2.0,
                    cx + gap / 2.0 + bar_w,
                    cy + bar_h / 2.0,
                ),
                radiusX: 4.0 * unit,
                radiusY: 4.0 * unit,
            },
            &white,
        );

        if let Ok(dw) = DWriteCreateFactory::<IDWriteFactory>(DWRITE_FACTORY_TYPE_SHARED) {
            // Segoe UI Variable on Windows 11; DirectWrite falls back to Segoe UI on Windows 10.
            let family = wide_z(
                &"Segoe UI Variable Display"
                    .encode_utf16()
                    .collect::<Vec<_>>(),
            );
            let locale = wide_z(&"vi-vn".encode_utf16().collect::<Vec<_>>());
            if let Ok(format) = dw.CreateTextFormat(
                PCWSTR(family.as_ptr()),
                None,
                DWRITE_FONT_WEIGHT_SEMI_BOLD,
                DWRITE_FONT_STYLE_NORMAL,
                DWRITE_FONT_STRETCH_NORMAL,
                64.0 * unit,
                PCWSTR(locale.as_ptr()),
            ) {
                let _ = format.SetTextAlignment(DWRITE_TEXT_ALIGNMENT_CENTER);
                let _ = format.SetParagraphAlignment(DWRITE_PARAGRAPH_ALIGNMENT_NEAR);
                let _ = format.SetWordWrapping(DWRITE_WORD_WRAPPING_WRAP);
                let top = cy + r + 36.0 * unit;
                t.DrawText(
                    text,
                    &format,
                    &rect_f(fw * 0.08, top, fw * 0.92, fh),
                    &white,
                    D2D1_DRAW_TEXT_OPTIONS_NONE,
                    DWRITE_MEASURING_MODE_NATURAL,
                );
            }
        }
        if t.EndDraw(None, None).is_err() {
            return false;
        }
    }
    c.to_nv12(w, h, nv12)
}

/// "Chụp ảnh": an NV12 frame (contiguous w x h) as a PNG file. Same as [`save_png_w`] for a `&str`.
pub fn save_png(path: &str, nv12: &[u8], w: u32, h: u32) -> HRESULT {
    let wide: Vec<u16> = path.encode_utf16().collect();
    save_png_w(&wide, nv12, w, h)
}

/// `still::SavePng` with the path as UTF-16. Returns the HRESULT (`S_OK` when written).
pub fn save_png_w(path: &[u16], nv12: &[u8], w: u32, h: u32) -> HRESULT {
    if nv12.is_empty() || w < 2 || h < 2 {
        return E_INVALIDARG;
    }
    if nv12.len() < nv12_bytes(w, h) {
        return E_INVALIDARG; // the C++ would read past the buffer
    }
    let mut bgra = vec![0u32; w as usize * h as usize];
    let (y, uv) = nv12.split_at(w as usize * h as usize);
    nv12_to_bgra(y, uv, w as usize, w, h, &mut bgra, w, h);
    let path_z = wide_z(path);
    let result = (|| -> windows::core::Result<()> {
        // SAFETY: WIC calls on interfaces created here; `bgra` outlives WritePixels and its
        // byte view is exactly w * h * 4 bytes.
        unsafe {
            let wic: IWICImagingFactory =
                CoCreateInstance(&CLSID_WICImagingFactory, None, CLSCTX_INPROC_SERVER)?;
            let stream = wic.CreateStream()?;
            stream.InitializeFromFilename(PCWSTR(path_z.as_ptr()), GENERIC_WRITE.0)?;
            let encoder = wic.CreateEncoder(&GUID_ContainerFormatPng, std::ptr::null())?;
            encoder.Initialize(&stream, WICBitmapEncoderNoCache)?;
            let mut frame = None;
            encoder.CreateNewFrame(&mut frame, std::ptr::null_mut())?;
            let frame = frame
                .ok_or_else(|| windows::core::Error::from(windows::Win32::Foundation::E_POINTER))?;
            frame.Initialize(None)?;
            frame.SetSize(w, h)?;
            let mut format: GUID = GUID_WICPixelFormat32bppBGRA; // alpha 255 everywhere (nv12_to_bgra)
            frame.SetPixelFormat(&mut format)?;
            if format != GUID_WICPixelFormat32bppBGRA {
                return Err(WINCODEC_ERR_UNSUPPORTEDPIXELFORMAT.into());
            }
            let bytes = std::slice::from_raw_parts(bgra.as_ptr() as *const u8, bgra.len() * 4);
            frame.WritePixels(h, w * 4, bytes)?;
            frame.Commit()?;
            encoder.Commit()?;
            // `stream` is released here, which closes the file (the C++ does stream.Reset()).
            Ok(())
        }
    })();
    match result {
        Ok(()) => S_OK,
        Err(e) => {
            // SAFETY: `path_z` is NUL-terminated and alive.
            let _ = unsafe { DeleteFileW(PCWSTR(path_z.as_ptr())) }; // no half-written file left behind
            e.code()
        }
    }
}

/// An image file (PNG, JPEG, BMP...) scaled to fill w x h, centred and cropped. False if
/// unreadable. Same as [`load_image_fill_w`] for a `&str`.
pub fn load_image_fill(path: &str, w: u32, h: u32, nv12: &mut Vec<u8>) -> bool {
    let wide: Vec<u16> = path.encode_utf16().collect();
    load_image_fill_w(&wide, w, h, nv12)
}

/// `still::LoadImageFill` with the path as UTF-16.
pub fn load_image_fill_w(path: &[u16], w: u32, h: u32, nv12: &mut Vec<u8>) -> bool {
    if w < 2 || h < 2 || path.is_empty() {
        return false;
    }
    let Some(c) = Canvas::create(w, h) else {
        return false;
    };
    let path_z = wide_z(path);
    // SAFETY: WIC and Direct2D calls on interfaces owned here; `path_z` is NUL-terminated and
    // alive for the decoder call.
    unsafe {
        let Ok(decoder) = c.wic.CreateDecoderFromFilename(
            PCWSTR(path_z.as_ptr()),
            None,
            GENERIC_READ,
            WICDecodeMetadataCacheOnDemand,
        ) else {
            return false;
        };
        let Ok(frame) = decoder.GetFrame(0) else {
            return false;
        };
        let Ok(converted) = c.wic.CreateFormatConverter() else {
            return false;
        };
        if converted
            .Initialize(
                &frame,
                &GUID_WICPixelFormat32bppPBGRA,
                WICBitmapDitherTypeNone,
                None,
                0.0,
                WICBitmapPaletteTypeCustom,
            )
            .is_err()
        {
            return false;
        }
        let Ok(image) = c.target.CreateBitmapFromWicBitmap(&converted, None) else {
            return false;
        };
        let size = image.GetSize();
        if size.width < 1.0 || size.height < 1.0 {
            return false;
        }
        // Fill: the image's shorter side meets the frame, the rest is cropped evenly.
        let scale = (w as f32 / size.width).max(h as f32 / size.height);
        let dw = size.width * scale;
        let dh = size.height * scale;
        let x = (w as f32 - dw) / 2.0;
        let y = (h as f32 - dh) / 2.0;
        c.target.BeginDraw();
        c.target.Clear(Some(&color_rgb(0x000000)));
        c.target.DrawBitmap(
            &image,
            Some(&rect_f(x, y, x + dw, y + dh)),
            1.0,
            D2D1_BITMAP_INTERPOLATION_MODE_LINEAR,
            None,
        );
        if c.target.EndDraw(None, None).is_err() {
            return false;
        }
    }
    c.to_nv12(w, h, nv12)
}
