# Rules for everyone porting a crate (agents and sessions)

Read first: `CLAUDE.md` (repo root), `rust/MAP.md`, `rust/DECISIONS.md`. The user writes Vietnamese; every
string the user can see (errors, status text) stays **byte-identical** to the C++ (they are Vietnamese).

## The job
Port one part of the C++ engine to Rust, **faithfully**: same behaviour, same constants, same timeouts, same
error paths, same kernel-object / registry / file names, same threading idea. No "improvements" and no new
features. Keep the C++ file and function structure so the two can be read side by side. Comments in English.
When the C++ is ugly or odd, copy the behaviour and note it; do not fix it.

## Where you may work
- Only your own crate directory under `rust/` (named in your brief). Do not edit another crate, the workspace
  `Cargo.toml`, `docs/`, `src/`, `app/` or `CLAUDE.md`. You may add files under your crate and change your
  crate's `Cargo.toml` (add `windows` features, dependencies).
- `niwcam-core` already has: `crypto`, `json`, `nv12`, `image_settings`, `auto_frame`, `skin_smooth`. Use them,
  do not copy them. If you need a missing pure helper, put it in your own crate.
- Build with your own target dir so parallel builds do not block each other:
  `$env:CARGO_TARGET_DIR='D:\winlax\job\cam-studio\build\rust-<crate>'; cargo test -p <crate>`.
  Cargo.lock is shared: if cargo says it is locked or changed, just rerun.
  `cargo` lives in `%USERPROFILE%\.cargo\bin` (add it to PATH). Format only your crate: `cargo fmt -p <crate>`.
- Do not commit, push, create branches or worktrees. The lead commits.

## Never do these (the user's PC is in use)
- Do not stop, restart or touch the user's running `NiwCam.exe`, `C:\Program Files\CamStudio`, the Frame Server
  service, or any registered camera. No admin actions, no `regsvr32`, no `register.ps1`, no installing anything.
- Do not use the real kernel-object names (`Global\CamStudioVCam_*`, `Local\CamStudioVCam_*`), the real data
  folder `%LOCALAPPDATA%\NiwCam` / `CamStudio`, or the real registry keys (`HKCU\Software\NiwCam`, `...\CamStudio`)
  in tests: the running app owns them. Make them overridable for tests (a constructor argument or an env var such
  as `NIWCAM_TEST_*`; the default is the real name) and test with names that carry a random suffix. The data folder
  can already be overridden with `CAMSTUDIO_DATA_DIR`.
- Do not use TCP port 47321 / 47322 for tests (a real phone may use them); pick another.
- Do not connect to anything but loopback in tests.

## Code rules
- `unsafe` only for FFI/COM, as small blocks, each with a `// SAFETY:` comment. `#![deny(unsafe_op_in_unsafe_fn)]`.
- A panic must never cross an FFI/COM boundary (it would take down Frame Server or the app): wrap exported
  functions and COM methods in `std::panic::catch_unwind` and return an error code.
- No `unwrap`/`expect` on OS or I/O results where the C++ tolerates failure. Return the same HRESULT / error.
- Light on CPU and memory is a requirement (the app idles at ~1 % of a core): no polling loops where the C++
  waits on an event, no per-frame allocations where the C++ reuses buffers, no extra threads.
- Windows: `windows` crate 0.62. The static CRT is already set in `rust/.cargo/config.toml`.
- Keep `cargo clippy -p <crate> --all-targets -- -D warnings` clean (use `#[expect(lint, reason = "...")]`).

## Proving it (this is the real bar)
"It compiles" and "my own tests pass" are not done. Done = the Rust behaves like the C++ **on the same inputs**:
build the C++ counterpart tool from `build\Release` (already built; rebuild with `scripts\build.ps1 -Target <name>` if
you must) and compare outputs/behaviour, as your brief says. Put the comparison in a script under your crate
(`tests/` or `xcheck/`) so the lead can rerun it. Report honestly what you could not test (no hardware, needs admin,
needs the user's app closed) instead of claiming it works.

## Your final report (short)
1. Files written, with the C++ file each one ports.
2. The public API (Rust signatures) other crates need.
3. What you ran and the results (counts, diffs). Paste the key lines.
4. Known differences from the C++ and anything untested.
