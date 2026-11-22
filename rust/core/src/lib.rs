//! NiwCam core: the parts of `src/engine` that need no operating system.
//! Ported module by module; `rust/MAP.md` says what is done.
#![forbid(unsafe_code)]

pub mod auto_frame;
pub mod crypto;
pub mod image_settings;
pub mod json;
pub mod nv12;
pub mod skin_smooth;
mod util;
