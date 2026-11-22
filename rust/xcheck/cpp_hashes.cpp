// Prints one line per case: "<name> <fnv1a-64 of the output>" (or text for geometry). The Rust
// side (core/examples/hashes.rs) prints the same lines; xcheck/run.ps1 diffs them.
#include "../../src/engine/ImageSettings.h"
#include "../../src/engine/SkinSmooth.h"
#include "../../src/engine/Json.h"

#include <cstdio>
#include <string>
#include <vector>

static uint64_t Fnv(const void* p, size_t n, uint64_t h = 1469598103934665603ull)
{
    const uint8_t* b = static_cast<const uint8_t*>(p);
    for (size_t i = 0; i < n; ++i) { h ^= b[i]; h *= 1099511628211ull; }
    return h;
}
static void Out(const std::string& name, uint64_t h) { printf("%s %016llx\n", name.c_str(), (unsigned long long)h); }
static void OutV(const std::string& name, const std::vector<uint8_t>& v) { Out(name, Fnv(v.data(), v.size())); }

static uint32_t g_seed = 1;
static uint8_t Rnd() { g_seed = g_seed * 1664525u + 1013904223u; return static_cast<uint8_t>(g_seed >> 24); }
static std::vector<uint8_t> Noise(size_t n, uint32_t seed) { g_seed = seed; std::vector<uint8_t> v(n); for (auto& x : v) x = Rnd(); return v; }
static size_t Bytes(uint32_t w, uint32_t h) { return (size_t)w * h * 3 / 2; }

static std::string G(double v) { char b[64]; snprintf(b, sizeof b, "%.9g", v); return b; }
static std::string Layout(const FrameLayout& l)
{
    return std::to_string(l.orientedWidth) + "x" + std::to_string(l.orientedHeight) + " out " + std::to_string(l.outWidth) + "x" + std::to_string(l.outHeight) +
           " src " + G(l.source.x) + "," + G(l.source.y) + "," + G(l.source.w) + "," + G(l.source.h) + " o" + (l.needsOrient ? "1" : "0") + " r" + (l.needsResample ? "1" : "0") +
           " pic " + std::to_string(l.pictureWidth) + "x" + std::to_string(l.pictureHeight) + " fit" + (l.fitted ? "1" : "0") +
           " dest " + std::to_string(l.destX) + "," + std::to_string(l.destY) + "," + std::to_string(l.destWidth) + "," + std::to_string(l.destHeight);
}

int main()
{
    // adjust
    {
        const uint32_t w = 66, h = 36;
        const auto src = Noise(Bytes(w, h), 7);
        const int vals[] = { -100, -37, 0, 41, 100 };
        for (int b : vals) for (int c : vals) for (int s : vals) for (int t : { -100, 0, 63 })
        {
            Nv12ColorAdjust a; a.brightness = b; a.contrast = c; a.saturation = s; a.temperature = t;
            std::vector<uint8_t> dst(src.size());
            Nv12Adjust(src.data(), w, dst.data(), w, h, a);
            OutV("adjust " + std::to_string(b) + " " + std::to_string(c) + " " + std::to_string(s) + " " + std::to_string(t), dst);
        }
    }
    // orient
    for (uint32_t w : { 64u, 66u }) for (int rot : { 0, 90, 180, 270 }) for (int m = 0; m < 2; ++m) for (int f = 0; f < 2; ++f)
    {
        const uint32_t h = 36;
        const long pitch = w + 6;
        auto src = Noise((size_t)pitch * h * 3 / 2, 11);
        Nv12Orientation o; o.rotation = rot; o.mirror = m; o.flip = f;
        std::vector<uint8_t> dst(Bytes(w, h));
        Nv12Orient(src.data(), pitch, w, h, dst.data(), o);
        OutV("orient " + std::to_string(w) + " " + std::to_string(rot) + " " + std::to_string(m) + std::to_string(f), dst);
    }
    // bilinear crop-scale
    {
        const uint32_t sw = 96, sh = 54;
        const long pitch = 100;
        const auto src = Noise((size_t)pitch * sh * 3 / 2, 21);
        const Nv12Rect rects[] = { { 0, 0, 96, 54 }, { 4.5, 2, 80, 45 }, { 10.25, 6.75, 48, 27 }, { 0, 0, 32, 18 }, { 50, 20, 46, 34 } };
        const uint32_t sizes[][2] = { { 96, 54 }, { 40, 22 }, { 160, 90 }, { 2, 2 }, { 64, 36 } };
        int i = 0;
        for (const auto& r : rects) for (const auto& s : sizes)
        {
            std::vector<uint8_t> dst(Bytes(s[0], s[1]));
            Nv12CropScaleBilinear(src.data(), pitch, sw, sh, r, dst.data(), s[0], s[1]);
            OutV("bilinear " + std::to_string(i++), dst);
        }
    }
    // bars, scale, fit
    {
        std::vector<uint8_t> f(Bytes(64, 36), 77);
        Nv12FillBars(f.data(), 64, 36, 8, 4, 20, 10);
        OutV("bars", f);
        const uint32_t pairs[][4] = { { 96, 54, 40, 22 }, { 40, 22, 96, 54 }, { 64, 36, 64, 36 }, { 108, 192, 192, 108 }, { 192, 108, 108, 192 }, { 66, 34, 100, 50 }, { 2, 200, 192, 108 } };
        int i = 0;
        for (const auto& p : pairs)
        {
            const long sp = p[0] + 4;
            const auto src = Noise((size_t)sp * p[1] * 3 / 2, 31 + i);
            std::vector<uint8_t> a(Bytes(p[2], p[3])), b(Bytes(p[2], p[3]), 0xAB);
            Nv12Scale(src.data(), sp, p[0], p[1], a.data(), p[2], p[2], p[3]);
            Nv12FitInto(src.data(), sp, p[0], p[1], b.data(), p[2], p[2], p[3]);
            OutV("scale " + std::to_string(i), a);
            OutV("fit " + std::to_string(i), b);
            ++i;
        }
    }
    // bgra <-> nv12
    {
        const uint32_t w = 34, h = 18;
        const size_t pitch = w * 4 + 8;
        const auto bgra = Noise(pitch * h, 41);
        std::vector<uint8_t> nv(Bytes(w, h));
        BgraToNv12(bgra.data(), pitch, w, h, nv.data(), nv.data() + w * h, w);
        OutV("bgra2nv12", nv);
        for (auto sz : { std::pair<uint32_t, uint32_t>{ 34, 18 }, { 17, 9 }, { 70, 37 } })
        {
            const auto src = Noise((size_t)(w + 6) * h * 3 / 2, 51);
            std::vector<uint32_t> out((size_t)sz.first * sz.second);
            Nv12ToBgra(src.data(), src.data() + (size_t)(w + 6) * h, w + 6, w, h, out.data(), sz.first, sz.second);
            Out("nv12tobgra " + std::to_string(sz.first), Fnv(out.data(), out.size() * 4));
        }
    }
    // layouts and plans
    {
        const uint32_t cams[][2] = { { 1920, 1080 }, { 1080, 1920 }, { 3840, 2160 }, { 1440, 1080 }, { 2, 200 }, { 641, 481 } };
        const uint32_t fits[][2] = { { 0, 0 }, { 1920, 1080 }, { 1080, 1440 }, { 1001, 601 }, { 1280, 720 } };
        int n = 0;
        for (const auto& c : cams) for (int rot : { 0, 90, 270 }) for (int a = 0; a <= 4; ++a) for (double z : { 1.0, 1.7, 4.5 }) for (const auto& f : fits)
        {
            ImageSettings s; s.orientation.rotation = rot; s.orientation.mirror = (n % 3) == 0; s.aspect = (CropAspect)a; s.zoom = z; s.centerX = 0.2 + 0.1 * (n % 7); s.centerY = 0.9 - 0.1 * (n % 5);
            printf("layout %d %s\n", n, Layout(ComputeLayout(c[0], c[1], s, f[0], f[1])).c_str());
            ++n;
        }
        n = 0;
        for (int m = 0; m < 4; ++m) for (uint32_t cw : { 0u, 1920u, 1280u }) for (bool holds : { false, true })
        {
            const OutputPlan p = PlanOutput((OutputSize)m, 1920, 1080, cw, cw ? cw * 9 / 16 : 0, holds);
            printf("plan %d %u %u %d\n", n++, p.width, p.height, p.rebuild);
        }
    }
    // skin smoothing
    for (int strength : { 1, 40, 100 })
    {
        const uint32_t w = 200, h = 160;
        auto y = Noise((size_t)w * h, 61);
        SkinScratch sc;
        SmoothFace(y.data(), w, w, h, FaceRect{ 0.3, 0.2, 0.3, 0.45 }, strength, sc);
        OutV("skin " + std::to_string(strength), y);
    }
    // auto framing trajectory (printed, 9 digits)
    {
        const FramingGeometry g{ 3840, 2160, 3840, 2160 };
        AutoFramer f;
        double t = 0, next = 0;
        for (int i = 1; i <= 600; ++i)
        {
            t = i / 30.0;
            if (t >= next)
            {
                std::vector<FaceRect> faces;
                if (t < 6) faces.push_back({ 0.45, 0.3, 0.1, 0.2 });
                else if (t < 12) { faces.push_back({ 0.2, 0.3, 0.1, 0.2 }); faces.push_back({ 0.6, 0.3, 0.09, 0.18 }); }
                f.OnFaces(faces, t);
                next += 0.2;
            }
            const FramingView v = f.Step(g, t);
            if (i % 30 == 0) printf("frame %d %s %s %s\n", i, G(v.zoom).c_str(), G(v.centerX).c_str(), G(v.centerY).c_str());
        }
        uint32_t dw, dh;
        DetectionSize(880, 1920, 640, dw, dh);
        printf("detect %u %u\n", dw, dh);
        const auto src = Noise((size_t)(100 + 8) * 60, 71);
        std::vector<uint8_t> dst(37 * 23);
        DownscaleLuma(src.data(), 108, 100, 60, dst.data(), 37, 23);
        OutV("downscale", dst);
    }
    // json
    {
        const char* docs[] = { "{\"a\":[1,2.5,-3e2,{\"b\":null}],\"s\":\"x\\u00e9\\ud83d\\ude00\\n\",\"t\":true}", "[]", "{}", "[0.1,1e21,123456789.5,0.000012345]" };
        for (const char* d : docs) printf("json %s\n", json::Value::Parse(d).Dump().c_str());
        for (double v : { 0.5, 1.0 / 3.0, 1e20, 1.5e-7, 123456789.5, -0.25, 1e15, 999999999999999.0, 0.1 + 0.2 }) printf("num %s\n", json::Value(v).Dump().c_str());
    }
    return 0;
}
