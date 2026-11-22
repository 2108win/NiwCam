// In-process test driver for the DirectShow source filter (C++ CamStudioDShow.dll and the Rust one).
//
//   dshow_driver <dll> <mappingName> <eventName> [fast]
//
// Loads the DLL by path (nothing is registered anywhere), takes the filter from its class factory, builds
// graphs by hand (filter -> SampleGrabber -> NullRenderer, like tools/dshow_consumer.cpp --grab) and feeds
// the filter from the REAL host writer (src/engine/FrameWriter.h) on a private channel: the object names
// are given on the command line (the C++ DLL is a patched scratch copy, the Rust DLL a test-names build).
//
// Every line is "D <key> = <value>" (deterministic: identical for the C++ and the Rust DLL) or
// "T <key> = <number>" (timing / statistics: compared with a tolerance by run.ps1).
#define NOMINMAX
#include <mfapi.h>
#include <mfidl.h>
#include "../../../src/common/SharedFrame.h"

#include <string>
static std::wstring g_mapName, g_evtName;
#undef CAMSTUDIO_LOCAL_MAPPING_NAME
#undef CAMSTUDIO_LOCAL_EVENT_NAME
#define CAMSTUDIO_LOCAL_MAPPING_NAME g_mapName.c_str()
#define CAMSTUDIO_LOCAL_EVENT_NAME g_evtName.c_str()
#include "../../../src/engine/FrameWriter.h"

#include <dshow.h>
#include <dvdmedia.h>
#include <wrl/client.h>

#include <atomic>
#include <cmath>
#include <cstdio>
#include <map>
#include <mutex>
#include <set>
#include <thread>
#include <vector>

#pragma comment(lib, "strmiids")
#pragma comment(lib, "ole32")
#pragma comment(lib, "oleaut32")
#pragma comment(lib, "mfplat")

using Microsoft::WRL::ComPtr;

// ---- small helpers ---------------------------------------------------------------------------

static std::string Hr(HRESULT hr)
{
    char b[16];
    snprintf(b, sizeof b, "0x%08X", static_cast<unsigned>(hr));
    return b;
}
static void D(const std::string& key, const std::string& value)
{
    printf("D %s = %s\n", key.c_str(), value.c_str());
    fflush(stdout);
}
static void T(const std::string& key, double value)
{
    printf("T %s = %.3f\n", key.c_str(), value);
    fflush(stdout);
}
static std::string Fmt(const char* f, ...)
{
    char b[512];
    va_list a;
    va_start(a, f);
    vsnprintf(b, sizeof b, f, a);
    va_end(a);
    return b;
}
static std::string Fourcc(uint32_t c)
{
    char b[5] = { static_cast<char>(c), static_cast<char>(c >> 8), static_cast<char>(c >> 16), static_cast<char>(c >> 24), 0 };
    return b;
}
static LONGLONG Qpc100ns()
{
    LARGE_INTEGER f, c;
    QueryPerformanceFrequency(&f);
    QueryPerformanceCounter(&c);
    return c.QuadPart * 10000000LL / f.QuadPart;
}
static uint64_t HashBytes(const uint8_t* p, size_t n)
{
    uint64_t h = 0xcbf29ce484222325ULL;
    size_t i = 0;
    for (; i + 8 <= n; i += 8)
    {
        uint64_t w;
        memcpy(&w, p + i, 8);
        h = (h ^ w) * 0x100000001b3ULL;
        h ^= h >> 29;
    }
    for (; i < n; ++i) h = (h ^ p[i]) * 0x100000001b3ULL;
    return h;
}
// CPU time of this whole process (the driver's hashing and the host writer are the same for both DLLs).
static double CpuMs()
{
    FILETIME c, e, k, u;
    GetProcessTimes(GetCurrentProcess(), &c, &e, &k, &u);
    auto v = [](const FILETIME& f) { return ((static_cast<unsigned long long>(f.dwHighDateTime) << 32) | f.dwLowDateTime) / 10000.0; };
    return v(k) + v(u);
}
static std::string Hex64(uint64_t h)
{
    char b[24];
    snprintf(b, sizeof b, "%016llx", static_cast<unsigned long long>(h));
    return b;
}

static const GUID kSubtypeNV12 = { 0x3231564E, 0x0000, 0x0010, { 0x80, 0x00, 0x00, 0xAA, 0x00, 0x38, 0x9B, 0x71 } };
static GUID FourccSub(uint32_t c) { return { c, 0x0000, 0x0010, { 0x80, 0x00, 0x00, 0xAA, 0x00, 0x38, 0x9B, 0x71 } }; }
constexpr uint32_t kNV12 = 0x3231564E, kYUY2 = 0x32595559, kI420 = 0x30323449;

// SampleGrabber (qedit.h is not in the SDK any more; the filter still ships with Windows).
static const GUID CLSID_SampleGrabberQ = { 0xC1F400A0, 0x3F08, 0x11d3, { 0x9F, 0x0B, 0x00, 0x60, 0x08, 0x03, 0x9E, 0x37 } };
static const GUID CLSID_NullRendererQ = { 0xC1F400A4, 0x3F08, 0x11d3, { 0x9F, 0x0B, 0x00, 0x60, 0x08, 0x03, 0x9E, 0x37 } };
MIDL_INTERFACE("0579154A-2B53-4994-B0D0-E773148EFF85")
ISampleGrabberCBQ : public IUnknown
{
    virtual HRESULT STDMETHODCALLTYPE SampleCB(double, IMediaSample*) = 0;
    virtual HRESULT STDMETHODCALLTYPE BufferCB(double, BYTE*, long) = 0;
};
MIDL_INTERFACE("6B652FFF-11FE-4fce-92AD-0266B5D7C78F")
ISampleGrabberQ : public IUnknown
{
    virtual HRESULT STDMETHODCALLTYPE SetOneShot(BOOL) = 0;
    virtual HRESULT STDMETHODCALLTYPE SetMediaType(const AM_MEDIA_TYPE*) = 0;
    virtual HRESULT STDMETHODCALLTYPE GetConnectedMediaType(AM_MEDIA_TYPE*) = 0;
    virtual HRESULT STDMETHODCALLTYPE SetBufferSamples(BOOL) = 0;
    virtual HRESULT STDMETHODCALLTYPE GetCurrentBuffer(long*, long*) = 0;
    virtual HRESULT STDMETHODCALLTYPE GetCurrentSample(IMediaSample**) = 0;
    virtual HRESULT STDMETHODCALLTYPE SetCallback(ISampleGrabberCBQ*, long) = 0;
};
MIDL_INTERFACE("31EFAC30-515C-11d0-A9AA-00AA0061BE93")
IKsPropertySetQ : public IUnknown
{
    virtual HRESULT STDMETHODCALLTYPE Set(REFGUID, DWORD, LPVOID, DWORD, LPVOID, DWORD) = 0;
    virtual HRESULT STDMETHODCALLTYPE Get(REFGUID, DWORD, LPVOID, DWORD, LPVOID, DWORD, DWORD*) = 0;
    virtual HRESULT STDMETHODCALLTYPE QuerySupported(REFGUID, DWORD, DWORD*) = 0;
};
static const GUID kAmPropSetPin = { 0x9b00f101, 0x1567, 0x11d1, { 0xb3, 0xf1, 0x00, 0xaa, 0x00, 0x37, 0x61, 0xc5 } };
static const GUID kPinCategoryCapture = { 0xfb6c4281, 0x0353, 0x11d1, { 0x90, 0x5f, 0x00, 0x00, 0xc0, 0xcc, 0x16, 0xba } };
static const GUID CLSID_CamStudioDShowQ = { 0x7d2b9c41, 0x3a6e, 0x4f58, { 0xb0, 0xd1, 0x9e, 0x4c, 0x2f, 0x6a, 0x8b, 0x13 } };
// IMarshal / IAgileObject: a WRL ClassicCom object answers neither.
static const GUID kIidMarshal = { 0x00000003, 0x0000, 0x0000, { 0xC0, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x46 } };
static const GUID kIidAgile = { 0x94ea2b94, 0xe9cc, 0x49e0, { 0xc0, 0xff, 0xee, 0x64, 0xca, 0x8f, 0x5b, 0x90 } };
static const GUID kIidAMFilterMisc = { 0x2dd74950, 0xa890, 0x11d1, { 0xab, 0xe8, 0x00, 0xa0, 0xc9, 0x05, 0xf3, 0x75 } };
static const GUID kIidAMStreamControl = { 0x36b73881, 0xc2c8, 0x11cf, { 0x8b, 0x46, 0x00, 0x80, 0x5f, 0x6c, 0xef, 0x60 } };
static const GUID kIidSpecifyPropertyPages = { 0xB196B28B, 0xBAB4, 0x101A, { 0xB6, 0x9C, 0x00, 0xAA, 0x00, 0x34, 0x1D, 0x07 } };

// ---- the DLL under test ----------------------------------------------------------------------

typedef HRESULT(WINAPI* GetClassObjectFn)(REFCLSID, REFIID, void**);
typedef HRESULT(WINAPI* CanUnloadFn)();
static GetClassObjectFn g_getClassObject;
static CanUnloadFn g_canUnload;

static ComPtr<IBaseFilter> NewFilter(HRESULT* out = nullptr)
{
    ComPtr<IClassFactory> factory;
    HRESULT hr = g_getClassObject(CLSID_CamStudioDShowQ, IID_PPV_ARGS(&factory));
    ComPtr<IBaseFilter> f;
    if (SUCCEEDED(hr)) hr = factory->CreateInstance(nullptr, IID_PPV_ARGS(&f));
    if (out) *out = hr;
    return f;
}

// ---- host emulation (the real FrameWriter) ---------------------------------------------------

static void MakePattern(int id, uint32_t w, uint32_t h, std::vector<uint8_t>& out)
{
    out.resize(static_cast<size_t>(w) * h * 3 / 2);
    uint32_t s = 0x9E3779B9u * (id + 1);
    auto rnd = [&] { s ^= s << 13; s ^= s >> 17; s ^= s << 5; return s; };
    for (uint32_t y = 0; y < h; ++y)
        for (uint32_t x = 0; x < w; ++x)
            out[static_cast<size_t>(y) * w + x] = static_cast<uint8_t>(32 + ((x * 3 + y * 5 + id * 41) & 0x7F) + (rnd() & 0x3F));
    uint8_t* uv = out.data() + static_cast<size_t>(w) * h;
    for (uint32_t y = 0; y < h / 2; ++y)
        for (uint32_t x = 0; x < w; ++x)
            uv[static_cast<size_t>(y) * w + x] = static_cast<uint8_t>(64 + ((x * 2 + y * 7 + id * 13) & 0x7F));
}

struct Host
{
    FrameWriter w;
    std::thread th;
    std::atomic<bool> quit{ false };
    std::atomic<bool> silent{ false };
    std::atomic<int> pattern{ 0 };
    std::mutex lock;
    uint32_t W = 0, H = 0;

    bool Start(uint32_t width, uint32_t height, int pid)
    {
        uint32_t ow = 0, oh = 0;
        if (FAILED(w.OpenLocal(width, height, ow, oh))) return false;
        W = ow;
        H = oh;
        pattern = pid;
        quit = false;
        th = std::thread([this] { Loop(); });
        return true;
    }
    void Loop()
    {
        std::vector<uint8_t> buf;
        int lastId = -1;
        uint32_t lw = 0, lh = 0;
        while (!quit)
        {
            {
                std::lock_guard<std::mutex> lk(lock);
                if (!silent)
                {
                    const int id = pattern;
                    if (id != lastId || lw != W || lh != H) { MakePattern(id, W, H, buf); lastId = id; lw = W; lh = H; }
                    w.Write(buf.data(), static_cast<long>(W), W, H);
                }
            }
            Sleep(33);
        }
    }
    // The size change OpenLocal refuses while a reader streams (readerActive > 0), done by hand:
    // what a host that raced a reader would have written.
    void ForceResize(uint32_t nw, uint32_t nh, int pid)
    {
        std::lock_guard<std::mutex> lk(lock);
        SharedFrameHeader* h = nullptr;
        // open a second view of the same mapping (FrameWriter keeps its header private)
        HANDLE m = OpenFileMappingW(FILE_MAP_READ | FILE_MAP_WRITE, FALSE, g_mapName.c_str());
        if (!m) return;
        h = static_cast<SharedFrameHeader*>(MapViewOfFile(m, FILE_MAP_READ | FILE_MAP_WRITE, 0, 0, 0));
        if (h)
        {
            for (uint32_t s = 0; s < CAMSTUDIO_SLOTS; ++s) InterlockedIncrement64(&h->slotSeq[s]);
            h->width = nw;
            h->height = nh;
            h->stride = nw;
            InterlockedExchange64(&h->latest, 0);
            for (uint32_t s = 0; s < CAMSTUDIO_SLOTS; ++s) InterlockedIncrement64(&h->slotSeq[s]);
            UnmapViewOfFile(h);
        }
        CloseHandle(m);
        W = nw;
        H = nh;
        pattern = pid;
    }
    int ReaderActive() { return w.ConsumerActive() ? 1 : 0; }
    LONG RawReaderActive()
    {
        HANDLE m = OpenFileMappingW(FILE_MAP_READ, FALSE, g_mapName.c_str());
        if (!m) return -99;
        auto* h = static_cast<SharedFrameHeader*>(MapViewOfFile(m, FILE_MAP_READ, 0, 0, 0));
        LONG v = h ? h->readerActive : -98;
        if (h) UnmapViewOfFile(h);
        CloseHandle(m);
        return v;
    }
    void Stop()
    {
        quit = true;
        if (th.joinable()) th.join();
        w.Close();
    }
    ~Host() { Stop(); }
};

static bool MappingExists()
{
    HANDLE m = OpenFileMappingW(FILE_MAP_READ, FALSE, g_mapName.c_str());
    if (m) CloseHandle(m);
    return m != nullptr;
}

// ---- expected pictures (ground truth from the C++ headers) -----------------------------------

static void Nv12ToYuy2Q(const uint8_t* nv12, uint32_t w, uint32_t h, uint8_t* dst)
{
    const uint8_t* uvPlane = nv12 + static_cast<size_t>(w) * h;
    for (uint32_t y = 0; y < h; ++y)
    {
        const uint8_t* yr = nv12 + static_cast<size_t>(y) * w;
        const uint8_t* uv = uvPlane + static_cast<size_t>(y / 2) * w;
        uint8_t* d = dst + static_cast<size_t>(y) * w * 2;
        for (uint32_t x = 0; x < w; x += 2, d += 4) { d[0] = yr[x]; d[1] = uv[x]; d[2] = yr[x + 1]; d[3] = uv[x + 1]; }
    }
}
static void Nv12ToI420Q(const uint8_t* nv12, uint32_t w, uint32_t h, uint8_t* dst)
{
    const size_t luma = static_cast<size_t>(w) * h;
    memcpy(dst, nv12, luma);
    const uint8_t* uv = nv12 + luma;
    uint8_t* u = dst + luma;
    uint8_t* v = u + luma / 4;
    for (size_t i = 0; i < luma / 4; ++i) { u[i] = uv[2 * i]; v[i] = uv[2 * i + 1]; }
}
static uint64_t ExpectedHash(int pattern, uint32_t sw, uint32_t sh, uint32_t dw, uint32_t dh, uint32_t fourcc)
{
    std::vector<uint8_t> src, fit(static_cast<size_t>(dw) * dh * 3 / 2);
    MakePattern(pattern, sw, sh, src);
    Nv12FitInto(src.data(), static_cast<long>(sw), sw, sh, fit.data(), static_cast<long>(dw), dw, dh);
    if (fourcc == kNV12) return HashBytes(fit.data(), fit.size());
    std::vector<uint8_t> out(fourcc == kYUY2 ? static_cast<size_t>(dw) * dh * 2 : fit.size());
    if (fourcc == kYUY2) Nv12ToYuy2Q(fit.data(), dw, dh, out.data());
    else Nv12ToI420Q(fit.data(), dw, dh, out.data());
    return HashBytes(out.data(), out.size());
}

// ---- sample collection -----------------------------------------------------------------------

struct Rec
{
    LONGLONG t;
    uint64_t hash;
    long len;
    long size;
    bool timed, sync, preroll, disc, mediaTime;
    LONGLONG dur;
    // no-signal check: luma bytes all in {0x30,0xB0}, chroma all 0x80 (format-aware, filled in by the collector)
    bool noSignalLike;
};

struct Collector final : ISampleGrabberCBQ
{
    std::mutex lock;
    std::vector<Rec> recs;
    uint32_t fourcc = kNV12, w = 0, h = 0;
    STDMETHODIMP QueryInterface(REFIID r, void** p) override
    {
        if (r == IID_IUnknown || r == __uuidof(ISampleGrabberCBQ)) { *p = this; return S_OK; }
        *p = nullptr;
        return E_NOINTERFACE;
    }
    STDMETHODIMP_(ULONG) AddRef() override { return 2; }
    STDMETHODIMP_(ULONG) Release() override { return 1; }
    STDMETHODIMP BufferCB(double, BYTE*, long) override { return S_OK; }
    STDMETHODIMP SampleCB(double, IMediaSample* s) override
    {
        BYTE* p = nullptr;
        s->GetPointer(&p);
        Rec r{};
        r.t = Qpc100ns();
        r.len = s->GetActualDataLength();
        r.size = s->GetSize();
        r.hash = p ? HashBytes(p, static_cast<size_t>(r.len)) : 0;
        LONGLONG a = 0, b = 0;
        const HRESULT ht = s->GetTime(&a, &b);
        r.timed = ht == S_OK;
        r.dur = r.timed ? b - a : 0;
        r.sync = s->IsSyncPoint() == S_OK;
        r.preroll = s->IsPreroll() == S_OK;
        r.disc = s->IsDiscontinuity() == S_OK;
        LONGLONG m0 = 0, m1 = 0;
        r.mediaTime = s->GetMediaTime(&m0, &m1) == S_OK;
        // no-signal pattern?
        bool ok = p != nullptr && w && h;
        if (ok)
        {
            const size_t n = static_cast<size_t>(w) * h;
            if (fourcc == kYUY2)
            {
                for (size_t i = 0; i < n && ok; ++i) ok = (p[2 * i] == 0x30 || p[2 * i] == 0xB0) && p[2 * i + 1] == 0x80;
            }
            else
            {
                for (size_t i = 0; i < n && ok; ++i) ok = p[i] == 0x30 || p[i] == 0xB0;
                for (size_t i = n; i < static_cast<size_t>(r.len) && ok; ++i) ok = p[i] == 0x80;
            }
        }
        r.noSignalLike = ok;
        std::lock_guard<std::mutex> lk(lock);
        recs.push_back(r);
        return S_OK;
    }
    size_t Mark()
    {
        std::lock_guard<std::mutex> lk(lock);
        return recs.size();
    }
    std::vector<Rec> Range(size_t from, size_t to)
    {
        std::lock_guard<std::mutex> lk(lock);
        if (to > recs.size()) to = recs.size();
        return std::vector<Rec>(recs.begin() + std::min(from, to), recs.begin() + to);
    }
};

// Prints the deterministic and the timing lines of a phase. `expected` are hashes a live frame may have.
static void Summarize(const std::string& tag, const std::vector<Rec>& r, const std::set<uint64_t>& expected, double seconds, bool showHashes = true)
{
    std::map<uint64_t, int> hist;
    std::set<long> lens;
    int timed = 0, sync = 0, preroll = 0, disc = 0, mediaTime = 0, noSig = 0;
    std::set<LONGLONG> durs;
    LONGLONG maxGap = 0, sumGap = 0;
    for (size_t i = 0; i < r.size(); ++i)
    {
        hist[r[i].hash]++;
        lens.insert(r[i].len);
        timed += r[i].timed;
        sync += r[i].sync;
        preroll += r[i].preroll;
        disc += r[i].disc;
        mediaTime += r[i].mediaTime;
        noSig += r[i].noSignalLike;
        if (r[i].timed) durs.insert(r[i].dur);
        if (i)
        {
            const LONGLONG g = r[i].t - r[i - 1].t;
            maxGap = std::max(maxGap, g);
            sumGap += g;
        }
    }
    std::string hashes, lenstr, durstr;
    for (auto& kv : hist)
        if (kv.second >= 3) hashes += Hex64(kv.first) + (expected.count(kv.first) ? "*" : "?") + " ";
    for (long l : lens) lenstr += Fmt("%ld ", l);
    for (LONGLONG d : durs) durstr += Fmt("%lld ", d);
    const size_t n = r.size();
    // (the no-signal bar moves with the clock, so its hashes are not comparable between runs)
    D(tag + ".stable_hashes", !showHashes ? "(not compared)" : hashes.empty() ? "(none)" : hashes);
    D(tag + ".lengths", lenstr.empty() ? "(none)" : lenstr);
    D(tag + ".sync", n == 0 ? "-" : sync == static_cast<int>(n) ? "all" : sync == 0 ? "none" : "some");
    D(tag + ".preroll", n == 0 ? "-" : preroll == 0 ? "none" : "some");
    D(tag + ".mediatime_set", n == 0 ? "-" : mediaTime == 0 ? "none" : "some");
    D(tag + ".discontinuity_first", n ? Fmt("%d", r[0].disc ? 1 : 0) : "-");
    D(tag + ".discontinuity_later", Fmt("%d", disc - (n && r[0].disc ? 1 : 0)));
    D(tag + ".timed", n == 0 ? "-" : timed == static_cast<int>(n) ? "all" : timed == 0 ? "none" : "some");
    D(tag + ".duration", durstr.empty() ? "(none)" : durstr);
    D(tag + ".nosignal_like", n == 0 ? "-" : noSig == static_cast<int>(n) ? "all" : noSig == 0 ? "none" : "some");
    D(tag + ".delivering", n >= 3 ? "yes" : "no");
    T(tag + ".fps", seconds > 0 ? n / seconds : 0);
    T(tag + ".gap_max_ms", maxGap / 10000.0);
    T(tag + ".gap_mean_ms", n > 1 ? sumGap / 10000.0 / (n - 1) : 0);
}

// ---- graph rig -------------------------------------------------------------------------------

struct Rig
{
    ComPtr<IBaseFilter> camera, grabberFilter, sink;
    ComPtr<IGraphBuilder> graph;
    ComPtr<ICaptureGraphBuilder2> builder;
    ComPtr<ISampleGrabberQ> grabber;
    ComPtr<IMediaControl> control;
    Collector col;
    uint32_t connW = 0, connH = 0, connFourcc = 0;
    LONGLONG connInterval = 0;

    HRESULT Create(const std::string& tag, uint32_t fourcc)
    {
        HRESULT hr;
        camera = NewFilter(&hr);
        D(tag + ".create_filter", Hr(hr));
        if (!camera) return hr;
        CoCreateInstance(CLSID_FilterGraph, nullptr, CLSCTX_INPROC_SERVER, IID_PPV_ARGS(&graph));
        CoCreateInstance(CLSID_CaptureGraphBuilder2, nullptr, CLSCTX_INPROC_SERVER, IID_PPV_ARGS(&builder));
        builder->SetFiltergraph(graph.Get());
        D(tag + ".add_filter", Hr(graph->AddFilter(camera.Get(), L"NiwCam")));
        CoCreateInstance(CLSID_SampleGrabberQ, nullptr, CLSCTX_INPROC_SERVER, IID_PPV_ARGS(&grabberFilter));
        CoCreateInstance(CLSID_NullRendererQ, nullptr, CLSCTX_INPROC_SERVER, IID_PPV_ARGS(&sink));
        if (!grabberFilter || !sink || FAILED(grabberFilter.As(&grabber))) { D(tag + ".grabber", "missing"); return E_FAIL; }
        AM_MEDIA_TYPE want{};
        want.majortype = MEDIATYPE_Video;
        want.formattype = FORMAT_VideoInfo;
        want.subtype = FourccSub(fourcc);
        grabber->SetMediaType(&want);
        grabber->SetBufferSamples(FALSE);
        graph->AddFilter(grabberFilter.Get(), L"Grabber");
        graph->AddFilter(sink.Get(), L"Null");
        col.fourcc = fourcc;
        grabber->SetCallback(&col, 0);
        graph.As(&control);
        return S_OK;
    }
    // SetFormat through IAMStreamConfig found the way an app finds it (capture graph builder).
    HRESULT SetFormat(const std::string& tag, uint32_t fourcc, uint32_t w, uint32_t h, LONGLONG interval)
    {
        ComPtr<IAMStreamConfig> cfg;
        HRESULT hr = builder->FindInterface(&PIN_CATEGORY_CAPTURE, &MEDIATYPE_Video, camera.Get(), IID_PPV_ARGS(&cfg));
        D(tag + ".find_streamconfig", Hr(hr));
        if (!cfg) return hr;
        VIDEOINFOHEADER vih{};
        vih.AvgTimePerFrame = interval;
        vih.bmiHeader.biSize = sizeof(BITMAPINFOHEADER);
        vih.bmiHeader.biWidth = static_cast<LONG>(w);
        vih.bmiHeader.biHeight = static_cast<LONG>(h);
        vih.bmiHeader.biPlanes = 1;
        vih.bmiHeader.biBitCount = fourcc == kYUY2 ? 16 : 12;
        vih.bmiHeader.biCompression = fourcc;
        vih.bmiHeader.biSizeImage = fourcc == kYUY2 ? w * h * 2 : w * h * 3 / 2;
        AM_MEDIA_TYPE mt{};
        mt.majortype = MEDIATYPE_Video;
        mt.subtype = FourccSub(fourcc);
        mt.formattype = FORMAT_VideoInfo;
        mt.cbFormat = sizeof(vih);
        mt.pbFormat = reinterpret_cast<BYTE*>(&vih);
        mt.lSampleSize = vih.bmiHeader.biSizeImage;
        mt.bFixedSizeSamples = TRUE;
        hr = cfg->SetFormat(&mt);
        D(tag + ".set_format", Hr(hr));
        return hr;
    }
    HRESULT Render(const std::string& tag)
    {
        const HRESULT hr = builder->RenderStream(&PIN_CATEGORY_CAPTURE, &MEDIATYPE_Video, camera.Get(), grabberFilter.Get(), sink.Get());
        D(tag + ".render_stream", Hr(hr));
        ReadConnected(tag);
        return hr;
    }
    void ReadConnected(const std::string& tag)
    {
        AM_MEDIA_TYPE mt{};
        if (grabber && SUCCEEDED(grabber->GetConnectedMediaType(&mt)) && mt.formattype == FORMAT_VideoInfo && mt.pbFormat)
        {
            const auto* v = reinterpret_cast<const VIDEOINFOHEADER*>(mt.pbFormat);
            connW = static_cast<uint32_t>(v->bmiHeader.biWidth);
            connH = static_cast<uint32_t>(v->bmiHeader.biHeight < 0 ? -v->bmiHeader.biHeight : v->bmiHeader.biHeight);
            connFourcc = v->bmiHeader.biCompression;
            connInterval = v->AvgTimePerFrame;
            col.w = connW;
            col.h = connH;
            D(tag + ".connected", Fmt("%s %ux%u interval=%lld bitrate=%u sampleSize=%lu", Fourcc(connFourcc).c_str(), connW, connH,
                                      connInterval, v->dwBitRate, mt.lSampleSize));
            CoTaskMemFree(mt.pbFormat);
        }
        else D(tag + ".connected", "(none)");
    }
    void Release(const std::string& tag)
    {
        if (control) control->Stop();
        if (grabber) grabber->SetCallback(nullptr, 0);
        control.Reset();
        grabber.Reset();
        sink.Reset();
        grabberFilter.Reset();
        builder.Reset();
        graph.Reset();
        camera.Reset();
        Sleep(150);
        D(tag + ".channel_released", MappingExists() ? "no" : "yes");
    }
};

static void Wait(double s) { Sleep(static_cast<DWORD>(s * 1000)); }

// ---- A. static API behaviour -----------------------------------------------------------------

static std::string Mt(const AM_MEDIA_TYPE& mt)
{
    if (mt.formattype != FORMAT_VideoInfo || !mt.pbFormat) return "(not videoinfo)";
    const auto* v = reinterpret_cast<const VIDEOINFOHEADER*>(mt.pbFormat);
    return Fmt("%s %ldx%ld interval=%lld bitrate=%u sampleSize=%lu fixed=%d temporal=%d cb=%lu bits=%u planes=%u image=%u size=%u major=%d",
               Fourcc(v->bmiHeader.biCompression).c_str(), v->bmiHeader.biWidth, v->bmiHeader.biHeight, v->AvgTimePerFrame, v->dwBitRate,
               mt.lSampleSize, mt.bFixedSizeSamples, mt.bTemporalCompression, mt.cbFormat, v->bmiHeader.biBitCount, v->bmiHeader.biPlanes,
               v->bmiHeader.biSizeImage, v->bmiHeader.biSize, mt.majortype == MEDIATYPE_Video);
}

static std::string QiRow(IUnknown* o)
{
    struct { const char* n; const GUID* g; } rows[] = {
        { "IUnknown", &IID_IUnknown }, { "IPersist", &IID_IPersist }, { "IMediaFilter", &IID_IMediaFilter },
        { "IBaseFilter", &IID_IBaseFilter }, { "IPin", &IID_IPin }, { "IAMStreamConfig", &IID_IAMStreamConfig },
        { "IKsPropertySet", &__uuidof(IKsPropertySetQ) }, { "IQualityControl", &__uuidof(IQualityControl) },
        { "IMarshal", &kIidMarshal }, { "IAgileObject", &kIidAgile }, { "IAMFilterMisc", &kIidAMFilterMisc },
        { "IAMStreamControl", &kIidAMStreamControl }, { "ISpecifyPropertyPages", &kIidSpecifyPropertyPages },
        { "IEnumPins", &IID_IEnumPins },
    };
    std::string s;
    for (auto& r : rows)
    {
        void* p = nullptr;
        const HRESULT hr = o->QueryInterface(*r.g, &p);
        if (p) static_cast<IUnknown*>(p)->Release();
        s += Fmt("%s=%s ", r.n, Hr(hr).c_str());
    }
    return s;
}

static void DumpMediaTypes(const std::string& tag, IPin* pin, IAMStreamConfig* cfg)
{
    ComPtr<IEnumMediaTypes> en;
    D(tag + ".enum_mediatypes", Hr(pin->EnumMediaTypes(&en)));
    int n = 0;
    AM_MEDIA_TYPE* mt = nullptr;
    ULONG got = 0;
    while (en && en->Next(1, &mt, &got) == S_OK)
    {
        D(Fmt("%s.mt[%d]", tag.c_str(), n++), Mt(*mt));
        CoTaskMemFree(mt->pbFormat);
        CoTaskMemFree(mt);
    }
    D(tag + ".mt_count", Fmt("%d", n));
    // enumerator behaviour
    if (en)
    {
        D(tag + ".enum.reset", Hr(en->Reset()));
        D(tag + ".enum.skip(2)", Hr(en->Skip(2)));
        ComPtr<IEnumMediaTypes> cl;
        D(tag + ".enum.clone", Hr(en->Clone(&cl)));
        AM_MEDIA_TYPE* a = nullptr;
        AM_MEDIA_TYPE* b = nullptr;
        ULONG fa = 0, fb = 0;
        const HRESULT ha = en->Next(1, &a, &fa);
        const HRESULT hb = cl ? cl->Next(1, &b, &fb) : E_FAIL;
        D(tag + ".enum.next_after_skip", Fmt("%s/%s same=%d", Hr(ha).c_str(), Hr(hb).c_str(), a && b && Mt(*a) == Mt(*b)));
        for (AM_MEDIA_TYPE* p : { a, b })
            if (p) { CoTaskMemFree(p->pbFormat); CoTaskMemFree(p); }
        D(tag + ".enum.skip(1000)", Hr(en->Skip(1000)));
        AM_MEDIA_TYPE* c = nullptr;
        ULONG fc = 7;
        { const HRESULT h__ = en->Next(1, &c, &fc); D(tag + ".enum.next_at_end", Hr(h__) + Fmt(" fetched=%lu", fc)); }
    }
    if (!cfg) return;
    int count = -1, size = -1;
    { const HRESULT h__ = cfg->GetNumberOfCapabilities(&count, &size); D(tag + ".caps_count", Hr(h__) + Fmt(" count=%d size=%d", count, size)); }
    for (int i = 0; i <= count; ++i)
    {
        AM_MEDIA_TYPE* m = nullptr;
        VIDEO_STREAM_CONFIG_CAPS c{};
        memset(&c, 0xCD, sizeof c);
        const HRESULT hr = cfg->GetStreamCaps(i, &m, reinterpret_cast<BYTE*>(&c));
        if (hr != S_OK) { D(Fmt("%s.caps[%d]", tag.c_str(), i), Hr(hr)); continue; }
        D(Fmt("%s.caps[%d]", tag.c_str(), i),
          Mt(*m) + Fmt(" | in=%ldx%ld cropmin=%ldx%ld cropmax=%ldx%ld grx=%d gry=%d ax=%d ay=%d outmin=%ldx%ld outmax=%ldx%ld ogx=%d ogy=%d taps=%d,%d,%d,%d frame=%lld..%lld bps=%ld..%ld std=%lu guid=%d",
                       c.InputSize.cx, c.InputSize.cy, c.MinCroppingSize.cx, c.MinCroppingSize.cy, c.MaxCroppingSize.cx, c.MaxCroppingSize.cy,
                       c.CropGranularityX, c.CropGranularityY, c.CropAlignX, c.CropAlignY, c.MinOutputSize.cx, c.MinOutputSize.cy,
                       c.MaxOutputSize.cx, c.MaxOutputSize.cy, c.OutputGranularityX, c.OutputGranularityY, c.StretchTapsX, c.StretchTapsY,
                       c.ShrinkTapsX, c.ShrinkTapsY, c.MinFrameInterval, c.MaxFrameInterval, c.MinBitsPerSecond, c.MaxBitsPerSecond,
                       c.VideoStandard, c.guid == FORMAT_VideoInfo));
        CoTaskMemFree(m->pbFormat);
        CoTaskMemFree(m);
    }
    D(tag + ".caps_negative", [&] { AM_MEDIA_TYPE* m = nullptr; VIDEO_STREAM_CONFIG_CAPS c{}; return Hr(cfg->GetStreamCaps(-1, &m, reinterpret_cast<BYTE*>(&c))); }());
    D(tag + ".caps_null", Hr(cfg->GetStreamCaps(0, nullptr, nullptr)));
    D(tag + ".caps_count_null", Hr(cfg->GetNumberOfCapabilities(nullptr, &size)));
    AM_MEDIA_TYPE* f = nullptr;
    { const HRESULT h__ = cfg->GetFormat(&f); D(tag + ".get_format", Hr(h__) + " " + (f ? Mt(*f) : "-")); }
    if (f) { CoTaskMemFree(f->pbFormat); CoTaskMemFree(f); }
    D(tag + ".get_format_null", Hr(cfg->GetFormat(nullptr)));
}

static void StaticApi(const std::string& host)
{
    const std::string t = "api" + host;
    D("api.can_unload_now", Hr(g_canUnload()));
    {
        ComPtr<IClassFactory> f;
        void* raw = reinterpret_cast<void*>(0x1234);
        { const HRESULT h__ = g_getClassObject(IID_IUnknown, IID_IClassFactory, &raw); D("api.getclassobject.wrong_clsid", Hr(h__) + (raw ? " out=nonnull" : " out=null")); }
        D("api.getclassobject.null_ppv", Hr(g_getClassObject(CLSID_CamStudioDShowQ, IID_IClassFactory, nullptr)));
        { const HRESULT h__ = g_getClassObject(CLSID_CamStudioDShowQ, kIidAgile, &raw); D("api.getclassobject.bad_iid", Hr(h__) + (raw ? " out=nonnull" : " out=null")); }
        D("api.getclassobject", Hr(g_getClassObject(CLSID_CamStudioDShowQ, IID_PPV_ARGS(&f))));
        if (f)
        {
            D("api.factory.qi", [&] { ComPtr<IUnknown> u; return Hr(f.As(&u)); }());
            D("api.factory.lockserver(1)", Hr(f->LockServer(TRUE)));
            D("api.factory.lockserver(0)", Hr(f->LockServer(FALSE)));
            void* p = reinterpret_cast<void*>(0x1234);
            { const HRESULT h__ = f->CreateInstance(reinterpret_cast<IUnknown*>(1), IID_IBaseFilter, &p); D("api.factory.create_aggregated", Hr(h__) + (p ? " out=nonnull" : " out=null")); }
            D("api.factory.create_null_ppv", Hr(f->CreateInstance(nullptr, IID_IBaseFilter, nullptr)));
            p = reinterpret_cast<void*>(0x1234);
            { const HRESULT h__ = f->CreateInstance(nullptr, kIidAgile, &p); D("api.factory.create_bad_iid", Hr(h__) + (p ? " out=nonnull" : " out=null")); }
            ComPtr<IUnknown> u;
            D("api.factory.create_iunknown", Hr(f->CreateInstance(nullptr, IID_PPV_ARGS(&u))));
            D("api.factory.create_ipin", [&] { ComPtr<IPin> pin; return Hr(f->CreateInstance(nullptr, IID_PPV_ARGS(&pin))); }());
        }
    }

    ComPtr<IBaseFilter> filter = NewFilter();
    if (!filter) { D("api.filter", "missing"); return; }
    D("api.filter.qi", QiRow(filter.Get()));
    CLSID id{};
    { const HRESULT h__ = filter->GetClassID(&id); D("api.filter.getclassid", Hr(h__) + (id == CLSID_CamStudioDShowQ ? " ours" : " other")); }
    D("api.filter.getclassid_null", Hr(filter->GetClassID(nullptr)));
    FILTER_STATE st = State_Running;
    { const HRESULT h__ = filter->GetState(0, &st); D("api.filter.getstate", Hr(h__) + Fmt(" state=%d", st)); }
    D("api.filter.getstate_null", Hr(filter->GetState(0, nullptr)));
    ComPtr<IReferenceClock> clk;
    { const HRESULT h__ = filter->GetSyncSource(&clk); D("api.filter.getsyncsource", Hr(h__) + (clk ? " clock" : " none")); }
    D("api.filter.getsyncsource_null", Hr(filter->GetSyncSource(nullptr)));
    LPWSTR vendor = reinterpret_cast<LPWSTR>(1);
    D("api.filter.queryvendorinfo", Hr(filter->QueryVendorInfo(&vendor)));
    FILTER_INFO fi{};
    wcscpy_s(fi.achName, L"junk");
    { const HRESULT h__ = filter->QueryFilterInfo(&fi); D("api.filter.queryfilterinfo(before join)", Hr(h__) + Fmt(" name='%ls' graph=%s", fi.achName, fi.pGraph ? "set" : "null")); }
    if (fi.pGraph) fi.pGraph->Release();
    D("api.filter.queryfilterinfo_null", Hr(filter->QueryFilterInfo(nullptr)));

    // pins
    ComPtr<IEnumPins> pins;
    D("api.filter.enumpins", Hr(filter->EnumPins(&pins)));
    D("api.filter.enumpins_null", Hr(filter->EnumPins(nullptr)));
    ComPtr<IPin> pin;
    ULONG got = 5;
    { const HRESULT h__ = pins->Next(1, &pin, &got); D("api.enumpins.next(1)", Hr(h__) + Fmt(" fetched=%lu", got)); }
    IPin* two[2] = {};
    got = 5;
    { const HRESULT h__ = pins->Next(1, two, &got); D("api.enumpins.next(1)_again", Hr(h__) + Fmt(" fetched=%lu", got)); }
    D("api.enumpins.reset", Hr(pins->Reset()));
    got = 5;
    { const HRESULT h__ = pins->Next(2, two, &got); D("api.enumpins.next(2)", Hr(h__) + Fmt(" fetched=%lu first=%s", got, two[0] ? "set" : "null")); }
    if (two[0]) two[0]->Release();
    D("api.enumpins.next(2)_nofetched", Hr(pins->Next(2, two, nullptr)));
    D("api.enumpins.next(1)_nofetched", [&] { pins->Reset(); IPin* p = nullptr; const HRESULT hr = pins->Next(1, &p, nullptr); if (p) p->Release(); return Hr(hr); }());
    D("api.enumpins.next_null", Hr(pins->Next(1, nullptr, nullptr)));
    D("api.enumpins.reset2", Hr(pins->Reset()));
    D("api.enumpins.skip(1)", Hr(pins->Skip(1)));
    D("api.enumpins.skip(1)_again", Hr(pins->Skip(1)));
    {
        pins->Reset();
        pins->Skip(1);
        ComPtr<IEnumPins> c2;
        D("api.enumpins.clone", Hr(pins->Clone(&c2)));
        IPin* p = nullptr;
        got = 5;
        if (c2) { const HRESULT h__ = c2->Next(1, &p, &got); D("api.enumpins.clone_next(after skip)", Hr(h__) + Fmt(" fetched=%lu", got)); }
        if (p) p->Release();
        D("api.enumpins.clone_null", Hr(pins->Clone(nullptr)));
    }
    {
        ComPtr<IPin> p;
        D("api.filter.findpin(Capture)", Hr(filter->FindPin(L"Capture", &p)));
        D("api.filter.findpin(same object)", p.Get() == pin.Get() ? "same" : "different");
        IPin* raw = reinterpret_cast<IPin*>(0x1234);
        { const HRESULT h__ = filter->FindPin(L"capture", &raw); D("api.filter.findpin(capture)", Hr(h__) + (raw ? " out=nonnull" : " out=null")); }
        raw = reinterpret_cast<IPin*>(0x1234);
        { const HRESULT h__ = filter->FindPin(nullptr, &raw); D("api.filter.findpin(null id)", Hr(h__) + (raw ? " out=nonnull" : " out=null")); }
        D("api.filter.findpin(null out)", Hr(filter->FindPin(L"Capture", nullptr)));
    }
    if (!pin) { D("api.pin", "missing"); return; }
    D("api.pin.qi", QiRow(pin.Get()));
    PIN_INFO pi{};
    wcscpy_s(pi.achName, L"junk");
    {
        const HRESULT h__ = pin->QueryPinInfo(&pi);
        const char* which = "null";
        if (pi.pFilter)
        {
            ComPtr<IUnknown> a, b;
            pi.pFilter->QueryInterface(IID_PPV_ARGS(&a));
            filter.As(&b);
            which = a.Get() == b.Get() ? "same-as-filter" : "other";
        }
        D("api.pin.querypininfo", Hr(h__) + Fmt(" dir=%d name='%ls' filter=%s", pi.dir, pi.achName, which));
    }
    if (pi.pFilter) pi.pFilter->Release();
    D("api.pin.querypininfo_null", Hr(pin->QueryPinInfo(nullptr)));
    PIN_DIRECTION dir = PINDIR_INPUT;
    { const HRESULT h__ = pin->QueryDirection(&dir); D("api.pin.querydirection", Hr(h__) + Fmt(" dir=%d", dir)); }
    D("api.pin.querydirection_null", Hr(pin->QueryDirection(nullptr)));
    LPWSTR pid = nullptr;
    { const HRESULT h__ = pin->QueryId(&pid); D("api.pin.queryid", Hr(h__) + Fmt(" id='%ls'", pid ? pid : L"(null)")); }
    CoTaskMemFree(pid);
    D("api.pin.queryid_null", Hr(pin->QueryId(nullptr)));
    IPin* ctRaw = reinterpret_cast<IPin*>(0x1234);
    { const HRESULT h__ = pin->ConnectedTo(&ctRaw); D("api.pin.connectedto(not connected)", Hr(h__) + (ctRaw ? " out=nonnull" : " out=null")); }
    D("api.pin.connectedto_null", Hr(pin->ConnectedTo(nullptr)));
    AM_MEDIA_TYPE cmt;
    memset(&cmt, 0xCD, sizeof cmt);
    { const HRESULT h__ = pin->ConnectionMediaType(&cmt); D("api.pin.connectionmediatype(not connected)", Hr(h__) + Fmt(" zeroed=%d", cmt.cbFormat == 0 && cmt.pbFormat == nullptr && cmt.pUnk == nullptr && cmt.lSampleSize == 0)); }
    D("api.pin.connectionmediatype_null", Hr(pin->ConnectionMediaType(nullptr)));
    D("api.pin.disconnect(not connected)", Hr(pin->Disconnect()));
    D("api.pin.receiveconnection", Hr(pin->ReceiveConnection(pin.Get(), nullptr)));
    {
        IPin* ic[2] = {};
        ULONG n = 2;
        D("api.pin.queryinternalconnections", Hr(pin->QueryInternalConnections(ic, &n)));
    }
    D("api.pin.endofstream", Hr(pin->EndOfStream()));
    D("api.pin.beginflush", Hr(pin->BeginFlush()));
    D("api.pin.endflush", Hr(pin->EndFlush()));
    D("api.pin.newsegment", Hr(pin->NewSegment(0, 100, 1.0)));
    D("api.pin.queryaccept(null)", Hr(pin->QueryAccept(nullptr)));

    // QueryAccept variants
    {
        auto vih = [&](uint32_t fourcc, LONG w, LONG h, REFERENCE_TIME avg, bool v2, bool shortFormat) {
            VIDEOINFOHEADER2 v{};
            v.AvgTimePerFrame = avg;
            v.bmiHeader.biSize = sizeof(BITMAPINFOHEADER);
            v.bmiHeader.biWidth = w;
            v.bmiHeader.biHeight = h;
            v.bmiHeader.biPlanes = 1;
            v.bmiHeader.biCompression = fourcc;
            AM_MEDIA_TYPE mt{};
            mt.majortype = MEDIATYPE_Video;
            mt.subtype = FourccSub(fourcc);
            mt.formattype = v2 ? FORMAT_VideoInfo2 : FORMAT_VideoInfo;
            mt.cbFormat = shortFormat ? 10 : (v2 ? sizeof(VIDEOINFOHEADER2) : sizeof(VIDEOINFOHEADER));
            VIDEOINFOHEADER v1{};
            if (!v2)
            {
                v1.AvgTimePerFrame = avg;
                v1.bmiHeader = v.bmiHeader;
                mt.pbFormat = reinterpret_cast<BYTE*>(&v1);
                return pin->QueryAccept(&mt);
            }
            mt.pbFormat = reinterpret_cast<BYTE*>(&v);
            return pin->QueryAccept(&mt);
        };
        D("api.queryaccept.nv12 1280x720", Hr(vih(kNV12, 1280, 720, 333333, false, false)));
        D("api.queryaccept.yuy2 640x480 v2", Hr(vih(kYUY2, 640, 480, 333333, true, false)));
        D("api.queryaccept.i420 negative height", Hr(vih(kI420, 640, -480, 0, false, false)));
        D("api.queryaccept.odd width", Hr(vih(kNV12, 641, 480, 333333, false, false)));
        D("api.queryaccept.odd height", Hr(vih(kNV12, 640, 481, 333333, false, false)));
        D("api.queryaccept.too small", Hr(vih(kNV12, 14, 480, 333333, false, false)));
        D("api.queryaccept.too big", Hr(vih(kNV12, 8194, 480, 333333, false, false)));
        D("api.queryaccept.8192", Hr(vih(kNV12, 8192, 8192, 333333, false, false)));
        D("api.queryaccept.compression mismatch", [&] {
            VIDEOINFOHEADER v1{};
            v1.bmiHeader.biSize = sizeof(BITMAPINFOHEADER);
            v1.bmiHeader.biWidth = 640;
            v1.bmiHeader.biHeight = 480;
            v1.bmiHeader.biCompression = kYUY2;
            AM_MEDIA_TYPE mt{};
            mt.majortype = MEDIATYPE_Video;
            mt.subtype = FourccSub(kNV12);
            mt.formattype = FORMAT_VideoInfo;
            mt.cbFormat = sizeof(v1);
            mt.pbFormat = reinterpret_cast<BYTE*>(&v1);
            return Hr(pin->QueryAccept(&mt));
        }());
        D("api.queryaccept.short format block", Hr(vih(kNV12, 640, 480, 333333, false, true)));
        D("api.queryaccept.unknown subtype", [&] {
            VIDEOINFOHEADER v1{};
            v1.bmiHeader.biWidth = 640;
            v1.bmiHeader.biHeight = 480;
            AM_MEDIA_TYPE mt{};
            mt.majortype = MEDIATYPE_Video;
            mt.subtype = MEDIASUBTYPE_RGB24;
            mt.formattype = FORMAT_VideoInfo;
            mt.cbFormat = sizeof(v1);
            mt.pbFormat = reinterpret_cast<BYTE*>(&v1);
            return Hr(pin->QueryAccept(&mt));
        }());
        D("api.queryaccept.audio", [&] { AM_MEDIA_TYPE mt{}; mt.majortype = MEDIATYPE_Audio; return Hr(pin->QueryAccept(&mt)); }());
    }

    // IKsPropertySet
    ComPtr<IKsPropertySetQ> ks;
    pin.As(&ks);
    if (ks)
    {
        GUID cat{};
        DWORD ret = 0;
        { const HRESULT h__ = ks->Get(kAmPropSetPin, 0, nullptr, 0, &cat, sizeof cat, &ret); D("api.ks.get(category)", Hr(h__) + Fmt(" ret=%lu capture=%d", ret, cat == kPinCategoryCapture)); }
        ret = 0;
        { const HRESULT h__ = ks->Get(kAmPropSetPin, 0, nullptr, 0, nullptr, 0, &ret); D("api.ks.get(size query)", Hr(h__) + Fmt(" ret=%lu", ret)); }
        D("api.ks.get(small buffer)", Hr(ks->Get(kAmPropSetPin, 0, nullptr, 0, &cat, 8, &ret)));
        D("api.ks.get(no returned)", Hr(ks->Get(kAmPropSetPin, 0, nullptr, 0, &cat, sizeof cat, nullptr)));
        D("api.ks.get(wrong set)", Hr(ks->Get(IID_IUnknown, 0, nullptr, 0, &cat, sizeof cat, &ret)));
        D("api.ks.get(wrong id)", Hr(ks->Get(kAmPropSetPin, 1, nullptr, 0, &cat, sizeof cat, &ret)));
        DWORD sup = 0;
        { const HRESULT h__ = ks->QuerySupported(kAmPropSetPin, 0, &sup); D("api.ks.querysupported", Hr(h__) + Fmt(" support=%lu", sup)); }
        D("api.ks.querysupported(wrong set)", Hr(ks->QuerySupported(IID_IUnknown, 0, &sup)));
        D("api.ks.querysupported(wrong id)", Hr(ks->QuerySupported(kAmPropSetPin, 3, &sup)));
        D("api.ks.set", Hr(ks->Set(kAmPropSetPin, 0, nullptr, 0, &cat, sizeof cat)));
    }
    ComPtr<IQualityControl> qc;
    pin.As(&qc);
    if (qc)
    {
        Quality q{};
        D("api.qc.notify", Hr(qc->Notify(filter.Get(), q)));
        D("api.qc.setsink", Hr(qc->SetSink(nullptr)));
    }

    // media types: no host channel yet, then with a host
    ComPtr<IAMStreamConfig> cfg;
    pin.As(&cfg);
    DumpMediaTypes("api.nohost", pin.Get(), cfg.Get());
    {
        Host host;
        if (host.Start(800, 600, 1))
        {
            DumpMediaTypes("api.host800x600", pin.Get(), cfg.Get());
            host.Stop();
        }
        else D("api.host", "could not start");
    }
    // SetFormat on a pin that is not connected, then enumerate again
    {
        ComPtr<IPin> p2;
        ComPtr<IBaseFilter> f2 = NewFilter();
        f2->FindPin(L"Capture", &p2);
        ComPtr<IAMStreamConfig> c2;
        p2.As(&c2);
        VIDEOINFOHEADER v{};
        v.AvgTimePerFrame = 400000;
        v.bmiHeader.biSize = sizeof(BITMAPINFOHEADER);
        v.bmiHeader.biWidth = 960;
        v.bmiHeader.biHeight = 540;
        v.bmiHeader.biPlanes = 1;
        v.bmiHeader.biBitCount = 16;
        v.bmiHeader.biCompression = kYUY2;
        v.bmiHeader.biSizeImage = 960 * 540 * 2;
        AM_MEDIA_TYPE mt{};
        mt.majortype = MEDIATYPE_Video;
        mt.subtype = FourccSub(kYUY2);
        mt.formattype = FORMAT_VideoInfo;
        mt.cbFormat = sizeof v;
        mt.pbFormat = reinterpret_cast<BYTE*>(&v);
        D("api.setformat(yuy2 960x540@400000)", Hr(c2->SetFormat(&mt)));
        D("api.setformat(null)", Hr(c2->SetFormat(nullptr)));
        v.bmiHeader.biWidth = 961;
        D("api.setformat(odd width)", Hr(c2->SetFormat(&mt)));
        v.bmiHeader.biWidth = 960;
        v.AvgTimePerFrame = 10;
        D("api.setformat(interval clamps to 166667)", Hr(c2->SetFormat(&mt)));
        DumpMediaTypes("api.after_setformat", p2.Get(), c2.Get());
    }
}

// ---- B. streaming scenarios ------------------------------------------------------------------

struct StreamCfg
{
    std::string tag;
    uint32_t hostW = 640, hostH = 480;
    int pattern = 3;
    uint32_t fourcc = kNV12;
    bool setFormat = false;
    uint32_t fmtW = 0, fmtH = 0;
    LONGLONG interval = 333333;
    double seconds = 1.5;
};

static void StreamOne(const StreamCfg& c)
{
    const std::string tag = c.tag;
    if (MappingExists()) D(tag + ".channel_before", "LEFT OVER");
    Host host;
    D(tag + ".host_start", host.Start(c.hostW, c.hostH, c.pattern) ? "ok" : "failed");
    D(tag + ".readeractive_before", Fmt("%ld", host.RawReaderActive()));
    Rig rig;
    if (FAILED(rig.Create(tag, c.fourcc))) { host.Stop(); return; }
    if (c.setFormat) rig.SetFormat(tag, c.fourcc, c.fmtW, c.fmtH, c.interval);
    if (FAILED(rig.Render(tag))) { rig.Release(tag); return; }
    D(tag + ".get_state(stopped)", [&] { OAFilterState s; const HRESULT hr = rig.control->GetState(0, &s); return Hr(hr) + Fmt(" %d", s); }());
    D(tag + ".run", Hr(rig.control->Run()));
    D(tag + ".readeractive_running", Fmt("%ld", host.RawReaderActive()));
    Wait(0.3);
    const size_t m0 = rig.col.Mark();
    const LONGLONG t0 = Qpc100ns();
    const double cpu0 = CpuMs();
    Wait(c.seconds);
    const size_t m1 = rig.col.Mark();
    const double secs = (Qpc100ns() - t0) / 1e7;
    T(tag + ".cpu_pct", (CpuMs() - cpu0) / (secs * 10.0));
    std::set<uint64_t> expected;
    expected.insert(ExpectedHash(c.pattern, host.W, host.H, rig.connW, rig.connH, rig.connFourcc));
    Summarize(tag, rig.col.Range(m0, m1), expected, secs);
    D(tag + ".stop", Hr(rig.control->Stop()));
    D(tag + ".readeractive_stopped", Fmt("%ld", host.RawReaderActive()));
    host.Stop();
    rig.Release(tag);
}

static void NoSignal(const std::string& tag, uint32_t fourcc)
{
    Rig rig;
    if (MappingExists()) D(tag + ".channel_before", "LEFT OVER");
    if (FAILED(rig.Create(tag, fourcc))) return;
    rig.SetFormat(tag, fourcc, 640, 480, 333333); // nothing to offer a size: set one
    if (FAILED(rig.Render(tag))) { rig.Release(tag); return; }
    D(tag + ".run", Hr(rig.control->Run()));
    Wait(0.3);
    const size_t m0 = rig.col.Mark();
    const LONGLONG t0 = Qpc100ns();
    Wait(1.5);
    const size_t m1 = rig.col.Mark();
    // the channel the filter created itself
    D(tag + ".readeractive_running", Fmt("%ld", [&] { HANDLE m = OpenFileMappingW(FILE_MAP_READ, FALSE, g_mapName.c_str()); if (!m) return -99L; auto* h = static_cast<SharedFrameHeader*>(MapViewOfFile(m, FILE_MAP_READ, 0, 0, 0)); LONG v = h ? h->readerActive : -98; if (h) { D(tag + ".created_header", Fmt("magic=%08X version=%u size=%ux%u slots=%u frameBytes=%llu latest=%lld", h->magic, h->version, h->width, h->height, h->slots, h->frameBytes, h->latest)); UnmapViewOfFile(h); } CloseHandle(m); return v; }()));
    Summarize(tag, rig.col.Range(m0, m1), {}, (Qpc100ns() - t0) / 1e7, false);
    rig.Release(tag);
}

// The host writes for 1 s and stops: after 2 s the picture is "no signal".
static void Stale(const std::string& tag)
{
    if (MappingExists()) D(tag + ".channel_before", "LEFT OVER");
    Host host;
    host.Start(640, 480, 5);
    Rig rig;
    if (FAILED(rig.Create(tag, kNV12))) return;
    if (FAILED(rig.Render(tag))) { rig.Release(tag); return; }
    rig.control->Run();
    const LONGLONG t0 = Qpc100ns();
    Wait(1.0);
    host.silent = true;
    Wait(4.0);
    const auto all = rig.col.Range(0, rig.col.Mark());
    const uint64_t live = ExpectedHash(5, 640, 480, rig.connW, rig.connH, kNV12);
    // classify frames by time: [0.3,0.9] live; [3.4,4.9] no signal
    int liveA = 0, otherA = 0, liveB = 0, noSigB = 0, otherB = 0;
    for (auto& r : all)
    {
        const double s = (r.t - t0) / 1e7;
        if (s >= 0.3 && s <= 0.9) { if (r.hash == live) ++liveA; else ++otherA; }
        if (s >= 3.4 && s <= 4.9) { if (r.hash == live) ++liveB; else if (r.noSignalLike) ++noSigB; else ++otherB; }
    }
    D(tag + ".live_window", Fmt("live=%s other=%s", liveA > 10 ? "most" : "few", otherA == 0 ? "none" : "some"));
    D(tag + ".stale_window", Fmt("live=%s nosignal=%s other=%s", liveB == 0 ? "none" : "some", noSigB > 10 ? "most" : "few", otherB == 0 ? "none" : "some"));
    // when did the picture go stale (should be about 2 s after the last write)?
    double lastLive = 0, firstNoSig = 0;
    for (auto& r : all)
    {
        const double s = (r.t - t0) / 1e7;
        if (r.hash == live) lastLive = s;
        else if (r.noSignalLike && firstNoSig == 0 && s > 1.0) firstNoSig = s;
    }
    T(tag + ".last_live_s", lastLive);
    T(tag + ".first_nosignal_s", firstNoSig);
    // back to life
    host.silent = false;
    Wait(0.8);
    const auto tail = rig.col.Range(rig.col.Mark() > 12 ? rig.col.Mark() - 12 : 0, rig.col.Mark());
    int back = 0;
    for (auto& r : tail) back += r.hash == live;
    D(tag + ".recovers", back >= 8 ? "yes" : "no");
    host.Stop();
    rig.Release(tag);
}

// The host picture changes size while the filter streams (what OpenLocal refuses to do with a reader).
static void Resize(const std::string& tag)
{
    if (MappingExists()) D(tag + ".channel_before", "LEFT OVER");
    Host host;
    host.Start(640, 360, 7);
    Rig rig;
    if (FAILED(rig.Create(tag, kNV12))) return;
    rig.SetFormat(tag, kNV12, 1280, 720, 333333);
    if (FAILED(rig.Render(tag))) { rig.Release(tag); return; }
    rig.control->Run();
    Wait(0.4);
    size_t a = rig.col.Mark();
    Wait(1.0);
    size_t b = rig.col.Mark();
    Summarize(tag + ".16x9", rig.col.Range(a, b), { ExpectedHash(7, 640, 360, rig.connW, rig.connH, kNV12) }, 1.0);
    host.ForceResize(480, 640, 8);
    Wait(0.5);
    a = rig.col.Mark();
    Wait(1.0);
    b = rig.col.Mark();
    Summarize(tag + ".portrait", rig.col.Range(a, b), { ExpectedHash(8, 480, 640, rig.connW, rig.connH, kNV12) }, 1.0);
    host.ForceResize(1280, 720, 9);
    Wait(0.5);
    a = rig.col.Mark();
    Wait(1.0);
    b = rig.col.Mark();
    Summarize(tag + ".same_size", rig.col.Range(a, b), { ExpectedHash(9, 1280, 720, rig.connW, rig.connH, kNV12) }, 1.0);
    host.Stop();
    rig.Release(tag);
}

// Stop / pause / run transitions.
static void States(const std::string& tag)
{
    if (MappingExists()) D(tag + ".channel_before", "LEFT OVER");
    Host host;
    host.Start(640, 480, 11);
    Rig rig;
    if (FAILED(rig.Create(tag, kNV12))) return;
    if (FAILED(rig.Render(tag))) { rig.Release(tag); return; }
    const std::set<uint64_t> exp{ ExpectedHash(11, 640, 480, rig.connW, rig.connH, kNV12) };
    auto st = [&](const char* label) {
        OAFilterState s = 99;
        const HRESULT hr = rig.control->GetState(100, &s);
        FILTER_STATE fs = State_Running;
        const HRESULT hf = rig.camera->GetState(0, &fs);
        { const HRESULT h__ = hr; D(tag + ".state_after_" + label, Hr(h__) + Fmt(" graph=%d | filter %s %d readerActive=%ld", s, Hr(hf).c_str(), fs, host.RawReaderActive())); }
    };
    st("render");
    Wait(0.5);
    D(tag + ".frames_while_stopped", Fmt("%zu", rig.col.Mark()));
    D(tag + ".pause", Hr(rig.control->Pause()));
    st("pause");
    Wait(0.3);
    size_t a = rig.col.Mark();
    Wait(1.0);
    size_t b = rig.col.Mark();
    Summarize(tag + ".paused", rig.col.Range(a, b), exp, 1.0);
    D(tag + ".run", Hr(rig.control->Run()));
    st("run");
    Wait(0.3);
    a = rig.col.Mark();
    Wait(1.0);
    b = rig.col.Mark();
    Summarize(tag + ".running", rig.col.Range(a, b), exp, 1.0);
    D(tag + ".pause2", Hr(rig.control->Pause()));
    st("pause2");
    Wait(0.3);
    a = rig.col.Mark();
    Wait(0.7);
    b = rig.col.Mark();
    Summarize(tag + ".paused2", rig.col.Range(a, b), exp, 0.7);
    D(tag + ".stop", Hr(rig.control->Stop()));
    st("stop");
    Wait(0.4);
    a = rig.col.Mark();
    Wait(0.8);
    b = rig.col.Mark();
    D(tag + ".frames_after_stop", Fmt("%zu", b - a));
    D(tag + ".stop2", Hr(rig.control->Stop()));
    D(tag + ".run_from_stopped", Hr(rig.control->Run()));
    st("run_from_stopped");
    Wait(0.6);
    a = rig.col.Mark();
    Wait(0.7);
    b = rig.col.Mark();
    auto after = rig.col.Range(a, b);
    Summarize(tag + ".rerun", after, exp, 0.7);
    // the first sample of the restarted stream is a discontinuity (a new worker)
    auto whole = rig.col.Range(0, rig.col.Mark());
    int discs = 0;
    for (auto& r : whole) discs += r.disc;
    D(tag + ".discontinuities_total", Fmt("%d (streams started: pause, run-from-stopped; the run after pause continues)", discs));
    // sync source after the graph ran
    ComPtr<IReferenceClock> clk;
    { const HRESULT h__ = rig.camera->GetSyncSource(&clk); D(tag + ".syncsource", Hr(h__) + (clk ? " clock" : " none")); }
    rig.control->Stop();
    D(tag + ".readeractive_after_stop", Fmt("%ld", host.RawReaderActive()));
    host.Stop();
    rig.Release(tag);
}

// Pin connection rules, SetFormat while connected, reconnect.
static void Connections(const std::string& tag)
{
    if (MappingExists()) D(tag + ".channel_before", "LEFT OVER");
    Host host;
    host.Start(640, 480, 13);
    Rig rig;
    if (FAILED(rig.Create(tag, kNV12))) return;
    if (FAILED(rig.Render(tag))) { rig.Release(tag); return; }
    ComPtr<IPin> out;
    rig.camera->FindPin(L"Capture", &out);
    ComPtr<IPin> peer;
    { const HRESULT h__ = out->ConnectedTo(&peer); D(tag + ".connectedto", Hr(h__) + (peer ? " peer" : " none")); }
    AM_MEDIA_TYPE cmt{};
    { const HRESULT h__ = out->ConnectionMediaType(&cmt); D(tag + ".connectionmediatype", Hr(h__) + " " + Mt(cmt)); }
    CoTaskMemFree(cmt.pbFormat);
    ComPtr<IPin> in;
    rig.grabberFilter->FindPin(L"In", &in);
    D(tag + ".connect_again", Hr(out->Connect(in.Get(), nullptr)));
    D(tag + ".querypininfo_filter_refcount_stable", [&] {
        PIN_INFO pi{};
        out->QueryPinInfo(&pi);
        const bool ok = pi.pFilter != nullptr;
        if (pi.pFilter) pi.pFilter->Release();
        return std::string(ok ? "ok" : "null");
    }());
    ComPtr<IAMStreamConfig> cfg;
    out.As(&cfg);
    // SetFormat while connected and stopped: the graph reconnects with the new type
    rig.SetFormat(tag + ".reconnect", kNV12, 960, 540, 400000);
    rig.ReadConnected(tag + ".after_setformat");
    {
        AM_MEDIA_TYPE m{};
        { const HRESULT h__ = out->ConnectionMediaType(&m); D(tag + ".connectionmediatype_after", Hr(h__) + " " + Mt(m)); }
        CoTaskMemFree(m.pbFormat);
    }
    D(tag + ".pause", Hr(rig.control->Pause()));
    D(tag + ".disconnect_while_paused", Hr(out->Disconnect()));
    rig.SetFormat(tag + ".while_paused", kNV12, 1280, 720, 333333);
    D(tag + ".setformat_unsupported_while_paused_note", "see .while_paused.set_format");
    D(tag + ".stop", Hr(rig.control->Stop()));
    D(tag + ".disconnect_pin_directly", Hr(out->Disconnect()));
    D(tag + ".connectedto_after_disconnect", [&] { ComPtr<IPin> p; return Hr(out->ConnectedTo(&p)) + (p ? " peer" : " none"); }());
    D(tag + ".disconnect_again", Hr(out->Disconnect()));
    // the grabber still thinks it is connected: let the graph tidy up, then reconnect from scratch
    in->Disconnect();
    D(tag + ".reconnect_direct", Hr(out->Connect(in.Get(), nullptr)));
    rig.ReadConnected(tag + ".direct");
    D(tag + ".run_again", Hr(rig.control->Run()));
    Wait(0.8);
    const size_t a = rig.col.Mark();
    Wait(0.8);
    const size_t b = rig.col.Mark();
    Summarize(tag + ".after_reconnect", rig.col.Range(a, b), { ExpectedHash(13, 640, 480, rig.connW, rig.connH, kNV12) }, 0.8);
    rig.control->Stop();
    host.Stop();
    out.Reset();
    peer.Reset();
    in.Reset();
    cfg.Reset();
    rig.Release(tag);
}

int wmain(int argc, wchar_t** argv)
{
    if (argc < 4) { fprintf(stderr, "usage: dshow_driver <dll> <mappingName> <eventName> [fast]\n"); return 2; }
    g_mapName = argv[2];
    g_evtName = argv[3];
    const bool fast = argc > 4 && std::wstring(argv[4]) == L"fast";
    const bool cpuOnly = argc > 4 && std::wstring(argv[4]) == L"cpu"; // long steady runs: CPU use of the filter
    CoInitializeEx(nullptr, COINIT_MULTITHREADED);
    // the Rust test-names build reads the suffix from the environment: run.ps1 sets it before starting us

    HMODULE dll = LoadLibraryW(argv[1]);
    if (!dll) { fprintf(stderr, "LoadLibrary failed %lu\n", GetLastError()); return 3; }
    g_getClassObject = reinterpret_cast<GetClassObjectFn>(GetProcAddress(dll, "DllGetClassObject"));
    g_canUnload = reinterpret_cast<CanUnloadFn>(GetProcAddress(dll, "DllCanUnloadNow"));
    if (!g_getClassObject || !g_canUnload) { fprintf(stderr, "exports missing\n"); return 4; }
    D("exports.DllRegisterServer", GetProcAddress(dll, "DllRegisterServer") ? "present" : "missing");
    D("exports.DllUnregisterServer", GetProcAddress(dll, "DllUnregisterServer") ? "present" : "missing");
    D("exports.DllMain", GetProcAddress(dll, "DllMain") ? "present" : "missing");
    if (MappingExists()) { fprintf(stderr, "test channel exists already, refusing\n"); return 5; }

    if (cpuOnly)
    {
        { StreamCfg c; c.tag = "cpu.native640x480"; c.seconds = 8; StreamOne(c); }
        { StreamCfg c; c.tag = "cpu.fit720p"; c.setFormat = true; c.fmtW = 1280; c.fmtH = 720; c.seconds = 8; StreamOne(c); }
        { StreamCfg c; c.tag = "cpu.native1080p"; c.hostW = 1920; c.hostH = 1080; c.pattern = 21; c.setFormat = true; c.fmtW = 1920; c.fmtH = 1080; c.seconds = 8; StreamOne(c); }
        { StreamCfg c; c.tag = "cpu.yuy2_1080p"; c.fourcc = kYUY2; c.hostW = 1920; c.hostH = 1080; c.pattern = 21; c.setFormat = true; c.fmtW = 1920; c.fmtH = 1080; c.seconds = 8; StreamOne(c); }
        return 0;
    }
    StaticApi("");

    const uint32_t fourccs[] = { kNV12, kYUY2, kI420 };
    for (uint32_t fc : fourccs)
    {
        const std::string f = Fourcc(fc);
        { StreamCfg c; c.tag = "s.native." + f; c.fourcc = fc; c.seconds = fast ? 0.8 : 1.5; StreamOne(c); }
        { StreamCfg c; c.tag = "s.pillarbox." + f; c.fourcc = fc; c.setFormat = true; c.fmtW = 1280; c.fmtH = 720; c.seconds = fast ? 0.8 : 1.5; StreamOne(c); }
        { StreamCfg c; c.tag = "s.letterbox20fps." + f; c.fourcc = fc; c.hostW = 1280; c.hostH = 720; c.setFormat = true; c.fmtW = 640; c.fmtH = 480; c.interval = 500000; c.seconds = fast ? 0.8 : 1.5; StreamOne(c); }
    }
    { StreamCfg c; c.tag = "s.rate60"; c.setFormat = true; c.fmtW = 640; c.fmtH = 480; c.interval = 166667; c.seconds = 1.5; StreamOne(c); }
    { StreamCfg c; c.tag = "s.big1080"; c.hostW = 1920; c.hostH = 1080; c.pattern = 21; c.setFormat = true; c.fmtW = 1920; c.fmtH = 1080; c.seconds = 1.5; StreamOne(c); }
    NoSignal("s.nosignal.NV12", kNV12);
    NoSignal("s.nosignal.YUY2", kYUY2);
    NoSignal("s.nosignal.I420", kI420);
    if (!fast)
    {
        Stale("s.stale");
        Resize("s.resize");
    }
    States("s.states");
    Connections("s.connections");
    D("end", "ok");
    return 0;
}
