// C++ twin of core/examples/bench.rs.
#include "../../src/engine/ImageSettings.h"
#include <chrono>
#include <cstdio>
#include <string>
#include <vector>
template <class F> void Time(const std::string& name, int n, F f)
{
    for (int i = 0; i < 3; ++i) f();
    auto t = std::chrono::steady_clock::now();
    for (int i = 0; i < n; ++i) f();
    printf("%s %.3f ms\n", name.c_str(), std::chrono::duration<double, std::milli>(std::chrono::steady_clock::now() - t).count() / n);
}
int main()
{
    for (auto [w, h] : { std::pair<uint32_t, uint32_t>{ 1280, 720 }, { 1920, 1080 } })
    {
        std::vector<uint8_t> src((size_t)w * h * 3 / 2);
        for (size_t i = 0; i < src.size(); ++i) src[i] = (uint8_t)(i * 7 + i / 13);
        const int n = w > 1300 ? 100 : 200;
        std::vector<uint32_t> bgra((size_t)w * h), small((size_t)(w / 2) * (h / 2));
        const std::string p = std::to_string(w) + "x" + std::to_string(h) + " ";
        const uint8_t* y = src.data(); const uint8_t* uv = y + (size_t)w * h;
        Time(p + "nv12_to_bgra 1:1", n, [&] { Nv12ToBgra(y, uv, w, w, h, bgra.data(), w, h); });
        Time(p + "nv12_to_bgra half", n, [&] { Nv12ToBgra(y, uv, w, w, h, small.data(), w / 2, h / 2); });
        std::vector<uint8_t> dst(src.size());
        Time(p + "nv12_scale same size", n, [&] { Nv12Scale(src.data(), w, w, h, dst.data(), w, w, h); });
        Nv12ColorAdjust a; a.brightness = 10; a.contrast = 5; a.saturation = 20; a.temperature = 8;
        Time(p + "nv12_adjust", n, [&] { Nv12Adjust(src.data(), w, dst.data(), w, h, a); });
        Nv12Rect r{ 100, 60, w - 200.0, h - 120.0 };
        Time(p + "bilinear zoom", n, [&] { Nv12CropScaleBilinear(src.data(), w, w, h, r, dst.data(), w, h); });
        Nv12Orientation o; o.rotation = 90;
        Time(p + "orient 90", n, [&] { Nv12Orient(src.data(), w, w, h, dst.data(), o); });
        std::vector<uint8_t> bg((size_t)w * 4 * h);
        Time(p + "bgra_to_nv12", n, [&] { BgraToNv12(bg.data(), (size_t)w * 4, w, h, dst.data(), dst.data() + (size_t)w * h, w); });
    }
}
