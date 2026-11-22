# Rust port: where it stands (2026-11-22)

Read first: `rust/MAP.md`, `rust/DECISIONS.md`, `rust/AGENTS.md`.

- Every crate is ported and checked against the C++ with its own `xcheck/run.ps1` (core, shm, phone, media, fx, vcam,
  dshow) and `engine/xcheck/{abi,engine}.ps1`. `cargo test --workspace` and `cargo clippy --workspace --all-targets -- -D warnings` pass.
- Baseline of the C++ is in `rust/baseline/`. The C++ is still what ships.
- Next, in order, each needing the user: (1) close their NiwCam and run the Rust `NiwCamEngine.dll` under the real app
  (`scripts\build.ps1 -Target app` copies the DLL), with the C++ vcam; (2) register the Rust vcam (admin, `scripts\register.ps1`
  style, keep the C++ DLL for rollback) and test with Discord/OBS; (3) real webcam/phone/iPhone runs; (4) Windows 10 and the dshow filter.
- Then ADR-008: background/face upgrade, from `rust/research/background-and-faces.md`.
- Known: `AppDataDir.h` line 17 `L"\NiwCam"` is a C++ typo (default folder `LocalNiwCam`); Rust copies it on purpose (`phone/src/appdata.rs`).
