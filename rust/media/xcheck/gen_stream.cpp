// Makes an Annex-B H.264 or HEVC test stream with the Windows encoder MFT (no phone link needed).
// gen_stream <h264|hevc> <width> <height> <frames> <out.bin>
// File: repeated { u32 size; i64 time100ns; u8 key; u8 data[size] }.
#include <windows.h>
#include <mfapi.h>
#include <mferror.h>
#include <mfidl.h>
#include <mftransform.h>
#include <strmif.h>
#include <initguid.h>
#include <codecapi.h>
#include <wrl/client.h>

#include <cstdint>
#include <cstdio>
#include <cstring>
#include <string>
#include <vector>

#pragma comment(lib, "mfplat.lib")
#pragma comment(lib, "mfuuid.lib")
#pragma comment(lib, "ole32.lib")

using Microsoft::WRL::ComPtr;

static void Draw(std::vector<uint8_t>& nv12, uint32_t w, uint32_t h, uint32_t n)
{
    uint8_t* y = nv12.data();
    uint8_t* uv = y + w * h;
    for (uint32_t r = 0; r < h; ++r)
        for (uint32_t c = 0; c < w; ++c)
            y[r * w + c] = static_cast<uint8_t>(40 + (c * 160 / w) + ((r * 7 + n * 3) % 23));
    for (uint32_t r = 0; r < h / 2; ++r)
        for (uint32_t c = 0; c < w / 2; ++c)
        {
            uv[r * w + 2 * c] = static_cast<uint8_t>(64 + c * 128 / (w / 2));
            uv[r * w + 2 * c + 1] = static_cast<uint8_t>(64 + r * 128 / (h / 2));
        }
    const uint32_t bx = (n * 12) % (w - 160);
    for (uint32_t r = h / 3; r < h / 3 + 120 && r < h; ++r) memset(y + r * w + bx, 235, 160);
}

int main(int argc, char** argv)
{
    if (argc < 6) { printf("usage: gen_stream <h264|hevc> <w> <h> <frames> <out>\n"); return 2; }
    const bool hevc = std::string(argv[1]) == "hevc";
    const uint32_t w = atoi(argv[2]), h = atoi(argv[3]), frames = atoi(argv[4]);
    const uint32_t fps = 30;
    CoInitializeEx(nullptr, COINIT_MULTITHREADED);
    MFStartup(MF_VERSION);

    MFT_REGISTER_TYPE_INFO in{ MFMediaType_Video, MFVideoFormat_NV12 }, out{ MFMediaType_Video, hevc ? MFVideoFormat_HEVC : MFVideoFormat_H264 };
    IMFActivate** acts = nullptr;
    UINT32 count = 0;
    MFTEnumEx(MFT_CATEGORY_VIDEO_ENCODER, MFT_ENUM_FLAG_SYNCMFT | MFT_ENUM_FLAG_SORTANDFILTER, &in, &out, &acts, &count);
    if (!count) { printf("NO-SYNC-ENCODER\n"); return 3; }
    ComPtr<IMFTransform> enc;
    HRESULT hr = acts[0]->ActivateObject(IID_PPV_ARGS(&enc));
    WCHAR* nm = nullptr; UINT32 nl = 0;
    if (SUCCEEDED(acts[0]->GetAllocatedString(MFT_FRIENDLY_NAME_Attribute, &nm, &nl))) { wprintf(L"encoder: %s\n", nm); CoTaskMemFree(nm); }
    for (UINT32 i = 0; i < count; ++i) acts[i]->Release();
    CoTaskMemFree(acts);
    if (FAILED(hr)) { printf("activate failed %08lx\n", hr); return 3; }

    ComPtr<IMFMediaType> ot, it;
    MFCreateMediaType(&ot);
    ot->SetGUID(MF_MT_MAJOR_TYPE, MFMediaType_Video);
    ot->SetGUID(MF_MT_SUBTYPE, out.guidSubtype);
    ot->SetUINT32(MF_MT_AVG_BITRATE, 6000000);
    ot->SetUINT32(MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive);
    MFSetAttributeSize(ot.Get(), MF_MT_FRAME_SIZE, w, h);
    MFSetAttributeRatio(ot.Get(), MF_MT_FRAME_RATE, fps, 1);
    if (!hevc) ot->SetUINT32(MF_MT_MPEG2_PROFILE, eAVEncH264VProfile_Main);
    hr = enc->SetOutputType(0, ot.Get(), 0);
    MFCreateMediaType(&it);
    it->SetGUID(MF_MT_MAJOR_TYPE, MFMediaType_Video);
    it->SetGUID(MF_MT_SUBTYPE, MFVideoFormat_NV12);
    it->SetUINT32(MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive);
    MFSetAttributeSize(it.Get(), MF_MT_FRAME_SIZE, w, h);
    MFSetAttributeRatio(it.Get(), MF_MT_FRAME_RATE, fps, 1);
    if (SUCCEEDED(hr)) hr = enc->SetInputType(0, it.Get(), 0);
    if (FAILED(hr)) { printf("encoder setup failed 0x%08lx\n", hr); return 3; }
    ComPtr<ICodecAPI> api;
    enc.As(&api);
    if (api)
    {
        VARIANT v{};
        v.vt = VT_UI4; v.ulVal = 10; // a keyframe every 10 frames
        api->SetValue(&CODECAPI_AVEncMPVGOPSize, &v);
        v.vt = VT_BOOL; v.boolVal = VARIANT_TRUE;
        api->SetValue(&CODECAPI_AVLowLatencyMode, &v);
    }
    enc->ProcessMessage(MFT_MESSAGE_NOTIFY_BEGIN_STREAMING, 0);
    enc->ProcessMessage(MFT_MESSAGE_NOTIFY_START_OF_STREAM, 0);

    MFT_OUTPUT_STREAM_INFO info{};
    enc->GetOutputStreamInfo(0, &info);
    FILE* f = nullptr;
    fopen_s(&f, argv[5], "wb");
    std::vector<uint8_t> nv12(static_cast<size_t>(w) * h * 3 / 2);
    const int64_t frame100ns = 10000000 / fps;
    uint32_t written = 0;
    auto pull = [&]() {
        for (;;)
        {
            ComPtr<IMFSample> os;
            MFT_OUTPUT_DATA_BUFFER od{};
            const bool provides = (info.dwFlags & (MFT_OUTPUT_STREAM_PROVIDES_SAMPLES | MFT_OUTPUT_STREAM_CAN_PROVIDE_SAMPLES)) != 0;
            if (!provides)
            {
                ComPtr<IMFMediaBuffer> ob;
                MFCreateMemoryBuffer(info.cbSize ? info.cbSize : 4 * 1024 * 1024, &ob);
                MFCreateSample(&os);
                os->AddBuffer(ob.Get());
                od.pSample = os.Get();
            }
            DWORD status = 0;
            HRESULT r = enc->ProcessOutput(0, 1, &od, &status);
            if (provides && od.pSample) os.Attach(od.pSample);
            if (od.pEvents) od.pEvents->Release();
            if (r == MF_E_TRANSFORM_STREAM_CHANGE) continue;
            if (FAILED(r) || !os) return;
            ComPtr<IMFMediaBuffer> cb;
            os->ConvertToContiguousBuffer(&cb);
            BYTE* d = nullptr; DWORD len = 0;
            cb->Lock(&d, nullptr, &len);
            const uint8_t key = MFGetAttributeUINT32(os.Get(), MFSampleExtension_CleanPoint, 0) ? 1 : 0;
            LONGLONG t = 0;
            os->GetSampleTime(&t);
            fwrite(&len, 4, 1, f); fwrite(&t, 8, 1, f); fwrite(&key, 1, 1, f); fwrite(d, 1, len, f);
            ++written;
            cb->Unlock();
        }
    };
    for (uint32_t n = 0; n < frames; ++n)
    {
        Draw(nv12, w, h, n);
        ComPtr<IMFMediaBuffer> buf;
        MFCreateMemoryBuffer(static_cast<DWORD>(nv12.size()), &buf);
        BYTE* p = nullptr;
        buf->Lock(&p, nullptr, nullptr);
        memcpy(p, nv12.data(), nv12.size());
        buf->Unlock();
        buf->SetCurrentLength(static_cast<DWORD>(nv12.size()));
        ComPtr<IMFSample> s;
        MFCreateSample(&s);
        s->AddBuffer(buf.Get());
        s->SetSampleTime(n * frame100ns);
        s->SetSampleDuration(frame100ns);
        hr = enc->ProcessInput(0, s.Get(), 0);
        if (FAILED(hr)) { printf("ProcessInput failed %08lx at %u\n", hr, n); break; }
        pull();
    }
    enc->ProcessMessage(MFT_MESSAGE_COMMAND_DRAIN, 0);
    pull();
    fclose(f);
    printf("wrote %u access units to %s\n", written, argv[5]);
    return written ? 0 : 4;
}
