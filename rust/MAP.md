# Rust port of NiwCam — map

Goal: the PC core (`src/engine`, `src/vcam`, `src/dshow`, C++) in Rust. The WinUI 3 app (C#) stays and
keeps calling the same C API (`EngineApi.h`) through a Rust `NiwCamEngine.dll`. Phone apps stay native.
Decisions: [DECISIONS.md](DECISIONS.md). One branch, `main`.

Method: hand port, one module at a time. The C++ checks are the bar (`niwcam-protocol-test`,
`niwcam-frame-check`, `niwcam-framing-check`, `niwcam-verify`). `rust-anything` is installed but cannot
bridge C++/C#.

## Waves

| Wave | Crate / module | State |
|---|---|---|
| 0-1 | `core`: crypto, json, nv12, image_settings, auto_frame, skin_smooth | **done**: `xcheck/run.ps1` identical to the C++ on 1863 lines |
| 2 | `shm` (SharedFrame, FrameWriter, FrameReader), `phone` (PhoneLink, discovery, USB, connector) | **done**, each with its own `xcheck/run.ps1` against the C++ |
| 3 | `media` (decoder, webcams, audio out, recorder), `fx` (pause picture, faces, background, window capture) | **done**, each with its own `xcheck/run.ps1` |
| 3b | `engine`: CaptureEngine + Pipeline, ScreenViewer, the 55 `cs_*` exports -> `NiwCamEngine.dll` | **done**: ABI layout and exports identical; `engine/xcheck/engine.ps1` runs the same scenario through the C++ and the Rust DLL: same behaviour, same RAM |
| 4 | `vcam` (`CamStudioVCam.dll`, Frame Server), `dshow` (`CamStudioDShow.dll`, x64+x86) | **done in isolation** (loaded in-process, not registered): exports identical, same samples/frames as the C++ |
| — | WinUI 3 app, Android, iOS | stay as they are |

**Proven on this PC (2026-11-22), with the user's NiwCam closed and put back afterwards:**
- Rust `NiwCamEngine.dll` + Rust `CamStudioVCam.dll` registered for the real Frame Server (admin, rolled back to the C++ DLL):
  `niwcam-verify` as the app read 244 live frames, 0 no-signal, 0 size errors, 30.3 fps, `--mode 1` (960x540) and `--reopen` fine;
  the same with the C++ engine under the Rust vcam. Identical numbers to C++/C++.
- The real WinUI app (`appin`) with the Rust engine DLL: tray idle 0.39 % CPU / 115 MB private (C++: 0.31 % / 115 MB), a consumer wakes the source the same way.
- CPU with a consumer reading, engine driver, 3 runs: C++ 3.66 s, Rust 3.77 s over 22 s.

**Not proven (needs hardware or the user):** OBSBOT and the Redmi/iPhone streaming through the Rust engine, a real
Discord/OBS/Zoom call, the dshow filter in a real app, Windows 10. Until then the shipped build is the C++ one;
`scripts\package-release.ps1` is not changed.

Sizes: engine ~13.5k lines C++ (CaptureEngine 2.0k, UsbTransport 0.9k, EngineApi 0.9k, SourceFilter 0.9k).

## Open questions

- Cutover per wave: ship a Rust `NiwCamEngine.dll` only after wave 3 (until then C++ and Rust meet only in tests).
- COM DLL names, CLSIDs and kernel object names must not change (see CLAUDE.md "stay CamStudio").
- Wave 4 needs `unsafe` (COM, Win32): separate crates, each `unsafe` block with a `// SAFETY:` note.

## Run

```powershell
cd rust; cargo test; cargo clippy --all-targets -- -D warnings
.\xcheck\run.ps1    # builds the C++ and the Rust driver, diffs every output line (needs VS Build Tools)
```

`xcheck` is the bar for each picture module: the same seeded inputs through the C++ headers and the
Rust code, output compared line by line. Add a case there whenever a module is ported.
