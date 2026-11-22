//! niwcam-engine: the flat C API (NiwCamEngine.dll) and the capture engine.
#![deny(unsafe_op_in_unsafe_fn)]
#![allow(non_snake_case, reason = "the DLL file must be NiwCamEngine.dll, so the crate carries that name")]

pub mod abi;
pub mod api;
pub mod capture_engine;
mod fx_bridge;
pub mod pipeline;
pub mod screen_viewer;

pub use capture_engine::{
    BackgroundMode, BackgroundState, CaptureEngine, Framing, PauseKind, PhoneAudioState, State,
    Status,
};
