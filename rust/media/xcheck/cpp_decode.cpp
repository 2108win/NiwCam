// The C++ VideoDecoder on a gen_stream file. Prints one line per decoded frame (hash of the visible
// NV12 pixels) and the HRESULT of every step. The Rust example `decode` prints the same lines.
#include "../../../src/engine/VideoDecoder.cpp"
#include <cstdio>
#include <string>

#pragma comment(lib, "mfplat.lib")
#pragma comment(lib, "mfuuid.lib")
#pragma comment(lib, "mf.lib")
#pragma comment(lib, "ole32.lib")
#pragma comment(lib, "strmiids.lib")
#pragma comment(lib, "d3d11.lib")
#pragma comment(lib, "dxguid.lib")

static uint64_t Fnv(const void* p, size_t n, uint64_t h)
{
    const uint8_t* b = static_cast<const uint8_t*>(p);
    for (size_t i = 0; i < n; ++i) { h ^= b[i]; h *= 1099511628211ull; }
    return h;
}

int main(int argc, char** argv)
{
    if (argc < 5) return 2;
    const bool hevc = std::string(argv[1]) == "hevc";
    const uint32_t w = atoi(argv[2]), h = atoi(argv[3]);
    CoInitializeEx(nullptr, COINIT_MULTITHREADED);
    MFStartup(MF_VERSION);
    VideoDecoder dec;
    uint8_t junk[8] = { 1, 2, 3, 4, 5, 6, 7, 8 };
    printf("decode-before-init %08x\n", (unsigned)dec.Decode(junk, sizeof junk, 0, [](const uint8_t*, long, uint32_t, uint32_t) {}));
    printf("init %08x\n", (unsigned)dec.Init(hevc, w, h));
    FILE* f = nullptr;
    fopen_s(&f, argv[4], "rb");
    uint32_t size = 0;
    int64_t t = 0;
    uint8_t key = 0;
    uint32_t au = 0, frames = 0;
    while (fread(&size, 4, 1, f) == 1)
    {
        fread(&t, 8, 1, f);
        fread(&key, 1, 1, f);
        std::vector<uint8_t> data(size);
        fread(data.data(), 1, size, f);
        const HRESULT hr = dec.Decode(data.data(), data.size(), t, [&](const uint8_t* nv12, long pitch, uint32_t fw, uint32_t fh) {
            uint64_t hash = 1469598103934665603ull;
            for (uint32_t r = 0; r < fh; ++r) hash = Fnv(nv12 + (size_t)r * pitch, fw, hash);
            const uint8_t* uv = nv12 + (size_t)pitch * fh;
            for (uint32_t r = 0; r < fh / 2; ++r) hash = Fnv(uv + (size_t)r * pitch, fw, hash);
            printf("frame %u %ux%u pitch %ld %016llx\n", frames++, fw, fh, pitch, (unsigned long long)hash);
        });
        printf("au %u size %u key %u hr %08x\n", au++, size, key, (unsigned)hr);
    }
    fclose(f);
    // garbage into a running decoder
    uint8_t garbage[64];
    uint32_t s = 12345;
    for (auto& g : garbage) { s = s * 1664525u + 1013904223u; g = (uint8_t)(s >> 24); }
    printf("garbage %08x\n", (unsigned)dec.Decode(garbage, sizeof garbage, 99, [](const uint8_t*, long, uint32_t, uint32_t) {}));
    dec.Reset();
    printf("after-reset %08x\n", (unsigned)dec.Decode(junk, sizeof junk, 0, [](const uint8_t*, long, uint32_t, uint32_t) {}));
    return 0;
}
