# Models used by the picture effects

| Model | File | Used for | Licence | Source | In git |
|---|---|---|---|---|---|
| PP-HumanSeg 2023mar | `app/Assets/Models/pphumanseg.onnx` (6 MB) | "Nền" (every engine) | Apache-2.0 | OpenCV Zoo | yes, with `pphumanseg-LICENSE.txt` |
| MODNet (webcam) | `modnet_webcam.onnx` (26 MB) | "Chất lượng cao": soft matte for the background | Apache-2.0, `docs/third-party/modnet-LICENSE.txt` | github.com/yakhyo/modnet (ONNX of ZHKKKe/MODNet, AAAI 2022) | no: `scripts/fetch-models.ps1` |
| YuNet 2026may | `face_detection_yunet_2026may.onnx` (230 KB) | "Chất lượng cao": faces for "Tự căn khung" and "Làm mịn da" | MIT, `docs/third-party/yunet-LICENSE.txt` | opencv/opencv_zoo, `models/face_detection_yunet` | no: `scripts/fetch-models.ps1` |

`scripts/fetch-models.ps1` downloads the last two into `app/Assets/Models` and checks their SHA-256
(MODNet `de03cc16…5306`, YuNet `ebafce4e…f0f0`). They stay out of git so 26 MB of binary does not stay in the
history for good, and `package-release.ps1` does not call it: the models only matter once the Rust engine
ships (the C++ engine ignores them; the app disables the switch with it). When that release is made:

1. run `scripts\fetch-models.ps1`;
2. the installer picks them up by itself (`Assets\**` is copied to the output and the whole folder goes into `{app}\App`);
3. show the two licence notices (Settings › About, next to the VB-CABLE attribution): the licence texts are in `docs/third-party/`.

Without the files the switch still works, with PP-HumanSeg and the guided filter (and Windows' own face tracker).
Licences read on the projects' own pages on 2026-11-22; check them again before a release.
