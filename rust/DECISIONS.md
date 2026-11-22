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

## ADR-003: No `unsafe` outside the OS crates
Status: accepted (2026-11-22)
Decision: `niwcam-core` is `forbid(unsafe_code)`. Win32/COM lives in later crates, each `unsafe` with `// SAFETY:`.

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

## ADR-007: Plain loops instead of SSE2
Status: accepted (2026-11-22)
Decision: `nv12.rs` has no SIMD; `xcheck` shows the same bytes as the SSE2 C++. Speed is measured when the
module runs inside the engine (wave 3); `std::simd` only if a frame costs more than in the C++.
