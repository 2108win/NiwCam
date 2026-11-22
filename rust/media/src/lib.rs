//! niwcam-media: the Media Foundation / WASAPI / WIC parts of the C++ engine.
//!
//! | module | C++ |
//! |---|---|
//! | [`video_decoder`] | `src/engine/VideoDecoder.{h,cpp}` |
//! | [`mf_devices`] | `src/common/MfDevices.h` (+ `BuildFormatOptions` of `CaptureEngine.cpp`) |
//! | [`audio_out`] | `src/engine/AudioOut.{h,cpp}` |
//! | [`recorder`] | `src/engine/Recorder.{h,cpp}` |
//! | [`snapshot`] | `still::SavePng` of `src/engine/StillPicture.cpp` (the WIC PNG writer) |
//!
//! Every fallible call returns the raw `HRESULT` the C++ returned (`S_FALSE` and friends keep
//! their meaning). COM and Media Foundation must be started by the caller exactly as for the C++
//! (`CoInitializeEx`, `MFStartup`).
#![deny(unsafe_op_in_unsafe_fn)]

pub mod audio_out;
pub mod mf_devices;
mod mf_util;
pub mod recorder;
pub mod snapshot;
pub mod video_decoder;

pub use windows::core::HRESULT;
