// The moving test picture shared by cpp_record.cpp and mediaprobe.cpp (examples/record.rs has the
// same integer formulas). Contiguous NV12, frame n.
#pragma once
#include <cstdint>
#include <cstring>
#include <vector>

inline void DrawPattern(std::vector<uint8_t>& nv12, uint32_t w, uint32_t h, uint32_t n)
{
    nv12.resize(static_cast<size_t>(w) * h * 3 / 2);
    uint8_t* y = nv12.data();
    uint8_t* uv = y + static_cast<size_t>(w) * h;
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
