// The C++ Recorder (MP4, microphone off) and still::SavePng on a generated moving picture.
// Rust twin: examples/record.rs. cpp_record <output dir>
// Writes <dir>\rec.mp4 (3 s, 1280x720 at 30), <dir>\snap.png, <dir>\snap-bad.png (never created).
#include "../../../src/engine/Recorder.cpp"
#include "../../../src/engine/StillPicture.cpp"
#include "pattern.h"

#include <cstdio>
#include <string>

#pragma comment(lib, "mfplat.lib")
#pragma comment(lib, "mfuuid.lib")
#pragma comment(lib, "mf.lib")
#pragma comment(lib, "mfreadwrite.lib")
#pragma comment(lib, "ole32.lib")
#pragma comment(lib, "d2d1.lib")
#pragma comment(lib, "dwrite.lib")
#pragma comment(lib, "windowscodecs.lib")

static std::wstring W(const char* s) { return std::wstring(s, s + strlen(s)); }

int main(int argc, char** argv)
{
    if (argc < 2) return 2;
    const std::wstring dir = W(argv[1]) + L"\\";
    CoInitializeEx(nullptr, COINIT_MULTITHREADED);
    MFStartup(MF_VERSION);

    {
        Recorder bad;
        printf("stop-idle %08x\n", (unsigned)bad.Stop());
        printf("start-too-small %08x\n", (unsigned)bad.Start(dir + L"x.mp4", 10, 10, 30, false));
        printf("start-bad-path %08x\n", (unsigned)bad.Start(dir + L"no-such-dir\\x.mp4", 640, 480, 30, false));
        printf("idle active %d audio %d elapsed %lld problem [%ls]\n", bad.Active() ? 1 : 0, bad.HasAudio() ? 1 : 0, (long long)bad.ElapsedMs(), bad.AudioProblem().c_str());
        std::vector<uint8_t> f;
        DrawPattern(f, 640, 480, 0);
        bad.Push(f.data(), 640, 640, 480); // inactive: ignored
    }

    Recorder r;
    printf("start %08x\n", (unsigned)r.Start(dir + L"rec.mp4", 1280, 721 /* odd: rounded down to 720 */, 30, false));
    printf("active %d audio %d problem [%ls]\n", r.Active() ? 1 : 0, r.HasAudio() ? 1 : 0, r.AudioProblem().c_str());
    LARGE_INTEGER freq, t0, now;
    QueryPerformanceFrequency(&freq);
    QueryPerformanceCounter(&t0);
    std::vector<uint8_t> frame;
    uint32_t pushed = 0;
    for (uint32_t n = 0;; ++n)
    {
        const double due = n * (1.0 / 30.0);
        for (;;)
        {
            QueryPerformanceCounter(&now);
            const double t = double(now.QuadPart - t0.QuadPart) / freq.QuadPart;
            if (t >= 3.0) goto done;
            if (t >= due) break;
            SwitchToThread(); // spin: the same pacing as the Rust driver, whatever the timer resolution
        }
        if (n >= 30 && n < 36)
        {
            DrawPattern(frame, 640, 480, n); // another shape: fitted with bars
            r.Push(frame.data(), 640, 640, 480);
        }
        else
        {
            DrawPattern(frame, 1280, 720, n);
            r.Push(frame.data(), 1280, 1280, 720);
        }
        ++pushed;
    }
done:
    printf("elapsed>=2900ms %d\n", r.ElapsedMs() >= 2900 ? 1 : 0);
    printf("stop %08x\n", (unsigned)r.Stop());
    printf("stop-again %08x\n", (unsigned)r.Stop());
    printf("after active %d elapsed %lld\n", r.Active() ? 1 : 0, (long long)r.ElapsedMs());
    fprintf(stderr, "pushed %u\n", pushed);

    // PNG
    std::vector<uint8_t> snap;
    DrawPattern(snap, 1280, 720, 7);
    printf("png %08x\n", (unsigned)still::SavePng(dir + L"snap.png", snap.data(), 1280, 720));
    std::vector<uint8_t> snapSmall;
    DrawPattern(snapSmall, 322, 182, 3);
    printf("png-small %08x\n", (unsigned)still::SavePng(dir + L"snap-small.png", snapSmall.data(), 322, 182));
    printf("png-tiny %08x\n", (unsigned)still::SavePng(dir + L"snap-tiny.png", snap.data(), 1, 1));
    printf("png-null %08x\n", (unsigned)still::SavePng(dir + L"snap-null.png", nullptr, 100, 100));
    printf("png-bad-path %08x\n", (unsigned)still::SavePng(dir + L"no-such-dir\\snap.png", snap.data(), 1280, 720));
    return 0;
}
