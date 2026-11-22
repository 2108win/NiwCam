// C++ side of the shared-memory cross-check. Built by run.ps1 as cpp_shm.dll from a *patched copy*
// of src/common/SharedFrame.h (the four kernel-object names become CsName(n), read from the
// suffix set by cpp_set_suffix) plus the unmodified FrameWriter.h and FrameReader.{h,cpp}.
// The Rust test (tests/interop.rs) loads this DLL and drives the real C++ FrameWriter and
// FrameReader through the flat C functions below, against the Rust ones on the same names.
#include "vcam/FrameReader.h"
#include "engine/FrameWriter.h"

#include <cstddef>
#include <cstdio>
#include <string>

static std::wstring g_names[4];

extern "C" const wchar_t* CsName(int n) { return g_names[n].c_str(); }

#define API extern "C" __declspec(dllexport)

// Names: Local\NiwCamTest_<suffix>_{G,L}_{Frames,NewFrame}
API void cpp_set_suffix(const wchar_t* suffix)
{
    const std::wstring s = suffix;
    g_names[0] = L"Local\\NiwCamTest_" + s + L"_G_Frames";
    g_names[1] = L"Local\\NiwCamTest_" + s + L"_G_NewFrame";
    g_names[2] = L"Local\\NiwCamTest_" + s + L"_L_Frames";
    g_names[3] = L"Local\\NiwCamTest_" + s + L"_L_NewFrame";
}

// sizeof and offsetof of SharedFrameHeader, in the order of layout_lines() in Rust.
API int cpp_layout(char* out, int cap)
{
#define OFF(f) std::snprintf(line, sizeof line, "offsetof " #f " %zu\n", offsetof(SharedFrameHeader, f)); text += line;
    char line[128];
    std::string text;
    std::snprintf(line, sizeof line, "sizeof SharedFrameHeader %zu\n", sizeof(SharedFrameHeader));
    text += line;
    OFF(magic) OFF(version) OFF(width) OFF(height) OFF(stride) OFF(slots) OFF(frameBytes) OFF(latest)
    OFF(writeTime100ns) OFF(readerActive) OFF(reserved1) OFF(slotSeq) OFF(slotReaders) OFF(reserved2) OFF(pad)
    if (static_cast<int>(text.size()) + 1 > cap) return -1;
    std::memcpy(out, text.c_str(), text.size() + 1);
    return static_cast<int>(text.size());
}

// Constants, as "name value" lines.
API int cpp_constants(char* out, int cap)
{
    char buf[1024];
    std::snprintf(buf, sizeof buf,
        "MAGIC %u\nVERSION %u\nSLOTS %u\nSTALE %lld\nLOCAL_MAX_PIXELS %u\nLOCAL_SLOT_BYTES %llu\nLOCAL_MAPPING_BYTES %llu\n"
        "MAPPING_BYTES_1920x1080 %llu\nNV12_BYTES_1921x1081 %llu\n",
        CAMSTUDIO_MAGIC, CAMSTUDIO_VERSION, CAMSTUDIO_SLOTS, static_cast<long long>(CAMSTUDIO_STALE_100NS),
        CAMSTUDIO_LOCAL_MAX_PIXELS, static_cast<unsigned long long>(CamStudioLocalSlotBytes()),
        static_cast<unsigned long long>(CamStudioLocalMappingBytes()),
        static_cast<unsigned long long>(CamStudioMappingBytes(1920, 1080)),
        static_cast<unsigned long long>(CamStudioNv12Bytes(1921, 1081)));
    const int n = static_cast<int>(std::strlen(buf));
    if (n + 1 > cap) return -1;
    std::memcpy(out, buf, n + 1);
    return n;
}

API void cpp_clamp(uint32_t* w, uint32_t* h) { CamStudioClampLocalSize(*w, *h); }

// ---- writer ----
API void* cw_new() { return new FrameWriter(); }
API void cw_free(void* p) { delete static_cast<FrameWriter*>(p); }
API void cw_use_local(void* p, int local) { static_cast<FrameWriter*>(p)->UseLocal(local != 0); }
API void cw_try_open(void* p) { static_cast<FrameWriter*>(p)->TryOpen(); }
API int cw_is_open(void* p) { return static_cast<FrameWriter*>(p)->IsOpen(); }
API int cw_is_local(void* p) { return static_cast<FrameWriter*>(p)->IsLocal(); }
API int cw_consumer_active(void* p) { return static_cast<FrameWriter*>(p)->ConsumerActive(); }
API uint32_t cw_width(void* p) { return static_cast<FrameWriter*>(p)->Width(); }
API uint32_t cw_height(void* p) { return static_cast<FrameWriter*>(p)->Height(); }
API int32_t cw_open_local(void* p, uint32_t w, uint32_t h, uint32_t* ow, uint32_t* oh)
{
    return static_cast<FrameWriter*>(p)->OpenLocal(w, h, *ow, *oh);
}
API void cw_close(void* p) { static_cast<FrameWriter*>(p)->Close(); }
API void cw_write(void* p, const uint8_t* src, int pitch, uint32_t w, uint32_t h)
{
    static_cast<FrameWriter*>(p)->Write(src, pitch, w, h);
}
// Returns the byte count (and w/h), or -1.
API int cw_copy_latest(void* p, uint8_t* out, int cap, uint32_t* w, uint32_t* h, int fresh)
{
    std::vector<uint8_t> v;
    auto* wr = static_cast<FrameWriter*>(p);
    const bool ok = fresh ? wr->CopyLatestFresh(v, *w, *h) : wr->CopyLatest(v, *w, *h);
    if (!ok || static_cast<int>(v.size()) > cap) return -1;
    std::memcpy(out, v.data(), v.size());
    return static_cast<int>(v.size());
}
API void cw_hold_for(void* p, uint32_t ms) { static_cast<FrameWriter*>(p)->HoldFor(ms); }
API void cw_release_hold(void* p) { static_cast<FrameWriter*>(p)->ReleaseHold(); }

// ---- reader ----
API void* cr_new() { return new FrameReader(); }
API void cr_free(void* p) { delete static_cast<FrameReader*>(p); }
API int32_t cr_open(void* p, uint32_t w, uint32_t h, int local) { return static_cast<FrameReader*>(p)->Open(w, h, local != 0); }
API int cr_open_existing(void* p, int local) { return static_cast<FrameReader*>(p)->OpenExisting(local != 0); }
API uint32_t cr_width(void* p) { return static_cast<FrameReader*>(p)->Width(); }
API uint32_t cr_height(void* p) { return static_cast<FrameReader*>(p)->Height(); }
API void cr_close(void* p) { static_cast<FrameReader*>(p)->Close(); }
API int64_t cr_latest_frame(void* p) { return static_cast<FrameReader*>(p)->LatestFrame(); }
API int64_t cr_delivered_frame(void* p) { return static_cast<FrameReader*>(p)->DeliveredFrame(); }
API void cr_set_active(void* p, int a) { static_cast<FrameReader*>(p)->SetActive(a != 0); }
API int cr_fill(void* p, uint8_t* dst, int pitch, uint32_t w, uint32_t h, int64_t now)
{
    return static_cast<FrameReader*>(p)->Fill(dst, pitch, w, h, now);
}
