# Rust port of NiwCam — map

Goal: the PC core (`src/engine`, `src/vcam`, `src/dshow`, C++) in Rust. The WinUI 3 app (C#) stays and
keeps calling the same C API (`EngineApi.h`) through a Rust `NiwCamEngine.dll`. Phone apps stay native.
Decisions: [DECISIONS.md](DECISIONS.md). One branch, `main`.

Method: hand port, one module at a time. The C++ checks are the bar (`niwcam-protocol-test`,
`niwcam-frame-check`, `niwcam-framing-check`, `niwcam-verify`). `rust-anything` is installed but cannot
bridge C++/C#.

## Waves

| Wave | Module (C++ file) | State |
|---|---|---|
| 0 | `crypto` (Crypto.cpp) — ECDH, HKDF, AES-GCM, SAS | **done**: `docs/protocol-vectors.json` matches, 6 tests, clippy clean |
| 1 | `json`, `image_settings`, `nv12`, `auto_frame`, `skin_smooth` | **done**: 31 Rust tests (ports of `frame-check` and `framing-check`), clippy clean, and `xcheck/run.ps1` shows 1863 output lines identical to the C++ |
| 1b | `shared_frame` (SharedFrame.h), `frame_writer` (FrameWriter.h) | open: Win32 shared memory, moves to wave 2 with the OS crate |
| 2 | `phone_link` (PhoneLink), `connector` (Connector), `discovery` (mDNS), `usb_transport` (adb, usbmuxd), `video_decoder` (Media Foundation) | open |
| 3 | `capture_engine` (CaptureEngine, 2000 lines), `mf_devices`, `window_source`, `screen_viewer`, `recorder`, `audio_out`, `still_picture`, `segmenter`, `face_detect` | open |
| 4 | `vcam` (COM media source, Frame Server), `dshow` (x64 + x86 filter), C API cdylib `NiwCamEngine.dll` | open |
| — | WinUI 3 app, Android, iOS | out of scope |

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
