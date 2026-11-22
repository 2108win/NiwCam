# Rust port: where it stands (2026-11-22, evening)

Read first: `docs/HANDOFF.md` section 0 (the whole current state), then `rust/MAP.md`, `rust/DECISIONS.md`, `rust/AGENTS.md`.

- Every crate is ported and checked against the C++ with its own `xcheck/run.ps1` (core, shm, phone, media, fx, vcam,
  dshow) and `engine/xcheck/{abi,engine}.ps1`. `cargo test --workspace` and `cargo clippy --workspace --all-targets -- -D warnings` pass.
- Baseline of the C++ is in `rust/baseline/`.
- **The user's PC now runs the Rust core** (engine + Frame Server camera), version 0.1.15, built with
  `scripts\package-release.ps1 -Rust -SkipAndroid`. It is not published: the last release (0.1.14) is C++.
- Done live: Rust engine + Rust vcam under the real Frame Server, the real app, OBSBOT, Redmi over USB, iPhone over USB (1080p60, 4K30, 4K60), Discord (the user's own use).
- Done since 2026-11-22 morning: ADR-008 (background/face upgrade, opt-in "Chat luong cao"), ADR-010 (models fetched, not committed), ADR-011 (GPU decode, both engines).
- Open, each needing the user or a device: Windows 10 + the dshow filter; Wi-Fi at 4K; a real Discord/OBS/Zoom call at high resolution (`docs/HANDOFF.md` 0.3);
  the iPhone 4K60 -> ~40 fps cause (Mac, `docs/ios-4k60.md`); deciding to publish the Rust core.
- Known: `AppDataDir.h` line 17 `L"\NiwCam"` is a C++ typo (default folder `LocalNiwCam`); Rust copies it on purpose (`phone/src/appdata.rs`).
