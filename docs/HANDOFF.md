# Handoff: unfinished work

> **Tóm tắt (2026-11-22):** lõi PC (engine + camera ảo + DirectShow) đã viết lại bằng **Rust**, đang chạy thật trên máy
> người dùng (**0.1.15 đã đăng release 2026-11-22**; bản C++ cuối là 0.1.14). Lag ở 4K/60 đã sửa phía PC bằng
> giải mã GPU (cả C++ lẫn Rust). **iPhone ~40 khung/giây ở 4K60: đã tìm ra và sửa trên iPhone** (mục 0.2; phần PC viết trên Mac, chưa
> biên dịch). Người dùng báo Discord vẫn giật khi dùng độ phân giải/tần số cao (mục 0.3). Mục 1 trở xuống là ghi chú cũ (2026-11-09).

> **Đã làm trên PC Windows (2026-11-22, tối):** build cả hai engine không lỗi; `cargo test --workspace`, `engine.ps1` (BEHAVIOUR SAME, 60 fps cả hai),
> `niwcam-protocol-test` đều đạt. **iPhone 14 thật, 4K60 qua cáp USB: ~60 fps** (trước ~40): Rust engine 5 phút liền trung bình 59,9 fps, lag đỉnh 33 ms,
> 7 khung bỏ/300 s, CPU ~77 % một lõi; C++ engine 60 fps nhưng nặng hơn (CPU ~100 %, bỏ 52 khung/15 s). Menu chế độ vẫn có 4K60/50/48 (HEVC), 4K30 vẫn H.264.
> **Còn lại:** mục 0.3 (Discord ở 4K, Wi‑Fi 4K60 thật); luật nóng máy của app chưa thử trong 5 phút (phone_perf không đọc `thermal`; fps không tụt);
> Lỗi thoát 0xC0000005 trong `dnsapi.dll` (engine Rust, ở `cs_shutdown`): **đã sửa** (2026-11-22): `DnsServiceBrowse` nhận địa chỉ một biến cục bộ rồi bản sao mới được đem đi hủy; giờ cái handle nằm trong `Box` (như biến toàn cục của C++), `shutdown_probe` thoát 0. Nằm trong **v0.1.16 (đã đăng 2026-11-22)**.
> TestFlight/App Store mới do người dùng quyết.
>
> **Wi‑Fi và app thật (2026-11-22, tối):** `phone_perf --wifi` 4K60 (HEVC), 30 s: 60 fps, lag 6–40 ms, 14 khung bỏ, CPU ~63 %. App NiwCam (bản sao của thư mục cài
> + `NiwCamEngine.dll` mới, đặt trong scratchpad) chọn iPhone › 1× › 4K60: `NIWCAM_TRACE` 60 fps (decode+pipeline ~6 ms); `niwcam-verify --mode 0 --hold 20` qua Frame Server
> thật: 1198/1200 khung live, 60,0 fps, khoảng cách khung 8–25 ms. **Đã cài lại 0.1.15 (Rust + ADR-012) lên máy lúc 19:58 (2026-11-22): app cài chính, iPhone 1× 4K60 = 60 fps (`NIWCAM_TRACE`).**

For AI agents picking the work up on any machine. Read CLAUDE.md first, then this file, then `rust/MAP.md` and
`rust/DECISIONS.md` (ADR-001..012). Update this file in the same commit as each step; delete it once everything is tested.

## 0. State on 2026-11-22

### 0.1 What exists and what runs where

| | State |
|---|---|
| `main` | up to date on GitHub (`origin/main`), one branch only (CLAUDE.md rule). Last code commits: `828208b` (iPhone encoder rule, built and measured), `26d266a` (PC asks HEVC for heavy iPhone modes, NOT compiled), `987746c` (fake-pc per-frame CSV); before: `f1213be` (GPU decode, 0.1.15). |
| Rust port (`rust/`) | every crate ported and compared with the C++ (`xcheck/run.ps1` per crate, `engine/xcheck/{abi,engine}.ps1`); `cargo test --workspace` passes. See `rust/MAP.md`. |
| Installed on the user's PC | **0.1.15 with the Rust core** (engine, Frame Server camera `CamStudioVCam.dll`; the DirectShow filters are in the installer but only installed on Windows 10). Built by `scripts\package-release.ps1 -Rust -SkipAndroid` -> `build\dist\NiwCam-Setup-0.1.15.exe` (git-ignored; the script wipes `build\dist`). Not published, not tagged. The optional models (MODNet, YuNet) are in `app\Assets\Models` via `scripts\fetch-models.ps1`. |
| Rollback to the C++ build | reinstall the published `NiwCam-Setup-0.1.14.exe` from GitHub releases (`2108win/NiwCam`), or build C++: `scripts\package-release.ps1` without `-Rust`. The C++ `CamStudioVCam.dll` registration is restored by installing it; `rust\vcam\xcheck\frameserver.ps1 -Restore` (admin) only restores what that script saved. |
| Last published release | **v0.1.16 (2026-11-22, the dnsapi exit fix); v0.1.15 (Rust core, tag on a2b214b)**; assets under both names (setup + APK, 0.1.14 was the last C++). Rollback: reinstall `NiwCam-Setup-0.1.14.exe`. The updater will move every 0.1.14 install to the Rust core. Windows 10 (DirectShow filter) still untested on a real PC. |
| iPhone app | **1.0.1 (ADR-012) is live on the App Store (Ready for Distribution, seen 2026-11-24). 1.0.2 (build 20261124.1035, `main` at 6649d8d, uploaded as 571514b before a rebase: iOS 16 support for the iPhone 8 / X, CLAUDE.md "iOS 16") uploaded and submitted to App Review 2026-11-24, "Waiting for Review", released automatically once approved**; it is also in TestFlight (internal group). Not run on an iOS 16 iPhone. After it is live: README badge/table say iOS 17 and must say 16. The user's iPhone runs a dev Release build (same bundle id, replaces the App Store install); installing from the App Store replaces that. |

### 0.2 iPhone 4K60 at ~40 fps: cause found (2026-11-22, Mac)

Write-up: **[docs/ios-4k60.md](ios-4k60.md)**, decision ADR-012. The low-latency encoder works one frame at a time (~26 ms per 4K frame).
- iOS: HEVC above 4K30 pixels/s or 60 fps uses the normal encoder: 4K60 HEVC 37.6 -> 60 fps (~37 ms), 1080p120 108 -> 120. H.264 unchanged
  (~40 at 4K60) because its normal encoder only reaches 50 fps at 139 ms. Built (Release), CryptoTests pass, measured on the iPhone 14.
- PC, both engines: ask HEVC for exactly those iPhone modes (`iphone_needs_hevc` in `rust/engine/src/api.rs`, `IphoneNeedsHevc` in
  `src/engine/EngineApi.cpp`). **Written on the Mac; compiled and measured on the PC (see the note at the top).** Was: build, then the real iPhone at 4K60 over USB through the
  virtual camera (`phone_perf --usb --format 3840x2160@60`, `NIWCAM_TRACE=1`): expect ~60 fps; the mode menu should still show 4K60. Run
  `rust\engine\xcheck\engine.ps1` (both engines changed). Then 5 minutes of 4K60 on the cable watching `thermal`: the cable charges the phone,
  and `serious` makes the PC drop to 1080p30 (`MainWindow.Thermal.cs`), which would hide the fix. The iPhone runs the dev build with the fix:
  keep it until this passes; only then a TestFlight/App Store build (the user's call).
- Wi‑Fi at 4K could not be judged from the Mac (iPhone -> router -> Mac carried ~15-18 Mbit/s, one 7 s stall; the phone lowered the bitrate as
  designed; keyframes are small, 52-156 KB). Measure on the real PC (0.3).

### 0.3 Discord still stutters at high resolution/rate (open, not reproduced by us)

What we know (2026-11-22, Rust engine, Redmi/iPhone):
- The PC chain is healthy at 4K60: fake phone replaying a 4K60 stream through the **real Frame Server**, read by `niwcam-verify`: mode 4K60 = 60.1 fps,
  4K30 = 30.0, 1080p60 = 60.0, 0 no-signal, 0 size errors (frame gaps 8-25 ms at 4K60).
- With the real iPhone the source itself is the weak part (0.2) and it overheats; the app's hot-phone rule steps down to 1080p30 and the
  virtual camera size can stay 3840x2160 (an app holding the camera keeps its size, the picture is fitted): Discord then gets 4K frames.
- Discord never sends 4K camera video; asking it for 4K60 only adds work on its side. Fixed output size 1080p is in Settings (the user's earlier choice for calls).
- NOT done: watching a real Discord call at 4K while tracing. Do it: close NiwCam, start it from a shell with `NIWCAM_TRACE=1` and stderr redirected
  (`Start-Process ... -RedirectStandardError`), join a call, read `[trace]` lines (decode+pipeline ms, `lag peak`, `skipped`, `lost`) and watch the
  shared-memory write rate (a 20-line script reading `Global\CamStudioVCam_Frames_v2`, header `latest >> 2`, spin loop, not `sleep`).
  A first reading at 4K Wi-Fi showed only ~13-17 frames/s written to the camera while the preview said 43: not explained yet.
- Wi-Fi at 4K60 was never measured (the cable was always in; the app prefers the cable). Unplug it, then run `phone_perf --wifi`.

### 0.4 Decisions open for the user

- Ship the Rust core in a release? `package-release.ps1 -Rust` builds it (runs `fetch-models.ps1`, shows the two licence notices,
  see `docs/third-party-models.md`). Needed first: a real Windows 10 PC for the DirectShow filter, iPhone/Wi-Fi runs, a real Discord/OBS/Zoom call.
- The hot-phone rule (`MainWindow.Thermal.cs`) lowers the mode to <= 1080p30 / 720p: it works against "native resolution always". Option: make it a
  setting, or warn only. Not changed.
- C++ `AppDataDir.h` typo (`L"\NiwCam"` -> default folder `LocalNiwCam`), copied on purpose in `rust/phone/src/appdata.rs`; fix both together.

### 0.5 How to measure (all built under `build\`)

- `NIWCAM_TRACE=1` (Rust engine): per second on stderr, `decode+pipeline` avg/max ms, `lag peak`, `skipped`, `lost`, `pending B`, `pipeline fps`.
  `NIWCAM_DECODER=software` forces the old software decode (both engines). Decode path and numbers: ADR-011.
- `rust\engine\examples\phone_perf.rs` (`cargo build --release -p niwcam-engine --examples --target-dir build\rust-engine`):
  `phone_perf --dll <NiwCamEngine.dll> --camera <name part> [--usb|--wifi] [--list] [--format 3840x2160@60] [--seconds N]` streams a real phone/webcam and
  prints fps/lag/skips/CPU per second. Uses the real pairing keys (`CAMSTUDIO_DATA_DIR=%LOCALAPPDATA%\NiwCam`), session-local channel (does not touch the
  installed camera). **Quit NiwCam first**: the phone serves one PC at a time and the app takes it back.
- `build\Release\niwcam-fake-phone.exe --replay <gen_stream .bin> --size WxH --fps N --codec h264|hevc` (`tools/fake_phone.cpp`): any size/rate with no encoder
  load. Streams from `build\media-xcheck\gen_stream.exe <h264|hevc> W H frames out.bin` (e.g. `build\bench\h264-4k.bin`). With
  `rust\engine\examples\driver.rs --dll ... --phone <fake> --phone-args "--replay <ABSOLUTE path> ..." [--real]` (`--real` = Frame Server, then read it with
  `niwcam-verify --expect WxH --mode N --hold S`; the fake phone runs with the data folder as cwd, so give an absolute stream path).
- `rust\media\examples\decode_bench.rs`: `decode_bench <h264|hevc> W H stream.bin [rounds]`, hashes every frame (`PER_FRAME=1`, `DUMP_DIR=`): hardware = software.
- `rust\media\xcheck\run.ps1 -Stage decode` runs with `NIWCAM_DECODER=software` on purpose (the C++ and the GPU path differ by one HEVC frame of latency).

### 0.6 Pitfalls learned this session

- `niwcam-verify` gets `0xC00D3704` while another app (Discord) holds the virtual camera. Check `HKCU\...\CapabilityAccessManager\ConsentStore\webcam` for `LastUsedTimeStop = 0`.
- The computer-use tool refuses clicks when the desktop shell is frontmost: call `open_application NiwCam` first. NiwCam's mode menu: the first click can land on a different row if the list scrolled;
  hover "Tất cả chế độ" first and take a screenshot before clicking.
- Killing a running `package-release.ps1` leaves MSBuild node processes: harmless.
- Writing Python patches through a bash heredoc breaks on `'` and `\` in the text: put the script in a file (the Write tool) and run it.
- The Rust engine used for dev tests is `build\rust-engine\release\NiwCamEngine.dll`; what is installed (and what `-Rust` packs) comes from `build\rust-ship`.

### 0.7 Verified vs not (this session)

Verified: GPU decode correct (hash per frame = software, 640x480..4K, H.264/HEVC, size changes) and fast (2.7-2.8 ms per 4K frame); 4K60 through the engine
28 -> 61 fps with a fake phone; real iPhone 4K30 = 30, 1080p60 = 60 via the cable; Redmi 1080p30 over USB; installed 0.1.15 starts, the camera lists 14 modes.
Not verified: the new code on a Windows 10 PC; HEVC 10-bit / HLG through the GPU path (falls back to software by design, never exercised); Wi-Fi at 4K60;
a real Discord/OBS/Zoom call with the Rust core; the rebuild-on-faster-source rule (`Pipeline::relayout`, both engines; compiled and reviewed, not run in the exact
"same size, faster rate" case); the HEVC-for-heavy-modes rule on other iPhone models and over Wi‑Fi.

## 1. What is not verified yet (older notes, 2026-11-09)

| Piece | Built | Tested on a device |
|---|---|---|
| **Windows 10 DirectShow filter** (x64+x86), engine run-time API pick | yes | **published in v0.1.6**; Win11 forced path verified; **no real Win10 PC** yet |
| **iPhone re-pair on key mismatch** (PC `pair` only on clean close) | yes | in v0.1.6; iPhone streamed 2K/60 with the saved key (no mismatch hit); iOS `Store` compiled on the Mac 2026-10-21 (app + screen extension, CryptoTests pass); the re-pair itself not yet hit on a device |
| USB: Redmi over adb, engine starts/stops adb | yes | yes |
| USB: iPhone through the Apple service on Windows | yes | **no** |
| Webcam found again after unplug/replug | yes | **no** |
| Probe-safe phone servers | yes | iPhone yes, Redmi yes |
| Screen over USB ("… · Màn hình (USB)") | yes | **no** (the Redmi needs PIN + consent) |
| PC phone screen in its own window, "Dùng làm camera ảo" | yes | **no** |
| Android `CameraService` (camera runs in the background) | yes | yes (screen off, locked) |
| Android UI split Camera / Màn hình | yes | camera part only |
| auto/busy/ping | yes | auto yes; busy (two PCs) **no** |
| PC connector, seamless switch, size "Tự động" | yes | yes (with `camstudio-verify`, not Discord) |
| PC window as the camera source | yes | yes (test window) |
| **"Tự căn khung" + Smart Zoom** (`AutoFrame.h`, `FaceDetect.cpp`, switch in the Camera tab), 2026-10-19 | **compiled on Windows 2026-10-19** (engine + app, 0 warnings) | `framing-check` 37/0, `frame-check` 375/0, protocol ALL PASS; **OBSBOT: follows a face** (1.1× at 1080p, "nét"), Redmi with nobody in view: "Không thấy ai". Fix needed first: FaceTracker wants `RelativeTime` on each frame, else E_FAIL every frame |
| PC camera always on, automatic source pick | yes | yes |
| PC UI 2026-10-19: device cards, camera bar under the preview, colour presets, (i) help, **Khởi động lại** | yes | dev build: dark + light theme, 960 px window, presets, restart (visible, hidden `--tray`, and during a Discord call: Discord then needs its camera off/on once, picture back at once) |
| PC design rules (spacing, radii, one status palette, screen-reader names), 2026-10-20 | yes | dev build: dark + light theme, menus, Settings |
| PC icons: bundled Fluent UI System Icons font (`tools/make_icon_font.py`) | yes | dev build on Win11; **not seen on Windows 10** |
| Phone mic → PC → VB-Audio "CABLE Input" (Android `MicCapture`, PC `AudioOut`, protocol 5.1) | yes | fake phone tone → WAV: 440 Hz, 48 kHz, no gaps, live off; Redmi over USB: sound with the screen off and locked, denied → camera fine and the reason shown; **not yet: the PC-triggered permission prompt, the WASAPI path into CABLE (VB-Cable not installed), a hidden window in a real call, iOS (not written)** |
| PC "Nền": blur / image behind the person (Windows ML, PP-HumanSeg, in Assets/Models) | yes | OBSBOT, empty room: blurred whole, image mode replaces it; 1–2 ms a look; 3.1 → 5.4 ms a frame at 1080p100. Installed 0.1.8, the user in frame: sharp person and hair, room blurred. **Not yet: a phone camera, Windows 10** |
| PC "Làm mịn da" (faces, guided filter) | yes | `frame-check` 379/0 (4 new); installed 0.1.8 at 70 on the user's face: smoother skin, glasses and eyes sharp, no ellipse edge, "1 khuôn mặt" |
| PC: a hot phone drops to 1080p30 / 720p30 (`state.thermal`) | yes | fake phone `--thermal serious`: 720p60 → 720p30, warning; "Quên ghép đôi" on the fake phone: key gone, camera moved to the webcam; **not on a real hot phone** |
| PC snapshot (PNG) and recording (MP4 H.264 + mic AAC) of the camera's picture | yes | OBSBOT: PNG 1920×1080, MP4 4.4 s and 17.7 s with the window hidden (~50 fps, sound track); **not tried: a phone source, paused while recording, no microphone, Windows 10** |
| PC phone-app QR ("+" › Tải app cho điện thoại…, empty preview) | yes | 16/16 codes (versions 1–10, Vietnamese text) read back by zxing-cpp; **not scanned with a phone camera** |
| PC phone-app QR: two codes (Android release page, iPhone TestFlight `join/h3NKXEjD`), 2026-10-28 | yes (Windows, 2026-10-29) | dialog: both codes decoded by zxing-cpp from a screenshot, the TestFlight page answers "Join the Cam Studio beta"; **the empty-preview row (no source) not seen** |
| PC pause picture (blurred last picture + text, card, own image, no signal) | yes | blurred card seen through `camstudio-verify --dump` (29 fps, 0 no-signal), resume live; **card / own image / Windows 10 path not looked at** |
| PC global shortcuts (P pause, N next, C snapshot, R record, B effects, with Ctrl+Alt) | yes | hidden: with `camstudio-verify --hold` on the camera (no frame lost) and in standby (snapshot woke the webcam); a taken combination (Zalo's Ctrl+Alt+S) shows the warning. **Not in a real Discord call** |
| PC source list: working sources first, "Chưa kết nối" below; right-click "Quên ghép đôi…" / "Xóa khỏi danh sách"; add by IP without a port | yes | order, menu, add + remove a test IP; **"Quên ghép đôi" not run** (needs re-pairing a real phone) |
| iPhone PiP after leaving the app (`BackgroundCamera.swift`, voip) | yes | **no** |
| Installer with adb | yes | yes (installed since 0.1.4) |
| Discord over Wi‑Fi (Redmi), screen off | yes | yes |
| Automatic updates, PC (`Updater.cs`) | yes | yes (0.1.4 → test 0.1.5); "Không" at UAC **no** |
| Automatic updates, Android (`AppUpdater.kt`) | yes | 0.1.4 installed; update path **no** (next release) |
| PC lighter and smoother (2026-10-30, CLAUDE.md "Light and smooth"): even 30 fps preview, SSE2 colour, effects capped to the camera's rate, tray trims | yes (Windows), **published in v0.1.12** | OBSBOT: preview 30 fps, blur + reader 140 % → 28 % CPU, tray (Task Manager) 38 → 24 MB; `camstudio-verify` live, 0 no-signal. **Not yet: a phone source, Windows 10, the preview looked at by a person** |
| PC Settings: "by Win Lã" link under the version (`Assets/WinLa.gif`, `MakerLink_Click`), 2026-10-24 | yes (Windows, 2026-10-29) | dev build: logo shows, the GIF has no loop extension (28 frames once), the link opens winlax.id.vn; **with Windows animations off: not looked at** |

## 2. The user's decisions (do not ask again)

- Bundle adb in the Windows installer.
- iPhone in the background: Picture in Picture plus the `voip` background mode. The App Review risk
  is accepted.
- Several sources: stay connected, switch seamlessly (not all streaming at once).
- Virtual camera size "Tự động, ưu tiên nét nhất":
  - no app on it: the source's native size;
  - during a call: keep the size, fit with black bars;
  - after the call: native again.
- Both: the phone screen in its own PC window, and a PC app window as the camera source.
- The virtual camera exists for as long as the app runs ("no signal" without a source).
- iPhone screen sharing stays on ReplayKit broadcast. No AirPlay receiver (clean-room rule).
- "Lật ngang" applies to every source, a PC window included (it mirrors its text): keep it so.
- Git: only `main` (see CLAUDE.md, Ground rules).

## 3. Next steps (need someone at the phone or PC)

- **Rename to NiwCam (0.1.14, built and PUBLISHED 2026-11-09, tag on 161b581).** Display text everywhere; on the PC also the exe (`NiwCam.exe`), namespace, `NiwCamEngine.dll`, `niwcam-*` tools, installer, `%LOCALAPPDATA%\NiwCam`, `HKCU\Software\NiwCam`, the `Run` value; repo renamed by the user to `2108win/NiwCam` (GitHub redirects the old API URL with a 301: checked, `releases/latest` answers; installed 0.1.12 builds still use the old URL). What stays `CamStudio` is in CLAUDE.md. Version 0.1.14 (versionCode 16) also carries the 0.1.13 low-latency work. Logo = a lens on a deep-blue tile (`scripts/make-icon.ps1`: .ico, tray icon with a red dot, taskbar badge, iOS 1024 px, site icon; Android vectors by hand). New: red dot while an app uses the camera (tray icon, taskbar badge, tooltip names the app, silent balloon when hidden, switch in Settings). Release assets go up under BOTH names (`package-release.ps1`), because builds before 0.1.14 only look for `CamStudio-Setup-<v>.exe` and `CamStudio-<v>-android.apk`.
  **Tested on the user's PC (real context):** the installer over the previous layout: `CamStudio.*` gone, `NiwCam.exe` back up, the real data folder moved whole (client id f612b066, 4 pairing keys, phones.txt unchanged), the 37 settings and the 5 per-phone modes copied to `HKCU\Software\NiwCam` (the old key stays), the `Run` value rewritten to `NiwCam.exe --tray`, no crash log; `niwcam-verify` 361/361 live at 30 fps, gaps 32-34 ms. Backup of the real data before the test: `build/realstate/backup` (git-ignored).
  **GOTCHA for whoever tests next:** the Claude desktop app is a packaged (MSIX) app and its shell tools see a VIRTUALISED `%LOCALAPPDATA%` and `HKCU` (writes go to `...\Packages\Claude_...\LocalCache\Local`). An app launched from there migrates and writes into that copy, not the user's real folder. Run real-context checks through Explorer: `Start-Process explorer.exe <script.cmd>` (see `build/realstate/*.cmd`). Program Files, HKLM and the installer's work are real.
  **Not tested:** the red dot and the title-bar / tray / taskbar / Start icons on screen (the user declined screen access: look at them), a phone reconnecting without a code after the move (open the phone app), the Android update path and the iOS build (Swift, needs a Mac), Windows 10, Discord after the rename (pick "NiwCam" again; pinned taskbar shortcuts to the old exe break).
  **Published** (2026-11-09, `scripts/publish-release.py`): four assets, the setup and the APK under both names (same bytes: setup sha256 19b17499..., APK d28dfb56..., versionCode 16). Checked after: `releases/latest` through the OLD api URL (301 to NiwCam) lists v0.1.14 with both names and their digests; the old-name setup downloads with the right hash. The tag sits on 161b581 (the user's iOS commits came in after the build; PC/Android/installer sources are the same as the build at 96da585). **Not yet seen:** an installed 0.1.12 on another PC updating itself from GitHub (the fake-feed run and a real install over the old layout passed on the user's PC), the Android update path. **Left:** winlax-v2 (site text, links and icon changed, tests pass, NOT committed or pushed: it deploys publicly); the user sets the GitHub repo description; the iOS side is in the next items.
- **iOS và TestFlight:** **đã tải lên 2026-11-09** từ `main` 96da585, build 20261109.0023: biên dịch không lỗi, CryptoTests PASS, `nm -u` chỉ UserDefaults, app và tiện ích đều tên "NiwCam". Chưa chạy trên iPhone (máy không cắm).
  - Đã đổi trong cây: `CFBundleDisplayName` = "NiwCam" ở `ios/Info.plist` và `ios/CamStudioScreen/Info.plist` (tên dưới icon, tên trong Trung tâm điều khiển), ba chuỗi quyền (camera, micro, mạng cục bộ), các câu báo lỗi trong `CamStudioApp.swift`, `CameraSource.swift`, `CameraStreamer.swift`, `PhoneServer.swift`, `Session.swift`, icon mới (ống kính, `ios/Resources/Assets.xcassets/AppIcon.appiconset/AppIcon.png`, vẽ bằng `scripts\make-icon.ps1` trên Windows; `scripts/make-icon-ios.swift` đã xóa), `ios/README.md`.
  - **Không đổi, cố ý:** bundle id (`CAMSTUDIO_BUNDLE_ID`, `vn.camstudio.phonewl` trên team riêng), tên target và project Xcode, keychain group, `_camstudio._tcp`, mọi nhãn mã hóa. Đổi sẽ tạo app mới, mất khóa ghép đôi và đường cập nhật TestFlight.
  - **App Store Connect (chỉ người dùng làm được):** bản ghi "Cam Studio" đổi tên thành "NiwCam" (tên phải còn trống trên App Store; nếu bị trùng, tên hiển thị dưới icon vẫn là "NiwCam"), tên và mô tả của nhóm beta TestFlight (trang tham gia vẫn ghi "Join the Cam Studio beta" cho tới khi đổi). Link công khai `testflight.apple.com/join/h3NKXEjD` giữ nguyên, nên mã QR trong app PC và trang winlax.id.vn/camstudio không phải đổi.
  - Chưa thử trên máy (cài build 20261109.0023 từ TestFlight): tên và icon NiwCam, màn hình chia sẻ trong Trung tâm điều khiển, PiP, "Tắt màn hình", phần giảm trễ Wi‑Fi của 0.1.13 (`MaxKeyFrameInterval`, `noDelay`), micro iPhone (chưa viết).

- **Phone mic, still open (code review 2026-10-21, on the Mac):** `RunPhone` reads `audio` only
  from later messages, not from the first `started` (`CaptureEngine.cpp` ~1575: add
  `audioFrom(v)` there). So a mic denied at start shows on the PC only after a `set` (the mic
  button toggled), and a phone recording at 44.1/16 kHz would play at 48 kHz. Also: "Tạm dừng
  hình" (`cs_switch_none`) stops the phone's sound too, in a call.

- **v0.1.12 published** (2026-10-30, tag on 4002312, setup sha256 a3d74240…, APK 40a33935…,
  versionCode 14, no Android code change): the "Light and smooth" work in CLAUDE.md. Installed on
  the user's PC over 0.1.11 the updater's way (`/SILENT /RELAUNCH=tray /LOG`): succeeded, no
  rollback, installed files match the build, settings untouched, back in the tray, virtual camera
  live. Not yet: an installed 0.1.11 elsewhere updating itself, a phone source, the preview looked at.
- **v0.1.11 published** (2026-10-29, tag on e2a4572, setup sha256 e3fa551e…, APK 12757a4a…,
  versionCode 13, no Android code change): two phone-app QR codes, "by Win Lã" in Settings.
  Installed on the user's PC with `/LOG`: succeeded, no rollback, installed files match the
  build, settings untouched (the size was already "Tự động"), virtual camera 181/181 live. No
  phone tested (Redmi not plugged in, iPhone app closed). The phone-mic bug above is still in it.
- **v0.1.10 published** (2026-10-21, tag on 91dae8f, setup sha256 10de9da0…, APK baf1f5ec…): Thành phần bổ sung (Apple Devices, VB-CABLE), Windows accent +
  Settings › Màu nhấn, bigger centred icons, one card per phone, crash log. Tested on the user's
  PC: winget install of Apple Devices, iPhone over USB (~130 ms vs ~185 ms Wi‑Fi), VB-CABLE
  install through the app, live accent change. Not clicked through: the in-app Apple Devices
  flow end to end (the same steps were run by hand). **iPhone "Tắt màn hình"** (black cover,
  auto after 60 s) is in TestFlight build 0.1.0 (20261021.2339), uploaded from the Mac; untested
  on the device (PiP with the cover above it).
- **v0.1.9 published** (2026-10-21, tag on 07218c5): `CamStudio-Setup-0.1.9.exe` (sha256
  d9b48899…) and `CamStudio-0.1.9-android.apk` (versionCode 11, sha256 1fa84ed7…, the same APK
  tested on the Redmi). Redmi over USB: pairing survived the upgrade; mic denied → camera fine,
  reason shown; mic allowed → sound, also screen off and locked. Left: with the mic denied, the
  PC's request popping the prompt and the sound starting once allowed; VB-Cable (CABLE Input →
  CABLE Output); Android 14+ service rules; the installed 0.1.8 updating itself to 0.1.9.
  The Redmi's "Chạy nền" was switched on by a stray adb tap during the test.
- **v0.1.8 published** (2026-10-20, tag on d7ac5cd): both digests match; the installed 0.1.7
  saw it within seconds and updated, but the install rolled back half-done (adb.exe locked by a
  test loop of ours, see CLAUDE.md Gotchas); reinstalled with `/LOG`: succeeded, 0.1.8 running.
  The installer now skips an unchanged adb (next release). The Redmi should offer 0.1.8 too.
- **v0.1.7 published** (2026-10-20, tag on 9bdd907): installed on the user's PC; GitHub
  `releases/latest` is v0.1.7, both digests match. Installed 0.1.6 PCs and the Redmi update
  themselves from it (not yet watched on a device: the Redmi's update path is still untested).

- **"Tự căn khung" on Windows:** built and seen working 2026-10-19 (OBSBOT, dev build run from `app/bin`). Still to try: the
  OBSBOT and a phone at 4K: it follows a face, goes wide after ~4 s with nobody, the hint shows
  the crop size. Measure "Chi tiết › Cam Studio dùng" (the whole app; the tracker has its own
  thread) and `processMs` with it off and on, at 4K and 1080p (framed means a crop-scale every
  frame). With it on, the window hidden and no app on the camera: no detector work. Check: screens
  and windows are never framed, tap-to-focus lands right while framed, a Discord call.

0. **v0.1.5 is published** (2026-10-18, the new PC layout): the user's PC updated itself 0.1.4 ->
   0.1.5 (UAC, relaunch, not elevated). The Redmi still runs 0.1.4: opening the app there should show
   "Có bản mới 0.1.5". Check the confirmations and that the app restarts as 0.1.5 (needs the PIN).
1. **Redmi, unlocked:** "Chia sẻ màn hình" → on the PC: "Redmi 9T · Màn hình (USB)", the screen
   window, "Dùng làm camera ảo"; stop/start sharing while an app holds the camera.
2. **Unplug tests** while Discord (or `camstudio-verify --hold`) holds the camera: the phone cable,
   the OBSBOT (replug into another port).
3. **Discord call:** turn the phone, switch sources, pause. **Seen 2026-10-19:** the phone turned
   portrait → landscape in a call keeps the portrait camera (fitted, `PlanOutput` with
   `consumerHolds`), so Discord shows a letterboxed/cropped landscape picture. A new size needs a
   rebuild, which Discord does not survive. **The user chose a fixed 1080p** (Settings, like Camo).
   Verified 2026-10-19 (Redmi, dev build): with no app on it, a portrait picture keeps the camera
   at 1920×1080 (no rebuild); in Discord portrait shows with side bars, landscape fills the tile,
   the camera never drops. The size had been found back at "Tự động" once (cause unknown). A new size applies
   only once Discord has let go for 5 s (`kConsumerGraceMs`): toggling the camera off and on
   quickly keeps the old size, which is what confused the user first.
4. **Two PCs** with one phone: the second shows "Bận với PC khác".
5. **iPhone:** Apple Devices on Windows. PiP and "Tắt màn hình": install TestFlight build 20261109.0023
   or `scripts/ios-run.sh` on the Mac (ask first: it replaces the TestFlight install).

## 4. Pitfalls

- iOS signing: `ios/Signing.local.xcconfig` is git-ignored (team `2TLZSD8YW7`, bundle id
  `vn.camstudio.phonewl`). Never sign a new bundle id with team `8VVXM8DN6B` (the employer).
- One PC per phone: a test from the Mac drops the user's Windows PC. Ask first.
- The iPhone is the user's own phone. Ask before launching apps on it or installing a build.
- The Redmi locks with a PIN after a while: never enter it, never change its screen settings.
- Don't run `usb-check hello` while the PC streams: a valid hello replaces the PC's session.
- `docs/camo-gap-analysis.md`: keep the clean-room rule in mind before using it.
