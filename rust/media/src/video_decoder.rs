//! H.264 / HEVC (Annex-B) -> NV12 with the Media Foundation decoder MFT.
//! Port of `src/engine/VideoDecoder.{h,cpp}`: same enumeration flags, same attributes, same output
//! type negotiation, same HRESULTs. Media Foundation must be started by the caller (`MFStartup`).
use std::mem::ManuallyDrop;

use windows::core::{Interface, GUID, HRESULT};
use windows::Win32::Media::MediaFoundation::*;
use windows::Win32::System::Com::CoTaskMemFree;
use windows::Win32::System::Variant::VARIANT;

use crate::mf_util::{self, E_UNEXPECTED, S_OK};

/// `nv12` is the visible picture: `height` luma rows of `pitch` bytes, then `height / 2`
/// interleaved UV rows, contiguous (`pitch * height * 3 / 2` bytes at least). Valid during the call.
/// Arguments: `(nv12, pitch, width, height)`.
pub type FrameCallback<'a> = dyn FnMut(&[u8], i32, u32, u32) + 'a;

#[derive(Default)]
pub struct VideoDecoder {
    mft: Option<IMFTransform>,
    provides_samples: bool,
    output_size: u32,
    coded_width: u32,
    coded_height: u32, // decoder surface (e.g. 1920x1088)
    width: u32,
    height: u32, // visible picture (e.g. 1920x1080)
    stride: i32,
    staging: Vec<u8>,
}

// SAFETY: a Media Foundation transform is free-threaded; the decoder is used from one thread at a
// time (the capture thread), but may be created on another.
unsafe impl Send for VideoDecoder {}

fn is_failed(hr: HRESULT) -> bool {
    hr.is_err()
}

impl VideoDecoder {
    pub fn new() -> Self {
        Self::default()
    }

    /// Visible picture size as negotiated (valid after `init`, updated on a stream change).
    pub fn size(&self) -> (u32, u32) {
        (self.width, self.height)
    }

    pub fn init(&mut self, hevc: bool, width: u32, height: u32) -> HRESULT {
        self.mft = None;
        let subtype = if hevc {
            MFVideoFormat_HEVC
        } else {
            MFVideoFormat_H264
        };
        let input = MFT_REGISTER_TYPE_INFO {
            guidMajorType: MFMediaType_Video,
            guidSubtype: subtype,
        };
        let output = MFT_REGISTER_TYPE_INFO {
            guidMajorType: MFMediaType_Video,
            guidSubtype: MFVideoFormat_NV12,
        };

        let mut activates: *mut Option<IMFActivate> = std::ptr::null_mut();
        let mut count: u32 = 0;
        // Synchronous decoders only; hardware MFTs are async and need an event loop.
        // SAFETY: out pointers are valid locals; MFTEnumEx allocates the array with CoTaskMemAlloc.
        let r = unsafe {
            MFTEnumEx(
                MFT_CATEGORY_VIDEO_DECODER,
                MFT_ENUM_FLAG_SYNCMFT | MFT_ENUM_FLAG_LOCALMFT | MFT_ENUM_FLAG_SORTANDFILTER,
                Some(&input),
                Some(&output),
                &mut activates,
                &mut count,
            )
        };
        if let Err(e) = r {
            return e.code();
        }
        if count == 0 {
            // SAFETY: null or the array MFTEnumEx allocated.
            unsafe { CoTaskMemFree(Some(activates as *const _)) };
            return MF_E_TOPO_CODEC_NOT_FOUND; // e.g. HEVC Video Extensions not installed
        }
        // SAFETY: `activates` holds `count` interface pointers we own: the first is used, every
        // one is released (read() moves each out so it drops), then the array itself is freed.
        let mft = unsafe {
            let first = (*activates).clone();
            let activated = match &first {
                Some(a) => a.ActivateObject::<IMFTransform>(),
                None => Err(windows::core::Error::from(E_UNEXPECTED)),
            };
            for i in 0..count as usize {
                drop(activates.add(i).read());
            }
            CoTaskMemFree(Some(activates as *const _));
            activated
        };
        let mft = match mft {
            Ok(m) => m,
            Err(e) => return e.code(),
        };
        // Like the C++ (`IID_PPV_ARGS(&m_mft)`), the MFT stays set when a later step fails.
        self.mft = Some(mft.clone());

        // SAFETY: all calls below are plain COM calls on interfaces we hold.
        unsafe {
            if let Ok(attrs) = mft.GetAttributes() {
                let _ = attrs.SetUINT32(&MF_LOW_LATENCY, 1);
            }
            if let Ok(codec_api) = mft.cast::<ICodecAPI>() {
                let v = VARIANT::from(true);
                let _ = codec_api.SetValue(&CODECAPI_AVLowLatencyMode, &v);
            }
        }

        // SAFETY: as above.
        let r = unsafe {
            let ty = match MFCreateMediaType() {
                Ok(t) => t,
                Err(e) => return e.code(),
            };
            let _ = ty.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video);
            let _ = ty.SetGUID(&MF_MT_SUBTYPE, &subtype);
            let _ = ty.SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32);
            mf_util::set_attribute_size(&ty, &MF_MT_FRAME_SIZE, width, height);
            mft.SetInputType(0, &ty, 0)
        };
        if let Err(e) = r {
            return e.code();
        }

        self.width = width;
        self.height = height;
        let hr = self.select_output_type();
        if is_failed(hr) {
            return hr;
        }
        if let Some(mft) = &self.mft {
            // SAFETY: plain COM calls.
            unsafe {
                let _ = mft.ProcessMessage(MFT_MESSAGE_NOTIFY_BEGIN_STREAMING, 0);
                let _ = mft.ProcessMessage(MFT_MESSAGE_NOTIFY_START_OF_STREAM, 0);
            }
        }
        S_OK
    }

    pub fn reset(&mut self) {
        self.mft = None;
    }

    fn select_output_type(&mut self) -> HRESULT {
        let Some(mft) = self.mft.clone() else {
            return E_UNEXPECTED;
        };
        let mut i = 0u32;
        loop {
            // SAFETY: plain COM calls on the MFT and the types it hands out.
            let ty = match unsafe { mft.GetOutputAvailableType(0, i) } {
                Ok(t) => t,
                Err(e) => return e.code(), // MF_E_NO_MORE_TYPES: no NV12 offered
            };
            i += 1;
            let sub = unsafe { ty.GetGUID(&MF_MT_SUBTYPE) }.unwrap_or(GUID::zeroed());
            if sub != MFVideoFormat_NV12 {
                continue;
            }
            if let Err(e) = unsafe { mft.SetOutputType(0, &ty, 0) } {
                return e.code();
            }

            // The surface may be taller than the picture (1088 for 1080p):
            // the visible area is the minimum display aperture.
            mf_util::get_attribute_size(
                &ty,
                &MF_MT_FRAME_SIZE,
                &mut self.coded_width,
                &mut self.coded_height,
            );
            self.stride = match unsafe { ty.GetUINT32(&MF_MT_DEFAULT_STRIDE) } {
                Ok(s) => s as i32,
                Err(_) => self.coded_width as i32,
            };
            let mut area = MFVideoArea::default();
            // SAFETY: the blob is read into a plain-old-data struct of exactly its size.
            let got_area = unsafe {
                let bytes = std::slice::from_raw_parts_mut(
                    &mut area as *mut MFVideoArea as *mut u8,
                    std::mem::size_of::<MFVideoArea>(),
                );
                ty.GetBlob(&MF_MT_MINIMUM_DISPLAY_APERTURE, bytes, None)
            }
            .is_ok();
            if got_area && area.Area.cx > 0 && area.Area.cy > 0 {
                self.width = area.Area.cx as u32;
                self.height = area.Area.cy as u32;
            } else {
                self.width = self.coded_width;
                self.height = self.coded_height;
            }

            let mut info = MFT_OUTPUT_STREAM_INFO::default();
            // SAFETY: `info` is a valid out parameter.
            if let Ok(got) = unsafe { mft.GetOutputStreamInfo(0) } {
                info = got;
            }
            self.provides_samples = (info.dwFlags
                & (MFT_OUTPUT_STREAM_PROVIDES_SAMPLES.0 | MFT_OUTPUT_STREAM_CAN_PROVIDE_SAMPLES.0)
                    as u32)
                != 0;
            self.output_size = info.cbSize;
            return S_OK;
        }
    }

    /// Feeds one access unit (Annex-B). Calls `on_frame` for every decoded frame. A failure means
    /// the stream needs a new keyframe.
    pub fn decode(
        &mut self,
        data: &[u8],
        time_100ns: i64,
        on_frame: &mut FrameCallback<'_>,
    ) -> HRESULT {
        let Some(mft) = self.mft.clone() else {
            return E_UNEXPECTED;
        };

        // SAFETY: the buffer is locked for the copy and unlocked again; `data.len()` bytes fit
        // because the buffer was created with that size.
        let sample = unsafe {
            let buffer = match MFCreateMemoryBuffer(data.len() as u32) {
                Ok(b) => b,
                Err(e) => return e.code(),
            };
            let mut dst: *mut u8 = std::ptr::null_mut();
            if let Err(e) = buffer.Lock(&mut dst, None, None) {
                return e.code();
            }
            std::ptr::copy_nonoverlapping(data.as_ptr(), dst, data.len());
            let _ = buffer.Unlock();
            let _ = buffer.SetCurrentLength(data.len() as u32);
            let sample = match MFCreateSample() {
                Ok(s) => s,
                Err(e) => return e.code(),
            };
            let _ = sample.AddBuffer(&buffer);
            let _ = sample.SetSampleTime(time_100ns);
            sample
        };

        // SAFETY: plain COM call.
        let mut r = unsafe { mft.ProcessInput(0, &sample, 0) };
        if matches!(&r, Err(e) if e.code() == MF_E_NOTACCEPTING) {
            // Output is pending: drain it, then retry once.
            let _ = self.drain(on_frame);
            r = unsafe { mft.ProcessInput(0, &sample, 0) };
        }
        if let Err(e) = r {
            return e.code();
        }
        self.drain(on_frame)
    }

    fn drain(&mut self, on_frame: &mut FrameCallback<'_>) -> HRESULT {
        let Some(mft) = self.mft.clone() else {
            return E_UNEXPECTED;
        };
        loop {
            let mut own: Option<IMFSample> = None;
            let mut out = MFT_OUTPUT_DATA_BUFFER::default();
            if !self.provides_samples {
                // SAFETY: plain COM calls creating the buffer and sample we hand to the MFT.
                unsafe {
                    let size = if self.output_size != 0 {
                        self.output_size
                    } else {
                        self.coded_width
                            .wrapping_mul(self.coded_height)
                            .wrapping_mul(3)
                            / 2
                    };
                    if let Ok(b) = MFCreateMemoryBuffer(size) {
                        if let Ok(s) = MFCreateSample() {
                            let _ = s.AddBuffer(&b);
                            own = Some(s);
                        }
                    }
                }
                out.pSample = ManuallyDrop::new(own.clone());
            }
            let mut status = 0u32;
            // SAFETY: `out` is one valid MFT_OUTPUT_DATA_BUFFER; the references it carries are
            // taken back below so each is released exactly once.
            let r = unsafe { mft.ProcessOutput(0, std::slice::from_mut(&mut out), &mut status) };
            let returned = ManuallyDrop::into_inner(std::mem::take(&mut out.pSample));
            drop(ManuallyDrop::into_inner(std::mem::take(&mut out.pEvents)));
            let produced = if self.provides_samples {
                returned
            } else {
                None
            };

            match r {
                Err(e) if e.code() == MF_E_TRANSFORM_NEED_MORE_INPUT => return S_OK,
                Err(e) if e.code() == MF_E_TRANSFORM_STREAM_CHANGE => {
                    // First frame (or resolution change): the real output format is known now.
                    let hr = self.select_output_type();
                    if is_failed(hr) {
                        return hr;
                    }
                    continue;
                }
                Err(e) => return e.code(),
                Ok(()) => {}
            }

            let result = if self.provides_samples { produced } else { own };
            // SAFETY: COM calls on the sample; the locked bytes are only read while locked.
            unsafe {
                let Some(result) = result else { continue };
                let Ok(buf) = result.ConvertToContiguousBuffer() else {
                    continue;
                };
                let mut p: *mut u8 = std::ptr::null_mut();
                let mut len = 0u32;
                if buf.Lock(&mut p, None, Some(&mut len)).is_err() {
                    continue;
                }

                let stride = if self.stride > 0 {
                    self.stride
                } else {
                    self.coded_width as i32
                };
                let luma_bytes = stride as usize * self.coded_height as usize;
                if len as usize >= luma_bytes + luma_bytes / 2 {
                    let whole = std::slice::from_raw_parts(p, len as usize);
                    if self.height == self.coded_height {
                        on_frame(
                            &whole[..luma_bytes + luma_bytes / 2],
                            stride,
                            self.width,
                            self.height,
                        );
                    } else {
                        // UV starts after the full coded height: repack so it follows
                        // the visible rows, as the rest of the pipeline expects.
                        let visible_luma = stride as usize * self.height as usize;
                        self.staging.resize(visible_luma + visible_luma / 2, 0);
                        self.staging[..visible_luma].copy_from_slice(&whole[..visible_luma]);
                        self.staging[visible_luma..]
                            .copy_from_slice(&whole[luma_bytes..luma_bytes + visible_luma / 2]);
                        on_frame(&self.staging, stride, self.width, self.height);
                    }
                }
                let _ = buf.Unlock();
            }
        }
    }
}
