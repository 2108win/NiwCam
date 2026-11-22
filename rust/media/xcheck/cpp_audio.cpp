// The C++ AudioOut on the wav: target (no sound is made), the device listing, and the "device not
// found" path. Rust twin: examples/audio.rs. cpp_audio <output dir> [cable]
//   "cable": also pushes a second of SILENCE into a virtual cable device ("CABLE Input"), if there is one.
#include "../../../src/engine/AudioOut.cpp"

#include <cmath>
#include <cstdio>
#include <string>

#pragma comment(lib, "ole32.lib")

static std::string U8(const std::wstring& w)
{
    if (w.empty()) return {};
    const int n = WideCharToMultiByte(CP_UTF8, 0, w.c_str(), (int)w.size(), nullptr, 0, nullptr, nullptr);
    std::string s(n, '\0');
    WideCharToMultiByte(CP_UTF8, 0, w.c_str(), (int)w.size(), s.data(), n, nullptr, nullptr);
    return s;
}

static std::vector<int16_t> Sine(uint32_t rate, uint32_t channels, double seconds, double freq, double phase0)
{
    const size_t frames = static_cast<size_t>(rate * seconds);
    std::vector<int16_t> v(frames * channels);
    for (size_t i = 0; i < frames; ++i)
        for (uint32_t c = 0; c < channels; ++c)
            v[i * channels + c] = static_cast<int16_t>(std::lround(12000.0 * std::sin(phase0 + 2 * 3.14159265358979323846 * freq * i / rate) * (c ? 0.5 : 1.0)));
    return v;
}

static uint64_t FileHash(const std::wstring& path, long long* size)
{
    FILE* f = nullptr;
    if (_wfopen_s(&f, path.c_str(), L"rb") != 0 || !f) { *size = -1; return 0; }
    uint64_t h = 1469598103934665603ull;
    long long n = 0;
    int c;
    while ((c = fgetc(f)) != EOF) { h ^= (uint8_t)c; h *= 1099511628211ull; ++n; }
    fclose(f);
    *size = n;
    return h;
}

int main(int argc, char** argv)
{
    if (argc < 2) return 2;
    const std::wstring dir = std::wstring(argv[1], argv[1] + strlen(argv[1])) + L"\\";
    CoInitializeEx(nullptr, COINIT_MULTITHREADED);

    // A: 48 kHz mono, 1 s of 440 Hz in 10 ms pushes.
    {
        AudioOut a(L"wav:" + dir + L"a.wav");
        const auto pcm = Sine(48000, 1, 1.0, 440.0, 0);
        printf("A receiving-before %d problem [%s]\n", a.Receiving() ? 1 : 0, U8(a.Problem()).c_str());
        for (size_t i = 0; i < pcm.size(); i += 480) a.Push(reinterpret_cast<const uint8_t*>(pcm.data() + i), std::min<size_t>(480, pcm.size() - i) * 2, 48000, 1);
        printf("A receiving-after %d problem [%s]\n", a.Receiving() ? 1 : 0, U8(a.Problem()).c_str());
    }
    // B: pushes the writer must ignore, an odd byte count, a format change in the middle.
    {
        AudioOut b(L"wav:" + dir + L"b.wav");
        const auto s1 = Sine(44100, 2, 0.5, 1000.0, 0);
        const auto s2 = Sine(48000, 1, 0.5, 300.0, 0.5);
        uint8_t one = 7;
        b.Push(&one, 1, 44100, 2);                                                                 // under 2 bytes: ignored
        b.Push(reinterpret_cast<const uint8_t*>(s1.data()), 100, 0, 2);                            // no rate: ignored
        b.Push(reinterpret_cast<const uint8_t*>(s1.data()), 100, 44100, 0);                        // no channels: ignored
        b.Push(reinterpret_cast<const uint8_t*>(s1.data()), 1001, 44100, 2);                       // odd: 500 samples used
        b.Push(reinterpret_cast<const uint8_t*>(s1.data()) + 1001, s1.size() * 2 - 1001, 44100, 2);
        b.Push(reinterpret_cast<const uint8_t*>(s2.data()), s2.size() * 2, 48000, 1);              // new format: header takes the last
        printf("B problem [%s]\n", U8(b.Problem()).c_str());
    }
    // C: a file that cannot be created: nothing crashes, nothing is written.
    {
        AudioOut c(L"wav:" + dir + L"no-such-dir\\c.wav");
        const auto pcm = Sine(48000, 1, 0.1, 440.0, 0);
        c.Push(reinterpret_cast<const uint8_t*>(pcm.data()), pcm.size() * 2, 48000, 1);
        printf("C receiving %d problem [%s]\n", c.Receiving() ? 1 : 0, U8(c.Problem()).c_str());
    }
    // D: never pushed: no file.
    {
        AudioOut d(L"wav:" + dir + L"d.wav");
        printf("D receiving %d\n", d.Receiving() ? 1 : 0);
    }
    for (const wchar_t* n : { L"a.wav", L"b.wav", L"c.wav", L"d.wav" })
    {
        long long size = 0;
        const uint64_t h = FileHash(dir + n, &size);
        printf("file %s size %lld hash %016llx\n", U8(n).c_str(), size, (unsigned long long)h);
    }

    // The device listing.
    const auto outs = AudioOut::Outputs();
    printf("outputs %zu\n", outs.size());
    for (const auto& [id, name] : outs) printf("output id=[%s] name=[%s]\n", U8(id).c_str(), U8(name).c_str());

    // A device that does not exist: the render thread reports it (nothing plays).
    {
        AudioOut e(L"{0.0.0.00000000}.{00000000-0000-0000-0000-000000000000}");
        const auto pcm = Sine(48000, 1, 0.1, 440.0, 0);
        e.Push(reinterpret_cast<const uint8_t*>(pcm.data()), pcm.size() * 2, 48000, 1);
        for (int i = 0; i < 100 && e.Problem().empty(); ++i) Sleep(20);
        printf("E problem [%s]\n", U8(e.Problem()).c_str());
    }
    {
        AudioOut e(L"not-an-endpoint-id");
        const auto pcm = Sine(48000, 1, 0.1, 440.0, 0);
        e.Push(reinterpret_cast<const uint8_t*>(pcm.data()), pcm.size() * 2, 48000, 1);
        for (int i = 0; i < 100 && e.Problem().empty(); ++i) Sleep(20);
        printf("F problem [%s]\n", U8(e.Problem()).c_str());
    }
    // Silence into a virtual cable, when asked and present.
    if (argc > 2)
    {
        std::wstring cable;
        const std::wstring prefix = argc > 3 ? std::wstring(argv[3], argv[3] + strlen(argv[3])) : L"CABLE Input";
        for (const auto& [id, name] : outs) if (name.rfind(prefix, 0) == 0) cable = id;
        if (cable.empty()) printf("G no cable device\n");
        else
        {
            AudioOut g(cable);
            std::vector<int16_t> silence(480 * 2, 0); // 10 ms stereo
            for (int i = 0; i < 100; ++i) { g.Push(reinterpret_cast<const uint8_t*>(silence.data()), silence.size() * 2, 48000, 2); Sleep(10); }
            printf("G cable problem [%s] receiving %d\n", U8(g.Problem()).c_str(), g.Receiving() ? 1 : 0);
        }
    }
    return 0;
}
