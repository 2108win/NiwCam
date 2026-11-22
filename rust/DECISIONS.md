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
