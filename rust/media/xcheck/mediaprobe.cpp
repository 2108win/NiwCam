// Container probe, run on the files of BOTH implementations.
//   mediaprobe mp4 <file> <w> <h>   duration, codec, size, fps, bitrate, every frame decoded to NV12 and
//                                   matched against the generated picture (pattern.h): PSNR, index order
//   mediaprobe png <file>           size, pixel format, hash of the decoded BGRA pixels
#include "pattern.h"

#include <windows.h>
#include <mfapi.h>
#include <mferror.h>
#include <mfidl.h>
#include <mfreadwrite.h>
#include <wincodec.h>
#include <wrl/client.h>

#include <algorithm>
#include <cmath>
#include <cstdio>
#include <string>
#include <vector>

#pragma comment(lib, "mfplat.lib")
#pragma comment(lib, "mfuuid.lib")
#pragma comment(lib, "mf.lib")
#pragma comment(lib, "mfreadwrite.lib")
#pragma comment(lib, "ole32.lib")
#pragma comment(lib, "windowscodecs.lib")

using Microsoft::WRL::ComPtr;

static std::wstring W(const char* s) { return std::wstring(s, s + strlen(s)); }

static int ProbePng(const char* path)
{
    ComPtr<IWICImagingFactory> wic;
    if (FAILED(CoCreateInstance(CLSID_WICImagingFactory, nullptr, CLSCTX_INPROC_SERVER, IID_PPV_ARGS(&wic)))) return 1;
    ComPtr<IWICBitmapDecoder> dec;
    HRESULT hr = wic->CreateDecoderFromFilename(W(path).c_str(), nullptr, GENERIC_READ, WICDecodeMetadataCacheOnDemand, &dec);
    if (FAILED(hr)) { printf("png open failed %08x\n", (unsigned)hr); return 1; }
    ComPtr<IWICBitmapFrameDecode> frame;
    dec->GetFrame(0, &frame);
    UINT w = 0, h = 0;
    frame->GetSize(&w, &h);
    WICPixelFormatGUID pf{};
    frame->GetPixelFormat(&pf);
    ComPtr<IWICFormatConverter> conv;
    wic->CreateFormatConverter(&conv);
    conv->Initialize(frame.Get(), GUID_WICPixelFormat32bppBGRA, WICBitmapDitherTypeNone, nullptr, 0, WICBitmapPaletteTypeCustom);
    std::vector<uint8_t> px(static_cast<size_t>(w) * h * 4);
    conv->CopyPixels(nullptr, w * 4, static_cast<UINT>(px.size()), px.data());
    uint64_t hash = 1469598103934665603ull;
    for (uint8_t b : px) { hash ^= b; hash *= 1099511628211ull; }
    int nativeBgra = (pf == GUID_WICPixelFormat32bppBGRA) ? 1 : 0;
    uint8_t minA = 255;
    for (size_t i = 3; i < px.size(); i += 4) minA = std::min(minA, px[i]);
    printf("png %ux%u native-32bppBGRA=%d min-alpha=%u pixel-hash=%016llx\n", w, h, nativeBgra, minA, (unsigned long long)hash);
    return 0;
}

static std::string Sub(const GUID& g)
{
    if (g == MFVideoFormat_H264) return "H264";
    if (g == MFVideoFormat_HEVC) return "HEVC";
    if (g == MFVideoFormat_NV12) return "NV12";
    if (g == MFAudioFormat_AAC) return "AAC";
    char b[16];
    snprintf(b, sizeof b, "%.4s", reinterpret_cast<const char*>(&g.Data1));
    return b;
}

static int ProbeMp4(const char* path, uint32_t pw, uint32_t ph)
{
    ComPtr<IMFAttributes> attrs;
    MFCreateAttributes(&attrs, 1);
    attrs->SetUINT32(MF_SOURCE_READER_ENABLE_VIDEO_PROCESSING, TRUE);
    ComPtr<IMFSourceReader> reader;
    HRESULT hr = MFCreateSourceReaderFromURL(W(path).c_str(), attrs.Get(), &reader);
    if (FAILED(hr)) { printf("mp4 open failed %08x\n", (unsigned)hr); return 1; }
    PROPVARIANT dur;
    PropVariantInit(&dur);
    reader->GetPresentationAttribute(static_cast<DWORD>(MF_SOURCE_READER_MEDIASOURCE), MF_PD_DURATION, &dur);
    printf("duration-ms %lld\n", (long long)(dur.uhVal.QuadPart / 10000));
    int streams = 0;
    for (DWORD i = 0;; ++i)
    {
        ComPtr<IMFMediaType> t;
        if (FAILED(reader->GetNativeMediaType(i, 0, &t))) break;
        GUID major{}, sub{};
        t->GetGUID(MF_MT_MAJOR_TYPE, &major);
        t->GetGUID(MF_MT_SUBTYPE, &sub);
        ++streams;
        if (major == MFMediaType_Video)
        {
            UINT32 w = 0, h = 0, n = 0, d = 1;
            MFGetAttributeSize(t.Get(), MF_MT_FRAME_SIZE, &w, &h);
            MFGetAttributeRatio(t.Get(), MF_MT_FRAME_RATE, &n, &d);
            printf("video-stream %lu codec %s %ux%u fps %u/%u bitrate-set %u\n", i, Sub(sub).c_str(), w, h, n, d, MFGetAttributeUINT32(t.Get(), MF_MT_AVG_BITRATE, 0) ? 1u : 0u);
        }
        else if (major == MFMediaType_Audio) printf("audio-stream %lu codec %s\n", i, Sub(sub).c_str());
    }
    printf("streams %d\n", streams);

    ComPtr<IMFMediaType> want;
    MFCreateMediaType(&want);
    want->SetGUID(MF_MT_MAJOR_TYPE, MFMediaType_Video);
    want->SetGUID(MF_MT_SUBTYPE, MFVideoFormat_NV12);
    hr = reader->SetCurrentMediaType(static_cast<DWORD>(MF_SOURCE_READER_FIRST_VIDEO_STREAM), nullptr, want.Get());
    if (FAILED(hr)) { printf("cannot decode to NV12 %08x\n", (unsigned)hr); return 1; }
    ComPtr<IMFMediaType> cur;
    reader->GetCurrentMediaType(static_cast<DWORD>(MF_SOURCE_READER_FIRST_VIDEO_STREAM), &cur);
    UINT32 w = 0, h = 0;
    MFGetAttributeSize(cur.Get(), MF_MT_FRAME_SIZE, &w, &h);
    LONG stride = (LONG)MFGetAttributeUINT32(cur.Get(), MF_MT_DEFAULT_STRIDE, w);

    // The expected pictures (every pattern index the writer could have been given).
    std::vector<std::vector<uint8_t>> expect(110);
    for (uint32_t n = 0; n < expect.size(); ++n) DrawPattern(expect[n], pw, ph, n);

    uint32_t frames = 0, lastIdx = 0, backwards = 0;
    long long first = -1, last = 0, maxGap = 0, prev = -1;
    double minPsnr = 1e9;
    uint32_t okFrames = 0, barFrames = 0, otherFrames = 0;
    for (;;)
    {
        DWORD flags = 0;
        LONGLONG ts = 0;
        ComPtr<IMFSample> s;
        hr = reader->ReadSample(static_cast<DWORD>(MF_SOURCE_READER_FIRST_VIDEO_STREAM), 0, nullptr, &flags, &ts, &s);
        if (FAILED(hr) || (flags & (MF_SOURCE_READERF_ERROR | MF_SOURCE_READERF_ENDOFSTREAM))) break;
        if (!s) continue;
        ComPtr<IMFMediaBuffer> b;
        s->ConvertToContiguousBuffer(&b);
        BYTE* p = nullptr;
        DWORD len = 0;
        b->Lock(&p, nullptr, &len);
        ++frames;
        if (first < 0) first = ts;
        last = ts;
        if (prev >= 0) maxGap = std::max<long long>(maxGap, ts - prev);
        prev = ts;
        if (w == pw && h == ph)
        {
            // Best match among the expected pictures (luma, every 3rd row/column), by sum of squared error.
            double best = 1e30;
            uint32_t bestIdx = 0;
            for (uint32_t n = 0; n < expect.size(); ++n)
            {
                double sse = 0;
                for (uint32_t r = 0; r < h; r += 3)
                    for (uint32_t c = 0; c < w; c += 3)
                    {
                        const int d = int(p[(size_t)r * stride + c]) - int(expect[n][(size_t)r * w + c]);
                        sse += d * d;
                    }
                if (sse < best) { best = sse; bestIdx = n; }
            }
            const double mse = best / (double((h + 2) / 3) * double((w + 2) / 3));
            const double psnr = mse > 0 ? 10 * log10(255.0 * 255.0 / mse) : 99;
            if (psnr >= 25)
            {
                ++okFrames;
                minPsnr = std::min(minPsnr, psnr);
                if (bestIdx < lastIdx) ++backwards;
                lastIdx = bestIdx;
            }
            else
            {
                // The 640x480 pushes: fitted into 1280x720, black bars (luma 16) left and right, picture between.
                const bool bars = p[(size_t)(h / 2) * stride + 20] <= 20 && p[(size_t)(h / 2) * stride + w - 20] <= 20 && p[(size_t)(h / 2) * stride + w / 2] > 30;
                if (bars) ++barFrames; else ++otherFrames;
            }
        }
        b->Unlock();
    }
    printf("decoded-size %ux%u\n", w, h);
    printf("frames %u first-ms %lld last-ms %lld max-gap-ms %lld\n", frames, first / 10000, last / 10000, maxGap / 10000);
    printf("frames-matching-pattern(psnr>=25dB) %u, fitted-with-bars %u, other %u, of %u; min-psnr %.1f dB, pattern-index-went-backwards %u\n", okFrames, barFrames, otherFrames, frames, minPsnr, backwards);
    return 0;
}

int main(int argc, char** argv)
{
    if (argc < 3) return 2;
    CoInitializeEx(nullptr, COINIT_MULTITHREADED);
    MFStartup(MF_VERSION);
    const std::string mode = argv[1];
    if (mode == "png") return ProbePng(argv[2]);
    if (mode == "mp4" && argc >= 5) return ProbeMp4(argv[2], atoi(argv[3]), atoi(argv[4]));
    return 2;
}
