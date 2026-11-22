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

## ADR-010: The optional models are fetched, not committed
Status: accepted (2026-11-22), chosen by the assistant when the user said "choose for me"
Context: MODNet (26 MB) and YuNet (230 KB) are needed by the high-quality effects of the Rust engine only.
Decision: not in git (the binary would stay in the history), not in the C++ release; `scripts/fetch-models.ps1` downloads them from the official
sources and checks SHA-256; licence texts and sources are in `docs/third-party-models.md` and `docs/third-party/`; the release that ships the Rust
engine runs the script first (the installer already copies everything under `Assets\`) and shows the two notices.
Consequences: a clone has no models until the script runs; without them the switch uses PP-HumanSeg + guided filter and Windows' face tracker.

## ADR-011: Phone video is decoded on the GPU (2026-11-22)
Status: accepted and built in BOTH engines (Rust `media/src/video_decoder.rs`, C++ `src/engine/VideoDecoder.cpp`)
Context: at 1080p30 a phone stream was fine, at 4K / 60 fps it was not. Measured on this PC (RTX 5060 + AMD iGPU), 4K, synthetic 7 Mbps stream: the
Microsoft software decoder takes 12.7 ms (H.264) / 11.2 ms (HEVC) per frame on ONE thread (`CODECAPI_AVDecNumWorkerThreads` gives E_INVALIDARG; a reused
output buffer changed nothing), so 4K60 (16.7 ms for decode + pipeline + copy) cannot keep up, and real streams (25+ Mbps) are slower than the test one.
Through the engine, 4K60 H.264 replayed by the fake phone: software = 28 fps out, frames skipped, 19 s of CPU per 10 s; GPU = 61 fps, 0 skipped, 4-5 s.
Decision: the same inbox *synchronous* decoder MFT, given a Direct3D 11 device (`MFT_MESSAGE_SET_D3D_MANAGER`, `D3D11_CREATE_DEVICE_VIDEO_SUPPORT`,
multithread-protected) so it uses DXVA; each picture is copied to a CPU-readable staging texture and handed on like before (pitch = `RowPitch`, repacked
when the texture is taller than the picture). `MFT_ENUM_FLAG_HARDWARE` MFTs stay unused (async). Falls back to software at init (no D3D11-aware MFT, device
creation fails, no NV12 output type) and at run time (readback fails, stream change offers no NV12: e.g. 10-bit HEVC as P010 only; a decode error is returned
once so the engine asks for a keyframe). `NIWCAM_DECODER=software` forces the old path.
Verified: `decode_bench` hashes every frame of H.264 and HEVC streams (640x480 to 4K, size changes, 1088-row surfaces) hardware vs software: identical
pixels. One difference: the GPU decoder has no one-frame delay, so an HEVC stream's last frame comes out too (software holds it back). 2.7 ms (H.264) / 2.8 ms
(HEVC) per 4K frame including the readback. Real Redmi 1080p30 over USB: CPU 37 % -> 30 % of a core, but 10.5 ms against 8.0 ms per frame (the GPU is idle
and slow to wake; irrelevant at 30 fps, wanted at 4K60). `xcheck` decode stage runs with `NIWCAM_DECODER=software` so C++ and Rust are still compared like with like.
Also: the camera is published again at a faster rate when a faster source arrives on the same size while no app holds it (4K60 after 4K30 would stay at 30).
Not tested: a real iPhone (4K60, HLG 10-bit) and the HEVC 10-bit fallback; Wi-Fi bandwidth at 4K60 (the bitrate heuristic caps at 60 Mbps).

## ADR-012: Heavy iPhone modes use HEVC and the normal encoder (2026-11-22)
Status: accepted. iOS built and measured on an iPhone 14; PC (both engines) compiled and measured on the PC with that iPhone over USB (below)
Context: the iPhone sent ~40 fps at 4K60/50/48. Measured on the phone (`StreamDiag`, docs/ios-4k60.md): the camera delivers 60, but VideoToolbox's
low-latency rate control encodes one frame at a time (the encode call returns when the frame is done): ~26 ms per 4K frame, ~9.5 ms per 1080p frame, so
4K60 HEVC gave 37.6 fps (22.4 frames/s dropped as `FrameWasLate`) and 1080p120 108. The normal encoder pipelines ~2 frames: HEVC 4K60 = 60 fps at 35-39 ms,
1080p120 = 120 at 21 ms, 1080p240 = 238 at 12 ms; but 4K30 waits 77 ms (low latency: 26), and H.264 4K60 reaches only 50 fps with its queue at ~139 ms.
Decision: low-latency rate control unless the codec is HEVC and the mode has fps > 60 or more than 3840x2160x30 pixels per second
(`VideoEncoder.lowLatencyFits`; `tune.lowLatency` overrides it for tests). H.264 keeps low latency everywhere, so every existing PC (which always asks
H.264) sees no change. The PC asks HEVC for exactly those iPhone modes when Windows has an HEVC decoder (`iphone_needs_hevc` / `IphoneNeedsHevc`); the
two rules must stay equal. The saved mode (`HKCU\...\Modes`) holds lens, size and fps only, so it still matches.
Not tested: the PC change (compile, then 4K60 from the real iPhone through the virtual camera); other iPhone models (thresholds from an A15);
HEVC 10-bit with the normal encoder; Wi-Fi at 4K to the real PC (from the Mac the link carried only ~15-18 Mbit/s, docs/ios-4k60.md).
Result on the PC (2026-11-22, iPhone 14 over USB, `phone_perf --usb --format 3840x2160@60`): the mode list offers 4K60/50/48 as HEVC and 4K30 as H.264 (both engines);
Rust engine 5 min: 59.9 fps average, lag peak 33 ms, 7 frames skipped in 300 s, ~77 % of a core; C++ engine 15 s: 60 fps, ~100 % of a core, 52 skipped.
Not seen: the app's hot-phone rule in a long run (phone_perf does not read `thermal`; the frame rate never fell).
