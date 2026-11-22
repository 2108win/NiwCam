# Decisions

## ADR-001: Port the PC core only
Status: accepted (2026-11-22)
Decision: engine, vcam and dshow go to Rust. The WinUI 3 app (C#) stays; phone apps stay Kotlin/Swift.
Consequences: the Rust engine must keep the C API of `EngineApi.h` so the app needs no change.

## ADR-002: Hand port, C++ checks as the bar
Status: accepted (2026-11-22)
Context: rust-anything bridges TS, Python, C, Go, Ruby, Java only; the engine is C++ and the app C#.
Decision: port module by module by hand. A module is done when the matching C++ check passes on the
Rust code (vectors, `frame-check`, `framing-check`) and clippy `-D warnings` is clean.
Consequences: the C++ stays as reference until the last wave.

## ADR-003: No `unsafe` outside the OS crates (one exception: ADR-007)
Status: accepted (2026-11-22)
Decision: `niwcam-core` is `deny(unsafe_code)` with one allowed module, `simd.rs`. Win32/COM lives in the OS crates, each `unsafe` with `// SAFETY:`.

## ADR-004: Crypto on RustCrypto crates, HKDF written out
Status: accepted (2026-11-22)
Decision: `sha2`, `hmac`, `p256`, `aes-gcm`; HKDF hand-written (RFC 5869), as on every other platform.
Consequences: it no longer depends on Windows CNG, so the same code runs on the Mac for `fake-pc`-style tools.

## ADR-005: Commit on `main`, no branches
Status: accepted (2026-11-22) — the repo rule in CLAUDE.md applies to the Rust work too.

## ADR-006: JSON differences accepted
Status: accepted (2026-11-22)
Context: `Json.h` keeps strings as raw bytes; a Rust `String` is UTF-8.
Decision: input bytes that are not UTF-8, and a lone `\uD800`-`\uDFFF` escape, become U+FFFD (the C++ kept or
wrote invalid bytes); a malformed `\u` escape or a number out of range is an error with another message.
Everything valid reads and writes the same (`xcheck`).
Consequences: revisit if a phone ever sends invalid UTF-8.

## ADR-007: SSE2 in `core/src/simd.rs`
Status: accepted (2026-11-22), replaces the first choice (plain loops)
Context: plain loops were 3.3x (NV12 -> BGRA) and 16x (colour adjust) slower than the C++ SSE2; the engine used ~45 % more CPU.
Decision: port the two SSE2 loops with `core::arch` (SSE2 is part of x86-64, no run-time detection); other targets use the plain
loops. Same bytes (`xcheck/run.ps1`, tests in `simd.rs`); `xcheck/cpp_bench.cpp` vs `core/examples/bench.rs` time them.
Consequences: the engine's CPU with a consumer reading is equal to the C++ (3.66 s vs 3.77 s over 22 s, 3 runs each).

## ADR-008: Better background, face and object detection come after parity
Status: accepted (2026-11-22), asked for by the user
Context: the user wants the background blur/replace to be the best and smoothest, and better face/object
detection. The port is held to byte/behaviour equality with the C++ (xcheck, driver), which an upgrade breaks.
Decision: first finish the faithful port of `Segmenter`, `FaceDetect`, `SkinSmooth`, `AutoFrame` (crate `fx`). Then add
the upgrade as separate modules behind a setting, default off until it wins: soft-alpha matting model with temporal
smoothing, edge refinement against the full-resolution frame, a better blur, a stronger face detector with tracking
(and a person/pose detector for framing). Models only with permissive licences (no GPL/AGPL/non-commercial).
Chosen from `rust/research/background-and-faces.md`; judged by quality metrics (matting error, flicker between frames),
frame time on this PC and idle cost, not by equality with the C++.
Consequences: the old path stays as the fallback and the reference.

## ADR-009: Keep the C++ `FrameReader::Open` quirk
Status: accepted (2026-11-22)
Context: the virtual camera's media source is also instantiated inside the app's own process (session 1, a normal user),
where creating the `Global\` mapping fails with ACCESS_DENIED. The C++ then reads `GetLastError()` after the event call
(0) and returns S_OK with no mapping, so `IMFVirtualCamera::Start` succeeds; the instance in Frame Server (privileged)
creates the real channel. The Rust port first returned the mapping's own error: `Start` failed with 0x80070005.
Decision: copy the quirk (`shm/src/frame_reader.rs`, comment there). Found only by loading the DLL into the real Frame Server.
Consequences: any "fix" of this must be tested with the real Frame Server, not in-process.

### ADR-008 progress (2026-11-22): step 1 done, no new weights
`fx/src/background_hq.rs` + `Segmenter::set_quality` + `cs_set_background_quality` (Rust engine only) + the app's "Chất lượng cao" switch
(Hiệu ứng › Nền, disabled with the C++ engine). Off by default. What it does: soft probability kept (no early hardening), uncertainty-weighted
smoothing over time, guided filter of the alpha against the picture's luma at 1/4 size, a blur made only from background pixels (no halo of the
person's colours), a light wrap for replacement, the model run 30x a second instead of 15.
Measured: `fx/examples/matte_bench.rs` (synthetic scene with a true alpha, simulated model output) vs the classic chain: flicker 0.91 -> 0.80
(still), edge error 112 -> 104 (still) and 137 -> 106 (moving), mean error 7.5 -> 6.9. The model's own error dominates the rest, so the next
real gain needs a better model. `fx/examples/bg_compare.rs` on a real portrait with the real model: the long hair on the left that the
classic chain blurs away is kept, and the blurred wall has no coloured halo. Cost: +1.2 ms per 720p frame (refine) = ~4 % of a core at 30 fps;
off = 0. `fx/tests/hq.rs` covers smoothing, edge snapping and the halo. Engine with the model, 30 fps held, 238 MB peak (the model's DirectML).
Next (needs downloads, asked first): a matting model (MODNet, Apache-2.0) and a face detector (YuNet, MIT); see `rust/research/background-and-faces.md`.

### ADR-008 progress, step 2 (2026-11-22): the matting model
`fx/src/matte_segmenter.rs`: MODNet `modnet_webcam.onnx` (Apache-2.0, 24.8 MB, from github.com/yakhyo/modnet) through Windows ML, dynamic input
(384x224 for 16:9, 224x384 upright, multiples of 32), soft alpha, 20 runs a second. The engine uses it when the high-quality switch is on and the
file is next to `pphumanseg.onnx` (`Assets\Models\modnet_webcam.onnx`); otherwise the PP-HumanSeg + guided filter chain of step 1.
Measured on this PC (RTX 5060, DirectML): 7.1 ms per run in the engine (9.2 ms at 512x288, 6.0 ms at 320x192; CPU 25-63 ms); the engine holds 30 fps;
working set 392 MB with it against 238 MB with PP-HumanSeg (the larger model in DirectML). On the real portrait the whole long grey hair stays on both sides
with a soft edge (PP-HumanSeg loses the left side). Not shipped: the model file is not in the installer or the repo (a 26 MB addition and its
Apache-2.0 notice are the user's call); a dev copy goes in `app\Assets\Models`.
Still open: YuNet face detector + Kalman tracking for framing and skin smoothing; the matte flicker on a real moving person (needs a clip).

### ADR-008 progress, step 3 (2026-11-22): faces
`fx/src/yunet_detector.rs` (YuNet `face_detection_yunet_2026may.onnx`, MIT, 230 KB, from opencv_zoo) + `fx/src/face_backend.rs`: in high quality the
engine's framing and skin smoothing use YuNet when the file is next to `pphumanseg.onnx`, else (or when it fails) Windows' FaceTracker. Runs through Windows
ML on the GPU in 3.8 ms at 640x352 (looked at in grey, 10 times a second). Decoded as OpenCV's FaceDetectorYN does (strides 8/16/32, score sqrt(cls*obj),
NMS 0.3). `fx/examples/face_compare.rs` on the one portrait Windows ships, at five shapes: YuNet's box agrees with the Windows tracker's within 1-2 %
of the picture, no false face on a flat picture. First answer takes 0.6-1.5 s (the model loads on its own thread), the tracker 30-75 ms.
NOT shown: any gain on several faces, small or turned faces (no such pictures here; YuNet's WIDER Face scores are the zoo's, not ours), and the Kalman
tracking of the research (the framing code already eases and holds the shot, so it was left out until a real clip shows jitter).
Engine with MODNet + YuNet + framing on: 29 fps held, 395-434 MB working set; nothing is loaded while the switch or the effects are off.
