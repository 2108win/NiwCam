# iPhone 4K60 chỉ ra ~40 fps: việc cho máy Mac

(2026-11-22. Đọc cùng ADR-011 trong `rust/DECISIONS.md`.)

## Đã biết (đo trên PC Windows, iPhone 14 cắm cáp USB, engine Rust, giải mã GPU)

| Chế độ | Khung/giây tới PC |
|---|---|
| 1080p60 | 60 |
| 4K30 | 30 |
| 4K60, 4K50, 4K48 (H.264 hoặc HEVC) | ~38–40 |

- PC không phải nút thắt: giải mã + xử lý ~11 ms một khung 4K, 0 khung bị bỏ, hàng đợi gửi 0 B, độ trễ ~20 ms,
  PC không báo `lost`. Điện thoại cũng không tự giảm bitrate (không có `congestion`).
- Con số ~40 giống nhau ở 60/50/48 fps và ở H.264/HEVC, nên không phải do codec hay do fps đặt: iPhone chỉ
  đẩy được ~40 khung 4K ra mỗi giây.
- iPhone có lúc báo `thermal` serious (cắm sạc, 70 %): PC khi đó tự hạ xuống 1080p30 (`MainWindow.Thermal.cs`). Phải
  loại trừ chuyện máy nóng trước khi kết luận về bộ mã hóa: để máy nguội rồi đo.

## Các nghi phạm (chưa chứng minh cái nào)

1. Bộ mã hóa ở chế độ độ trễ thấp (`kVTVideoEncoderSpecification_EnableLowLatencyRateControl`, `VideoEncoder.swift`):
   một số máy chỉ đạt vài trăm Mpixel/s ở chế độ này.
2. Video HDR tự bật (`automaticallyAdjustsVideoHDREnabled` mặc định bật): xử lý thêm ở 4K60.
3. Low-light boost (`automaticallyEnablesLowLightBoostWhenAvailable`): phòng tối, có thể ảnh hưởng tốc độ khung.
4. Hàng đợi `camstudio.capture` bận (`alwaysDiscardsLateVideoFrames = true` bỏ khung khi `captureOutput` còn chạy).
5. Nhiệt (xem trên).

## Cái đã thêm sẵn để đo (chưa chạy: viết trên Windows, không có Swift ở đó)

- `StreamDiag` trong `ios/CamStudio/VideoEncoder.swift`: mỗi giây đếm khung camera giao (`captureFps`), khung bị bỏ
  ở hàng đợi kèm lý do của iOS (`droppedFps`, `dropReason`, qua `captureOutput(_:didDrop:from:)`), khung vào/ra bộ mã hóa
  (`encodeInFps`/`encodeOutFps`), thời gian gọi `VTCompressionSessionEncodeFrame` lâu nhất (`encodeCallMs`) và độ trễ
  mã hóa trung bình (`encodeLatencyMs`). Đi vào `state` (docs/protocol.md 4.2) và log hệ thống
  (`subsystem vn.camstudio.phone`, category `stream`).
- Object `tune` tùy chọn trong `start`: `lowLatency` (false = bỏ chế độ độ trễ thấp), `realTime`, `maxFrameDelay`,
  `videoHDR` (false = tắt), `lowLightBoost` (false = tắt). Mặc định không đổi gì.
- `tools/fake_pc.swift`: `--tune '<json>'` và dòng `phone side: ...` trong bản tóm tắt mỗi luồng.

## Việc cần làm (cần iPhone cắm vào Mac)

1. `git pull`; `scripts/ios-run.sh` (có test mật mã, build, cài, mở app). Nếu không biên dịch được thì sửa lỗi cú pháp
   trong ba file Swift trên trước (code mới chưa qua trình biên dịch).
2. Để máy nguội (hoặc ghi lại `thermal` trong dòng tóm tắt), rồi đo từng cấu hình, 15 giây mỗi cái, ở 4K60:
   ```
   scripts/fake-pc.sh --usb --lens back-wide-1 --width 3840 --height 2160 --fps 60 --codec hevc --bitrate 40000 --seconds 15
   scripts/fake-pc.sh ... --tune '{"lowLatency":false}'
   scripts/fake-pc.sh ... --tune '{"videoHDR":false}'
   scripts/fake-pc.sh ... --tune '{"lowLightBoost":false}'
   scripts/fake-pc.sh ... --tune '{"lowLatency":false,"videoHDR":false,"lowLightBoost":false}'
   scripts/fake-pc.sh ... --codec h264     # so sánh
   ```
   (lens id lấy từ dòng `caps` mà fake-pc in; `--lens all` liệt kê.) Đọc `fps=` (đo theo pts) và `phone side:`.
3. Đọc kết quả:
   - `captureFps` ~40 (camera chỉ giao 40): nguyên nhân ở camera (HDR, boost, nhiệt, định dạng được chọn trong `pickFormat`).
   - `captureFps` 60 nhưng `droppedFps` ~20: hàng đợi bận → xem `encodeCallMs`; nếu lớn, `encode()` đang chặn hàng đợi
     (thử `maxFrameDelay`, hoặc đẩy `encode` sang hàng đợi riêng).
   - `encodeInFps` 60 nhưng `encodeOutFps` ~40: bộ mã hóa không kịp → `lowLatency:false` thường là lời giải;
     khi đó cân nhắc chỉ tắt chế độ độ trễ thấp cho 4K (pixel rate lớn), giữ nguyên ở độ phân giải thấp
     (độ trễ qua Wi‑Fi: docs/protocol.md 6).
4. Khi biết nguyên nhân: sửa mặc định (chỉ cho các chế độ nặng), chạy `CryptoTests`, đo lại 4K60 phải ≥ 58 fps ở
   `fake-pc --usb` và 1080p30/60 không đổi, rồi đo qua PC thật nếu được (`phone_perf`, `NIWCAM_TRACE=1`).
5. Ghi kết quả vào CLAUDE.md (mục iOS) và ADR-012 trong `rust/DECISIONS.md`. Một bản TestFlight mới mới đến tay người dùng.
