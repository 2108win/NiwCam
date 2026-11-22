# iPhone 4K60 chỉ ra ~40 fps: đã tìm ra (2026-11-22)

Quyết định: ADR-012 trong `rust/DECISIONS.md`. Đo trên máy Mac với iPhone 14 qua Wi‑Fi (`fake-pc --host`), bản Release,
`back-wide`, máy nguội (`thermal` nominal suốt). Số "phía iPhone" là bộ đếm `StreamDiag`, không phụ thuộc đường truyền.

## Nguyên nhân

Bộ mã hóa ở chế độ độ trễ thấp (`kVTVideoEncoderSpecification_EnableLowLatencyRateControl`) làm **từng khung một**: lệnh
`VTCompressionSessionEncodeFrame` chỉ trả về khi khung đã mã hóa xong. Một khung 4K mất ~26 ms, một khung 1080p ~9,5 ms.
Khung nào hết thời gian thì camera bỏ (`FrameWasLate`, hàng đợi `captureOutput` còn bận).

| Chế độ | Độ trễ thấp (bản cũ) | Bộ mã hóa thường |
|---|---|---|
| 4K60 HEVC | 37,6 fps, bỏ 22,4/s, 26 ms | **60 fps**, bỏ 0, 35–39 ms |
| 4K60 H.264 | ~39–43 fps, 25 ms | 50 fps, bỏ 9,6/s, **139 ms** (không kịp, hàng đợi dài ra) |
| 1080p120 HEVC | 108 fps, 9 ms | **120 fps**, 21 ms |
| 1080p240 HEVC | 96 fps | (chưa đo) |
| 4K30 HEVC | 30 fps, 24–26 ms | 30 fps, **77 ms** (tệ hơn) |
| 1080p60 / 1080p30 | 60 / 30, ~10 ms | (không cần) |

Không phải nguyên nhân: camera luôn giao đủ 60 (giao + bỏ = 60), nên HDR và low-light boost phía camera không cản (không đo
riêng; caps báo `lowLightBoost` false trên máy này); `maxFrameDelay:1` không đổi gì; nhiệt nominal suốt, kể cả 2 phút 4K60 HEVC.

## Đã sửa

- iPhone (`VideoEncoder.lowLatencyFits`): HEVC có fps > 60 hoặc số điểm ảnh/giây > 4K30 thì dùng bộ mã hóa thường. H.264 giữ
  độ trễ thấp ở mọi chế độ (bộ mã hóa thường của H.264 trễ 139 ms ở 4K60). `state.lowLatency` cho biết chế độ đang dùng.
- PC (`phone_formats` trong `rust/engine/src/api.rs`, `PhoneFormats` trong `src/engine/EngineApi.cpp`): với iPhone, đúng các
  chế độ đó xin HEVC (khi Windows có bộ giải mã HEVC). **Viết trên Mac, chưa biên dịch.**
- Kiểm lại sau khi sửa: 4K60 HEVC phía iPhone 60, máy nhận 58,5; H.264 4K60 vẫn ~40 với `lowLatency=1` (y như bản cũ);
  4K30, 1080p60, 1080p30 không đổi. CryptoTests đạt.

## Còn mở

- **Wi‑Fi ở 4K chưa đo được đúng:** từ máy Mac, đường iPhone → router → Mac chỉ chở ~15–18 Mbit/s (khung đến trễ 100–400 ms
  ngay cả khi không có khung khóa) và có lúc đứng hẳn ~7 s; iPhone giảm bitrate đúng như thiết kế (tới 1 Mbps). Không phải do
  khung khóa: khung khóa 4K HEVC chỉ 52–156 KB, khung thường ~24 KB (`<lens>.frames.csv` của fake-pc). Mac: 5 GHz, 300 Mbit/s,
  `awdl0` (AirDrop) bật. Phải đo trên PC thật (HANDOFF 0.3).
- Các iPhone khác chưa đo: ngưỡng lấy từ iPhone 14 (A15).
- Cách đo lại: `scripts/fake-pc.sh --host <ip iPhone> --lens back-wide --width 3840 --height 2160 --fps 60 --codec hevc
  --bitrate 15000 --seconds 15 [--tune '{"lowLatency":true}']`, đọc dòng `phone side:`. IP lấy bằng
  `dns-sd -B _camstudio._tcp`, rồi `dns-sd -L iPhone _camstudio._tcp` và `dns-sd -G v4 <host>`.
