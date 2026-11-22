//! niwcam-shm: the shared-memory frame channel between the host app (writer) and the virtual
//! camera media source / DirectShow filter (reader).
//!
//! Ports `src/common/SharedFrame.h`, `src/engine/FrameWriter.h` and `src/vcam/FrameReader.{h,cpp}`.
//! The header layout, constants, kernel-object names and the SDDL are byte-for-byte the C++ ones
//! (`xcheck` proves both directions against the C++ code). Every kernel-object name can be given
//! as a parameter (`Names`) so tests use unique ones; the default is the real names.

mod frame_reader;
mod frame_writer;
mod os;
mod shared_frame;

pub use frame_reader::{draw_no_signal, FrameReader};
pub use frame_writer::FrameWriter;
pub use os::{mf_system_time, set_log_hook, ChannelNames, Names};
pub use shared_frame::*;
