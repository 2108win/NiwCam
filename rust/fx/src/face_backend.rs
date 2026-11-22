//! One face detector for the engine, two engines behind it (ADR-008): YuNet when its model file is
//! given and works, Windows' FaceTracker otherwise (and as the way out when the model fails to load
//! or keeps failing). Same calls as either: `due`, `offer`, `take_result`, `state`.

use niwcam_core::auto_frame::FaceRect;

use crate::face_detect::FaceDetector;
use crate::yunet_detector::YuNetDetector;

pub struct FaceBackend {
    yunet: Option<YuNetDetector>,
    windows: FaceDetector,
}

impl FaceBackend {
    /// `yunet_model`: the path of `face_detection_yunet_*.onnx` to use YuNet, `None` for Windows' own.
    pub fn new(yunet_model: Option<&str>) -> FaceBackend {
        let yunet = yunet_model.map(|path| {
            let d = YuNetDetector::new();
            d.set_model(path);
            d
        });
        FaceBackend {
            yunet,
            windows: FaceDetector::new(),
        }
    }

    /// YuNet while it works (state 1); the Windows tracker once it is known not to.
    fn yunet(&self) -> Option<&YuNetDetector> {
        self.yunet.as_ref().filter(|y| y.state() == 1)
    }

    /// Which engine answers now: "yunet" or "windows".
    pub fn engine(&self) -> &'static str {
        if self.yunet().is_some() {
            "yunet"
        } else {
            "windows"
        }
    }

    pub fn due(&self, now_ms: f64) -> bool {
        match self.yunet() {
            Some(y) => y.due(now_ms),
            None => self.windows.due(now_ms),
        }
    }

    pub fn offer(
        &self,
        luma: &[u8],
        pitch: usize,
        width: u32,
        height: u32,
        generation: u32,
        now_ms: f64,
    ) {
        match self.yunet() {
            Some(y) => y.offer(luma, pitch, width, height, generation, now_ms),
            None => self
                .windows
                .offer(luma, pitch, width, height, generation, now_ms),
        }
    }

    pub fn take_result(&self, generation: u32, faces: &mut Vec<FaceRect>) -> bool {
        match self.yunet() {
            Some(y) => y.take_result(generation, faces),
            None => self.windows.take_result(generation, faces),
        }
    }

    /// 1 works or not tried yet; 0 nothing here can detect faces; -1 it kept failing.
    pub fn state(&self) -> i32 {
        match self.yunet() {
            Some(y) => y.state(),
            None => self.windows.state(),
        }
    }
}
