// C++ reference driver for rust/fx: runs the real src/engine/StillPicture, Segmenter, FaceDetect
// and WindowSource code on the inputs gen_inputs.py wrote, and writes the outputs the Rust driver
// (fx/examples/fx_rust.rs) is compared with. Built by run.ps1 with the CMake flags of the engine.
//   fx_cpp still  <indir> <outdir>
//   fx_cpp blend  <indir> <outdir>
//   fx_cpp seg    <indir> <outdir> <model.onnx>
//   fx_cpp face   <indir> <outdir>
//   fx_cpp windowhost <indir> <outdir> x y seconds   (a window for another process to capture; handle in hwnd.txt)
//   fx_cpp closewatch <indir> <outdir> <hwnd>      (what Next() says when that window goes away, no message pump)
//   fx_cpp window <indir> <outdir> [x y|auto auto]   (a test window of its own, at x y: default the bottom right corner)
#include "../../../src/engine/FaceDetect.h"
#include "../../../src/engine/Segmenter.h"
#include "../../../src/engine/StillPicture.h"

#include <windows.h>
#include <objbase.h>

#include <chrono>
#include <climits>
#include <cstdio>
#include <cstdlib>
#include <fstream>
#include <sstream>
#include <string>
#include <thread>
#include <vector>

int CmdWindow(const std::string& outdir, FILE* results, int x, int y); // window_cpp.cpp
int CmdWindowHost(const std::string& outdir, int x, int y, int seconds);
int CmdCloseWatch(FILE* results, long long hwndValue, FILE* timing);

namespace
{
std::string g_in, g_out;
FILE* g_results = nullptr;

std::vector<uint8_t> ReadFile(const std::string& path)
{
    std::ifstream f(path, std::ios::binary);
    if (!f) return {};
    return std::vector<uint8_t>(std::istreambuf_iterator<char>(f), {});
}

void WriteFile(const std::string& path, const uint8_t* data, size_t n)
{
    std::ofstream f(path, std::ios::binary);
    f.write(reinterpret_cast<const char*>(data), static_cast<std::streamsize>(n));
}

std::wstring Widen(const std::string& s)
{
    if (s.empty()) return {};
    const int n = MultiByteToWideChar(CP_UTF8, 0, s.data(), static_cast<int>(s.size()), nullptr, 0);
    std::wstring w(static_cast<size_t>(n), L'\0');
    MultiByteToWideChar(CP_UTF8, 0, s.data(), static_cast<int>(s.size()), w.data(), n);
    return w;
}

std::wstring ReadText(const std::string& name)
{
    const auto b = ReadFile(g_in + "\\" + name);
    return Widen(std::string(b.begin(), b.end()));
}

uint64_t Fnv(const uint8_t* d, size_t n)
{
    uint64_t h = 1469598103934665603ull;
    for (size_t i = 0; i < n; ++i) h = (h ^ d[i]) * 1099511628211ull;
    return h;
}

void Result(const std::string& name, const std::string& what, const std::vector<uint8_t>* data)
{
    if (data) WriteFile(g_out + "\\" + name + ".nv12", data->data(), data->size());
    fprintf(g_results, "%s %s", name.c_str(), what.c_str());
    if (data) fprintf(g_results, " bytes=%zu fnv=%016llx", data->size(), static_cast<unsigned long long>(Fnv(data->data(), data->size())));
    fprintf(g_results, "\n");
}

double Ms(std::chrono::steady_clock::time_point t0)
{
    return std::chrono::duration<double, std::milli>(std::chrono::steady_clock::now() - t0).count();
}

// ---- still -------------------------------------------------------------------------------

int CmdStill()
{
    struct Size { uint32_t w, h; };
    const Size sizes[] = { { 1920, 1080 }, { 1280, 720 }, { 720, 1280 }, { 64, 36 } };
    const auto frameA = ReadFile(g_in + "\\frame_a_1920x1080.nv12");
    const auto frameC = ReadFile(g_in + "\\frame_c_640x480.nv12");
    const auto frameD = ReadFile(g_in + "\\frame_d_1366x768.nv12");
    const std::wstring shortText = ReadText("text_short.txt"), longText = ReadText("text_long.txt");
    FILE* timing = fopen((g_out + "\\timing.txt").c_str(), "w");
    for (const Size s : sizes)
    {
        const std::string tag = "_" + std::to_string(s.w) + "x" + std::to_string(s.h);
        auto run = [&](const std::string& name, const std::wstring& text, const uint8_t* bg, uint32_t bw, uint32_t bh) {
            std::vector<uint8_t> out;
            const auto t0 = std::chrono::steady_clock::now();
            const bool ok = still::RenderCard(text, s.w, s.h, bg, bw, bh, out);
            fprintf(timing, "%s %.1f ms\n", (name + tag).c_str(), Ms(t0));
            Result(name + tag, ok ? "ok" : "false", ok ? &out : nullptr);
        };
        run("card_short", shortText, nullptr, 0, 0);
        run("card_long", longText, nullptr, 0, 0);
        run("card_empty", L"", nullptr, 0, 0);
        run("blur_a", shortText, frameA.data(), 1920, 1080);
        run("blur_c_long", longText, frameC.data(), 640, 480);
        run("blur_d", shortText, frameD.data(), 1366, 768);
        run("blur_smallbw", shortText, frameA.data(), 1, 1); // the C++ quirk: neither gradient nor picture
        auto img = [&](const std::string& name, const std::string& file) {
            std::vector<uint8_t> out;
            const auto t0 = std::chrono::steady_clock::now();
            const bool ok = still::LoadImageFill(Widen(file), s.w, s.h, out);
            fprintf(timing, "%s %.1f ms\n", (name + tag).c_str(), Ms(t0));
            Result(name + tag, ok ? "ok" : "false", ok ? &out : nullptr);
        };
        img("img_png", g_in + "\\img_portrait_1000x1500.png");
        img("img_jpg", g_in + "\\img_wide_1600x900.jpg");
        img("img_bmp", g_in + "\\img_small_332x500.bmp");
        img("img_missing", g_in + "\\no_such_file.png");
        img("img_notimage", g_in + "\\not_an_image.png");
        img("img_emptypath", "");
    }
    {
        std::vector<uint8_t> out;
        Result("card_w1", still::RenderCard(shortText, 1, 1080, nullptr, 0, 0, out) ? "ok" : "false", nullptr);
    }
    // "Chụp ảnh": the frame as a PNG; the file is decoded by the compare script.
    const std::wstring png = Widen(g_out + "\\save_a.png");
    fprintf(g_results, "save_png hr=%08x\n", static_cast<unsigned>(still::SavePng(png, frameA.data(), 1920, 1080)));
    fprintf(g_results, "save_png_badpath hr=%08x\n",
            static_cast<unsigned>(still::SavePng(Widen(g_out + "\\no_such_dir\\x.png"), frameA.data(), 1920, 1080)));
    fprintf(g_results, "save_png_null hr=%08x\n", static_cast<unsigned>(still::SavePng(png, nullptr, 1920, 1080)));
    fprintf(g_results, "save_png_tiny hr=%08x\n", static_cast<unsigned>(still::SavePng(png, frameA.data(), 1, 1)));
    fclose(timing);
    return 0;
}

// ---- blend -------------------------------------------------------------------------------

int CmdBlend()
{
    struct Frame { const char* file; uint32_t w, h; };
    const Frame fa{ "frame_a_1920x1080", 1920, 1080 }, fb{ "frame_b_1280x720", 1280, 720 }, fd{ "frame_d_1366x768", 1366, 768 };
    const auto maskA = ReadFile(g_in + "\\mask_a.u8"), maskB = ReadFile(g_in + "\\mask_b.u8");
    auto run = [&](const std::string& name, const Frame& f, uint32_t padding, const std::vector<uint8_t>& mask, bool changed, int strength,
                   const char* bgImage, BackgroundScratch& scratch, bool parallel) {
        const auto src = ReadFile(g_in + "\\" + f.file + ".nv12");
        const uint32_t pitch = f.w + padding;
        // Planes with `padding` junk bytes at the end of each row (0xAB: must stay untouched).
        std::vector<uint8_t> buf(static_cast<size_t>(pitch) * f.h * 3 / 2, 0xAB);
        for (uint32_t y = 0; y < f.h; ++y) memcpy(buf.data() + static_cast<size_t>(y) * pitch, src.data() + static_cast<size_t>(y) * f.w, f.w);
        for (uint32_t y = 0; y < f.h / 2; ++y)
            memcpy(buf.data() + static_cast<size_t>(pitch) * f.h + static_cast<size_t>(y) * pitch, src.data() + static_cast<size_t>(f.w) * f.h + static_cast<size_t>(y) * f.w, f.w);
        std::vector<uint8_t> image;
        if (bgImage) image = ReadFile(g_in + "\\" + bgImage + ".nv12");
        uint8_t* y = buf.data();
        uint8_t* uv = buf.data() + static_cast<size_t>(pitch) * f.h;
        const auto t0 = std::chrono::steady_clock::now();
        BlendBackground(y, uv, static_cast<long>(pitch), f.w, f.h, mask, changed, strength, bgImage ? image.data() : nullptr, scratch, parallel);
        fprintf(stderr, "  blend %-28s %.2f ms\n", name.c_str(), Ms(t0));
        Result(name, "ok", &buf);
    };
    {
        BackgroundScratch s;
        run("seq1_a_720p_changed", fb, 0, maskA, true, 50, nullptr, s, false);
        run("seq1_b_1080p_size_changed", fa, 0, maskA, false, 50, nullptr, s, false);
        run("seq1_c_1080p_stale_mask", fa, 0, maskB, false, 50, nullptr, s, false);
        run("seq1_d_1080p_new_mask", fa, 0, maskB, true, 50, nullptr, s, false);
    }
    for (const int strength : { 0, 25, 100 })
    {
        BackgroundScratch s;
        run("blur_1080p_s" + std::to_string(strength), fa, 0, maskA, true, strength, nullptr, s, false);
    }
    {
        BackgroundScratch s;
        run("blur_1366_pitch64_s30", fd, 64, maskA, true, 30, nullptr, s, false);
    }
    {
        BackgroundScratch s;
        run("blur_1080p_s50_par", fa, 0, maskA, true, 50, nullptr, s, true);
    }
    {
        BackgroundScratch s;
        run("blur_1366_pitch64_s30_par", fd, 64, maskA, true, 30, nullptr, s, true);
    }
    {
        BackgroundScratch s;
        run("image_1080p", fa, 0, maskA, true, 50, "bgimg_1920x1080", s, false);
    }
    {
        BackgroundScratch s;
        run("image_720p_pitch32", fb, 32, maskB, true, 50, "bgimg_1280x720", s, false);
    }
    {
        BackgroundScratch s;
        run("image_1366_par", fd, 0, maskA, true, 50, "bgimg_1366x768", s, true);
    }
    {
        BackgroundScratch s;
        std::vector<uint8_t> bad(100, 255);
        run("bad_mask_size", fb, 0, bad, true, 50, nullptr, s, false);
    }
    return 0;
}

// ---- CPU cost of BlendBackground, serial and with every core ---------------------------------------------

double ProcessCpuMs()
{
    FILETIME create, exit, kernel, user;
    GetProcessTimes(GetCurrentProcess(), &create, &exit, &kernel, &user);
    const auto ms = [](const FILETIME& f) { return static_cast<double>((static_cast<uint64_t>(f.dwHighDateTime) << 32) | f.dwLowDateTime) / 10000.0; };
    return ms(kernel) + ms(user);
}

int CmdCpu()
{
    const auto mask = ReadFile(g_in + "\\mask_a.u8");
    FILE* f = fopen((g_out + "\\cpu.txt").c_str(), "w");
    for (const auto size : { std::pair<uint32_t, uint32_t>{ 1280, 720 }, { 1920, 1080 }, { 3840, 2160 } })
        for (const bool parallel : { false, true })
        {
            const uint32_t w = size.first, h = size.second;
            std::vector<uint8_t> buf(static_cast<size_t>(w) * h * 3 / 2);
            for (size_t i = 0; i < buf.size(); ++i) buf[i] = static_cast<uint8_t>(16 + (i * 7) % 200);
            BackgroundScratch scratch;
            const auto run = [&](bool changed) {
                BlendBackground(buf.data(), buf.data() + static_cast<size_t>(w) * h, static_cast<long>(w), w, h, mask, changed, 50, nullptr, scratch, parallel);
            };
            run(true);
            for (int i = 0; i < 5; ++i) run(false);
            const int n = 300;
            const double cpu0 = ProcessCpuMs();
            const auto t0 = std::chrono::steady_clock::now();
            for (int i = 0; i < n; ++i) run(false);
            const double wall = Ms(t0), cpu = ProcessCpuMs() - cpu0;
            fprintf(f, "%ux%u parallel=%d: wall %.2f ms/frame, cpu %.2f ms/frame\n", w, h, parallel ? 1 : 0, wall / n, cpu / n);
        }
    fclose(f);
    return 0;
}

// ---- segmenter (Windows ML) -----------------------------------------------------------------

int CmdSeg(const std::wstring& model)
{
    struct Frame { const char* file; uint32_t w, h; };
    const Frame frames[] = { { "face_one_1280x720", 1280, 720 }, { "face_one_720x1280", 720, 1280 }, { "face_two_1280x720", 1280, 720 },
                             { "face_none_1280x720", 1280, 720 }, { "frame_a_1920x1080", 1920, 1080 } };
    FILE* info = fopen((g_out + "\\seg_info.txt").c_str(), "w");
    {
        // No model file: the state must become "unsupported" (0).
        Segmenter s;
        s.SetModel(L"C:\\no\\such\\model.onnx");
        std::vector<uint8_t> dummy(1280 * 720 * 3 / 2, 128);
        s.Offer(dummy.data(), dummy.data() + 1280 * 720, 1280, 1280, 720, 0);
        for (int i = 0; i < 200 && s.State() == 1; ++i) std::this_thread::sleep_for(std::chrono::milliseconds(20));
        fprintf(g_results, "seg_missing_model state=%d\n", s.State());
    }
    for (const Frame& f : frames)
    {
        const auto src = ReadFile(g_in + "\\" + f.file + ".nv12");
        if (src.empty()) continue;
        Segmenter s;
        s.SetModel(model);
        std::vector<uint8_t> mask;
        // The same frame twice: the second mask is smoothed with the first (half and half), as in a run.
        for (int round = 0; round < 3; ++round)
        {
            const double now = 1000.0 * (round + 1);
            if (!s.Due(now)) { fprintf(g_results, "seg_%s_round%d not-due\n", f.file, round); continue; }
            s.Offer(src.data(), src.data() + static_cast<size_t>(f.w) * f.h, static_cast<long>(f.w), f.w, f.h, now);
            bool got = false;
            for (int i = 0; i < 1000 && !got; ++i)
            {
                got = s.TakeMask(mask);
                if (!got) { if (s.State() != 1) break; std::this_thread::sleep_for(std::chrono::milliseconds(5)); }
            }
            const std::string name = std::string("mask_") + f.file + "_r" + std::to_string(round);
            if (got) Result(name, "ok state=" + std::to_string(s.State()), &mask);
            else fprintf(g_results, "%s none state=%d\n", name.c_str(), s.State());
            fprintf(info, "%s inferMs=%.2f state=%d\n", name.c_str(), s.InferMs(), s.State());
            if (round == 1) s.Reset();
        }
    }
    {
        // One segmenter over different pictures: smoothing with the previous mask, then Reset().
        Segmenter s;
        s.SetModel(model);
        const char* seq[] = { "face_one_1280x720", "face_none_1280x720", "face_two_1280x720", "face_two_1280x720" };
        std::vector<uint8_t> mask;
        fprintf(g_results, "seg_due_initial=%d\n", s.Due(1000) ? 1 : 0);
        for (int i = 0; i < 4; ++i)
        {
            const auto src = ReadFile(g_in + "\\" + seq[i] + ".nv12");
            if (src.empty()) break;
            const double now = 2000.0 + 100.0 * i;
            if (i == 3) s.Reset();
            s.Offer(src.data(), src.data() + 1280 * 720, 1280, 1280, 720, now);
            if (i == 0) fprintf(g_results, "seg_due_10ms_after_offer=%d\n", s.Due(now + 10) ? 1 : 0);
            bool got = false;
            for (int k = 0; k < 1000 && !got; ++k)
            {
                got = s.TakeMask(mask);
                if (!got) std::this_thread::sleep_for(std::chrono::milliseconds(5));
            }
            if (i == 0) fprintf(g_results, "seg_due_70ms_after_done=%d\n", s.Due(now + 70) ? 1 : 0);
            if (got) Result("mask_mix_r" + std::to_string(i), "ok", &mask);
            else fprintf(g_results, "mask_mix_r%d none\n", i);
        }
        fprintf(g_results, "seg_take_again=%d\n", s.TakeMask(mask) ? 1 : 0);
    }
    fclose(info);
    return 0;
}

// ---- face detector -----------------------------------------------------------------------------

int CmdFace()
{
    struct Frame { const char* file; uint32_t w, h; };
    const Frame frames[] = { { "face_one_1280x720", 1280, 720 }, { "face_one_1920x1080", 1920, 1080 }, { "face_one_640x360", 640, 360 },
                             { "face_two_1280x720", 1280, 720 }, { "face_two_1920x1080", 1920, 1080 }, { "face_none_1280x720", 1280, 720 },
                             { "face_none_1920x1080", 1920, 1080 }, { "face_one_720x1280", 720, 1280 } };
    FILE* info = fopen((g_out + "\\face_info.txt").c_str(), "w");
    for (const Frame& f : frames)
    {
        const auto src = ReadFile(g_in + "\\" + f.file + ".nv12");
        if (src.empty()) continue;
        // A fresh detector per file; the same frame is offered 4 times (a tracker follows faces over time).
        FaceDetector d;
        double now = 1000;
        for (int round = 0; round < 4; ++round)
        {
            now += 250;
            int tries = 0;
            while (!d.Due(now) && tries++ < 1000) std::this_thread::sleep_for(std::chrono::milliseconds(5));
            d.Offer(src.data(), static_cast<long>(f.w), f.w, f.h, 7, now);
            std::vector<FaceRect> faces;
            bool got = false;
            for (int i = 0; i < 2000 && !got; ++i)
            {
                got = d.TakeResult(7, faces);
                if (!got) { if (d.State() != 1) break; std::this_thread::sleep_for(std::chrono::milliseconds(5)); }
            }
            std::ostringstream line;
            line << "face_" << f.file << "_r" << round << " " << (got ? "result" : "none") << " state=" << d.State() << " n=" << faces.size();
            for (const FaceRect& r : faces)
            {
                char buf[160];
                snprintf(buf, sizeof(buf), " [%.9f %.9f %.9f %.9f]", r.x, r.y, r.w, r.h);
                line << buf;
            }
            fprintf(g_results, "%s\n", line.str().c_str());
            if (!got && d.State() != 1) break;
        }
        fprintf(info, "%s state=%d\n", f.file, d.State());
    }
    {
        // Gating: the interval, the generation tag, results handed out once.
        const auto src = ReadFile(g_in + "\\face_one_1280x720.nv12");
        FaceDetector d;
        fprintf(g_results, "face_due_initial=%d\n", d.Due(1000) ? 1 : 0);
        d.Offer(src.data(), 1280, 1280, 720, 7, 1000);
        fprintf(g_results, "face_due_100ms_after_offer=%d\n", d.Due(1100) ? 1 : 0);
        int tries = 0;
        while (!d.Due(1300) && tries++ < 2000) std::this_thread::sleep_for(std::chrono::milliseconds(5));
        fprintf(g_results, "face_due_300ms_after_offer_when_done=%d\n", d.Due(1300) ? 1 : 0);
        std::vector<FaceRect> faces;
        fprintf(g_results, "face_take_other_generation=%d\n", d.TakeResult(8, faces) ? 1 : 0);
        fprintf(g_results, "face_take_after_drop=%d n=%zu\n", d.TakeResult(7, faces) ? 1 : 0, faces.size());
    }
    fclose(info);
    return 0;
}
} // namespace

// A window coordinate, or "auto" (INT_MIN: the corner of the work area).
static int Coord(const wchar_t* s)
{
    return wcscmp(s, L"auto") == 0 ? INT_MIN : _wtoi(s);
}

int wmain(int argc, wchar_t** argv)
{
    if (argc < 4)
    {
        fprintf(stderr, "usage: fx_cpp still|blend|seg|face <indir> <outdir> [model]\n");
        return 2;
    }
    const auto narrow = [](const wchar_t* w) {
        const int n = WideCharToMultiByte(CP_UTF8, 0, w, -1, nullptr, 0, nullptr, nullptr);
        std::string s(static_cast<size_t>(n > 0 ? n - 1 : 0), '\0');
        WideCharToMultiByte(CP_UTF8, 0, w, -1, s.data(), n, nullptr, nullptr);
        return s;
    };
    const std::wstring cmd = argv[1];
    g_in = narrow(argv[2]);
    g_out = narrow(argv[3]);
    CreateDirectoryA(g_out.c_str(), nullptr);
    // The engine's capture thread is in the multithreaded apartment: so is this one.
    CoInitializeEx(nullptr, COINIT_MULTITHREADED);
    g_results = fopen((g_out + "\\results.txt").c_str(), "w");
    int rc = 2;
    if (cmd == L"still") rc = CmdStill();
    else if (cmd == L"blend") rc = CmdBlend();
    else if (cmd == L"seg" && argc >= 5) rc = CmdSeg(argv[4]);
    else if (cmd == L"face") rc = CmdFace();
    else if (cmd == L"cpu") rc = CmdCpu();
    else if (cmd == L"windowhost" && argc >= 7) rc = CmdWindowHost(g_out, Coord(argv[4]), Coord(argv[5]), _wtoi(argv[6]));
    else if (cmd == L"closewatch" && argc >= 5)
    {
        FILE* timing = fopen((g_out + "\\timing.txt").c_str(), "w");
        rc = CmdCloseWatch(g_results, _wtoi64(argv[4]), timing);
        fclose(timing);
    }
    else if (cmd == L"window") rc = CmdWindow(g_out, g_results, argc >= 6 ? Coord(argv[4]) : INT_MIN, argc >= 6 ? Coord(argv[5]) : INT_MIN);
    fclose(g_results);
    CoUninitialize();
    return rc;
}
