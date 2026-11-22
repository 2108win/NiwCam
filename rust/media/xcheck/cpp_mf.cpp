// The C++ MfDevices.h on this PC's webcams. Prints names, symbolic links, every native format in the
// camera's own order, PickLargest, the best-first option list and a few SelectCamera calls.
// The Rust example `devices` prints the same lines. Cameras are only enumerated and their format
// list read (no frames are ever requested); our own virtual camera's source is not even opened.
#include "../../../src/common/MfDevices.h"
#include <algorithm>
#include <cmath>
#include <cstdio>

#pragma comment(lib, "mfplat.lib")
#pragma comment(lib, "mfuuid.lib")
#pragma comment(lib, "mf.lib")
#pragma comment(lib, "ole32.lib")

using Microsoft::WRL::ComPtr;

static std::string U8(const std::wstring& w)
{
    if (w.empty()) return {};
    const int n = WideCharToMultiByte(CP_UTF8, 0, w.c_str(), (int)w.size(), nullptr, 0, nullptr, nullptr);
    std::string s(n, '\0');
    WideCharToMultiByte(CP_UTF8, 0, w.c_str(), (int)w.size(), s.data(), n, nullptr, nullptr);
    return s;
}

// Verbatim from src/engine/CaptureEngine.cpp (that file cannot be included here).
struct FormatOption
{
    NativeFormat native;
    bool largest = false;
    std::wstring lensLabel;
};
static std::vector<FormatOption> BuildFormatOptions(const std::vector<NativeFormat>& formats)
{
    std::vector<FormatOption> options;
    for (const auto& f : formats)
    {
        if (!f.width || !f.height || !f.fpsNum) continue;
        auto same = std::find_if(options.begin(), options.end(), [&](const FormatOption& o) {
            return o.native.width == f.width && o.native.height == f.height &&
                   std::fabs(o.native.Fps() - f.Fps()) < 0.01;
        });
        if (same == options.end()) options.push_back({ f, false, {} });
        else if (SubtypeRank(f.subtype) > SubtypeRank(same->native.subtype)) same->native = f;
    }
    std::sort(options.begin(), options.end(), [](const FormatOption& a, const FormatOption& b) {
        if (a.native.Pixels() != b.native.Pixels()) return a.native.Pixels() > b.native.Pixels();
        return a.native.Fps() > b.native.Fps();
    });
    if (!options.empty()) options.front().largest = true;
    return options;
}

static void PrintFormat(const char* tag, const NativeFormat& f)
{
    printf("%s idx=%lu %s %ux%u %u/%u fps=%.6f pixels=%llu rank=%d\n", tag, f.index, U8(SubtypeName(f.subtype)).c_str(), f.width, f.height,
           f.fpsNum, f.fpsDen, f.Fps(), (unsigned long long)f.Pixels(), SubtypeRank(f.subtype));
}

int main()
{
    CoInitializeEx(nullptr, COINIT_MULTITHREADED);
    MFStartup(MF_VERSION);
    auto cams = EnumerateCameras();
    printf("cameras %zu\n", cams.size());
    for (size_t i = 0; i < cams.size(); ++i)
    {
        const auto& c = cams[i];
        printf("camera %zu name=[%s] link=[%s] ours=%d phone=%d window=%d\n", i, U8(c.name).c_str(), U8(c.symbolicLink).c_str(), IsOurVirtualCamera(c) ? 1 : 0,
               c.IsPhone() ? 1 : 0, c.IsPcWindow() ? 1 : 0);
        if (IsOurVirtualCamera(c)) { printf("  formats skipped (our own virtual camera)\n"); continue; }
        ComPtr<IMFMediaSource> source;
        HRESULT hr = c.activate->ActivateObject(IID_PPV_ARGS(&source));
        if (FAILED(hr)) { printf("  activate %08x\n", (unsigned)hr); continue; }
        std::vector<NativeFormat> formats;
        hr = GetNativeFormats(source.Get(), formats);
        source->Shutdown();
        c.activate->ShutdownObject();
        printf("  GetNativeFormats %08x count %zu\n", (unsigned)hr, formats.size());
        for (const auto& f : formats) PrintFormat("  fmt", f);
        if (const NativeFormat* best = PickLargest(formats)) PrintFormat("  largest", *best);
        else printf("  largest none\n");
        for (const auto& o : BuildFormatOptions(formats)) PrintFormat(o.largest ? "  option*" : "  option ", o.native);
    }
    for (const wchar_t* sel : { L"", L"0", L"1", L"99", L"obsbot", L"OBSBOT", L"cam", L"niwcam", L"zzz-none", L"Windows Virtual" })
    {
        const CameraDevice* c = SelectCamera(cams, sel);
        printf("select [%s] -> %s\n", U8(sel).c_str(), c ? ("[" + U8(c->name) + "]").c_str() : "none");
    }
    for (size_t i = 0; i < cams.size(); ++i)
    {
        CameraDevice found;
        const bool ok = FindCameraAgain(cams[i], found);
        printf("find-again %zu -> %s [%s] link=[%s]\n", i, ok ? "found" : "none", U8(found.name).c_str(), U8(found.symbolicLink).c_str());
    }
    CameraDevice ghost;
    ghost.name = L"No Such Camera";
    ghost.symbolicLink = L"\\\\?\\usb#vid_0000";
    CameraDevice found;
    printf("find-again ghost -> %s\n", FindCameraAgain(ghost, found) ? "found" : "none");
    // by name only (symbolic link differs): first non-virtual camera's name
    for (const auto& c : cams)
        if (!IsOurVirtualCamera(c))
        {
            CameraDevice moved;
            moved.name = c.name;
            moved.symbolicLink = L"other-port";
            CameraDevice f2;
            printf("find-again by-name [%s] -> %s\n", U8(c.name).c_str(), FindCameraAgain(moved, f2) ? U8(f2.name).c_str() : "none");
            break;
        }
    // IsOurVirtualCamera spellings
    for (const wchar_t* n : { L"NiwCam", L"NiwCam (Windows Virtual Camera)", L"Cam Studio", L"Cam Studio (x)", L"NiwCam2", L"niwcam", L"Cam Studio  (", L"" })
    {
        CameraDevice d;
        d.name = n;
        printf("ours [%s] -> %d\n", U8(n).c_str(), IsOurVirtualCamera(d) ? 1 : 0);
    }
    // ToLower (towlower in the engine's C locale): ASCII, Vietnamese, Latin-1, Cyrillic, Turkish dotted I
    for (wchar_t ch : { L'A', L'Z', L'Đ', L'Á', L'Ж', L'İ', L'Ạ', L'Ð' })
        printf("tolower %04x -> %04x\n", (unsigned)ch, (unsigned)ToLower(std::wstring(1, ch))[0]);
    // SubtypeName
    GUID odd = MFVideoFormat_NV12;
    odd.Data1 = 0x41424344; // "DCBA" little endian bytes 44 43 42 41
    printf("subtype odd [%s]\n", U8(SubtypeName(odd)).c_str());
    odd.Data1 = 0x00004142;
    printf("subtype odd2 [%s]\n", U8(SubtypeName(odd)).c_str());
    for (const GUID* g : { &MFVideoFormat_NV12, &MFVideoFormat_YUY2, &MFVideoFormat_MJPG, &MFVideoFormat_RGB24, &MFVideoFormat_RGB32, &MFVideoFormat_H264, &MFVideoFormat_HEVC, &MFVideoFormat_I420, &MFVideoFormat_L8, &MFVideoFormat_UYVY })
        printf("subtype [%s] rank %d\n", U8(SubtypeName(*g)).c_str(), SubtypeRank(*g));
    // PickLargest on a synthetic list (ties)
    std::vector<NativeFormat> syn;
    auto add = [&](GUID sub, UINT32 w, UINT32 h, UINT32 n, UINT32 d) { NativeFormat f; f.index = (DWORD)syn.size(); f.subtype = sub; f.width = w; f.height = h; f.fpsNum = n; f.fpsDen = d; syn.push_back(f); };
    add(MFVideoFormat_MJPG, 1280, 720, 30, 1);
    add(MFVideoFormat_YUY2, 1280, 720, 30, 1);
    add(MFVideoFormat_NV12, 1280, 720, 30, 1);
    add(MFVideoFormat_H264, 1280, 720, 30, 1);
    add(MFVideoFormat_H264, 1920, 1080, 30, 1);
    add(MFVideoFormat_YUY2, 1920, 1080, 30, 1);
    add(MFVideoFormat_MJPG, 1920, 1080, 60, 1);
    add(MFVideoFormat_MJPG, 1920, 1080, 30000, 1001);
    add(MFVideoFormat_YUY2, 0, 0, 90, 1);
    add(MFVideoFormat_NV12, 640, 480, 0, 1);
    for (const auto& o : BuildFormatOptions(syn)) PrintFormat(o.largest ? "syn option*" : "syn option ", o.native);
    if (const NativeFormat* b = PickLargest(syn)) PrintFormat("syn largest", *b);
    return 0;
}
