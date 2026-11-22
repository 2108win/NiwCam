//! Everything the engine uses from `niwcam-fx` (StillPicture, FaceDetect, Segmenter,
//! WindowSource), in one place. No stubs: the effects crate is finished.

pub(crate) use niwcam_fx::background_hq::{blend_background_hq, HqScratch};
pub(crate) use niwcam_fx::face_detect::FaceDetector;
pub(crate) use niwcam_fx::matte_segmenter::MatteSegmenter;
pub(crate) use niwcam_fx::segmenter::{blend_background, BackgroundScratch, Segmenter, K_SIZE};
pub(crate) use niwcam_fx::still_picture::{load_image_fill, render_card};
pub(crate) use niwcam_fx::window_source::{CaptureResult, WindowCapture};
