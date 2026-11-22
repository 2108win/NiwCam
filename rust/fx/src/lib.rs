//! niwcam-fx: StillPicture, FaceDetect, Segmenter, WindowSource (see rust/MAP.md).
#![deny(unsafe_op_in_unsafe_fn)]

pub mod background_hq;
pub mod face_backend;
pub mod face_detect;
pub mod matte_segmenter;
pub mod segmenter;
pub mod still_picture;
pub mod window_source;
pub mod yunet_detector;
