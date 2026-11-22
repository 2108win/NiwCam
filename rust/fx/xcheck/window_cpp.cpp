// WindowSource check, C++ side: a plain Win32 window of our own (never a user's window) drawn
// with GDI, captured with the real WindowCapture. fx/examples/fx_rust.rs does the same with the
// Rust port; run.ps1 compares frame sizes, frame counts and pixels.
#include "../../../src/engine/WindowSource.h"

#include <windows.h>

#include <algorithm>
#include <climits>
#include <atomic>
#include <chrono>
#include <cstdio>
#include <string>
#include <thread>
#include <vector>

namespace
{
std::atomic<int> g_step{ 0 };
std::atomic<bool> g_animate{ false };
HWND g_hwnd = nullptr;

// The test pattern: a function of the size and g_step only, so both languages draw the same pixels.
void Draw(HDC dc, int w, int h, int step)
{
    for (int cy = 0; cy * 20 < h; ++cy)
        for (int cx = 0; cx * 20 < w; ++cx)
        {
            HBRUSH b = CreateSolidBrush(RGB((cx * 37) & 255, (cy * 53) & 255, ((cx + cy) * 29) & 255));
            const RECT r{ cx * 20, cy * 20, cx * 20 + 20, cy * 20 + 20 };
            FillRect(dc, &r, b);
            DeleteObject(b);
        }
    if (step > 0)
    {
        HBRUSH white = CreateSolidBrush(RGB(255, 255, 255));
        const int x = (step * 12) % std::max(1, w - 30);
        const RECT r{ x, h / 2 - 15, x + 30, h / 2 + 15 };
        FillRect(dc, &r, white);
        DeleteObject(white);
    }
}

LRESULT CALLBACK Proc(HWND hwnd, UINT msg, WPARAM wp, LPARAM lp)
{
    switch (msg)
    {
    case WM_PAINT:
    {
        PAINTSTRUCT ps;
        HDC dc = BeginPaint(hwnd, &ps);
        RECT rc;
        GetClientRect(hwnd, &rc);
        Draw(dc, rc.right, rc.bottom, g_step);
        EndPaint(hwnd, &ps);
        return 0;
    }
    case WM_ERASEBKGND:
        return 1;
    case WM_SIZE:
        InvalidateRect(hwnd, nullptr, FALSE);
        return 0;
    case WM_TIMER:
        if (g_animate)
        {
            ++g_step;
            InvalidateRect(hwnd, nullptr, FALSE);
        }
        return 0;
    case WM_APP + 1: // from the capture thread: destroy (a window is destroyed on its own thread)
        DestroyWindow(hwnd);
        return 0;
    case WM_DESTROY:
        PostQuitMessage(0);
        return 0;
    }
    return DefWindowProcW(hwnd, msg, wp, lp);
}

struct TestWindow
{
    std::thread thread;
    HANDLE ready = CreateEventW(nullptr, TRUE, FALSE, nullptr);
    void Start(int x, int y, int w, int h)
    {
        thread = std::thread([=, this] {
            WNDCLASSW wc{};
            wc.lpfnWndProc = Proc;
            wc.hInstance = GetModuleHandleW(nullptr);
            wc.lpszClassName = L"NiwCamFxTestWindow";
            wc.hCursor = LoadCursorW(nullptr, IDC_ARROW);
            RegisterClassW(&wc);
            // No frame (pure content), never activated, off the taskbar: it must not disturb anyone.
            g_hwnd = CreateWindowExW(WS_EX_NOACTIVATE | WS_EX_TOOLWINDOW, wc.lpszClassName, L"NiwCam fx test window", WS_POPUP,
                                     x, y, w, h, nullptr, nullptr, wc.hInstance, nullptr);
            ShowWindow(g_hwnd, SW_SHOWNOACTIVATE);
            // Under everything else: windows in front of it hide it from the user, not from the capture.
            SetWindowPos(g_hwnd, HWND_BOTTOM, 0, 0, 0, 0, SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE);
            UpdateWindow(g_hwnd);
            SetTimer(g_hwnd, 1, 33, nullptr);
            SetEvent(ready);
            MSG m;
            while (GetMessageW(&m, nullptr, 0, 0) > 0)
            {
                TranslateMessage(&m);
                DispatchMessageW(&m);
            }
        });
        WaitForSingleObject(ready, 5000);
    }
};

uint64_t Fnv(const uint8_t* d, size_t n)
{
    uint64_t h = 1469598103934665603ull;
    for (size_t i = 0; i < n; ++i) h = (h ^ d[i]) * 1099511628211ull;
    return h;
}

std::string Narrow(const std::wstring& w)
{
    if (w.empty()) return {};
    const int n = WideCharToMultiByte(CP_UTF8, 0, w.data(), static_cast<int>(w.size()), nullptr, 0, nullptr, nullptr);
    std::string s(static_cast<size_t>(n), '\0');
    WideCharToMultiByte(CP_UTF8, 0, w.data(), static_cast<int>(w.size()), s.data(), n, nullptr, nullptr);
    return s;
}

const char* Name(WindowCapture::Result r)
{
    switch (r)
    {
    case WindowCapture::Result::Frame: return "Frame";
    case WindowCapture::Result::Timeout: return "Timeout";
    case WindowCapture::Result::Closed: return "Closed";
    default: return "Error";
    }
}

double Ms(std::chrono::steady_clock::time_point t0)
{
    return std::chrono::duration<double, std::milli>(std::chrono::steady_clock::now() - t0).count();
}
} // namespace

// Bottom right corner of the work area, room for the 520 x 340 the window grows to (a window that is
// off screen is not composed: its frames are empty).
void DefaultSpot(int& x, int& y)
{
    RECT wa{ 0, 0, 1920, 1080 };
    SystemParametersInfoW(SPI_GETWORKAREA, 0, &wa, 0);
    x = wa.right - 560;
    y = wa.bottom - 380;
}

int CmdWindow(const std::string& outdir, FILE* results, int x, int y)
{
    if (x == INT_MIN) DefaultSpot(x, y);
    TestWindow tw;
    tw.Start(x, y, 400, 300);
    HWND hwnd = g_hwnd;
    FILE* timing = fopen((outdir + "\\timing.txt").c_str(), "w");

    uint32_t cw = 0, ch = 0;
    CaptureWindowSize(hwnd, cw, ch);
    fprintf(results, "capture_window_size %ux%u\n", cw, ch);
    {
        // The two engines' lists must match each other (this process's own windows are left out of both).
        const auto list = ListCaptureWindows();
        std::string all;
        for (const auto& w : list) all += Narrow(w.title) + "\n";
        fprintf(results, "list_capture_windows count=%zu titles_fnv=%016llx\n", list.size(),
                static_cast<unsigned long long>(Fnv(reinterpret_cast<const uint8_t*>(all.data()), all.size())));
    }

    WindowCapture cap;
    std::wstring error;
    if (!cap.Open(hwnd, error))
    {
        fprintf(results, "open FAILED %s\n", Narrow(error).c_str());
        return 1;
    }
    fprintf(results, "open ok\n");

    std::vector<uint8_t> last;       // the newest frame, tightly packed BGRA
    uint32_t lastW = 0, lastH = 0;
    std::vector<std::string> sizes;  // sizes of the frames handed to the callback, consecutive duplicates merged
    uint64_t callbacks = 0;
    const auto onFrame = [&](const uint8_t* bgra, uint32_t pitch, uint32_t w, uint32_t h) {
        ++callbacks;
        last.resize(static_cast<size_t>(w) * h * 4);
        for (uint32_t row = 0; row < h; ++row) memcpy(last.data() + static_cast<size_t>(row) * w * 4, bgra + static_cast<size_t>(row) * pitch, static_cast<size_t>(w) * 4);
        lastW = w;
        lastH = h;
        const std::string s = std::to_string(w) + "x" + std::to_string(h);
        if (sizes.empty() || sizes.back() != s) sizes.push_back(s);
    };
    const auto join = [](const std::vector<std::string>& v) {
        std::string s;
        for (size_t i = 0; i < v.size(); ++i) s += (i ? ">" : "") + v[i];
        return s;
    };
    const auto save = [&](const std::string& name) {
        const std::string path = outdir + "\\" + name + ".bgra";
        FILE* f = fopen(path.c_str(), "wb");
        if (f)
        {
            fwrite(&lastW, 4, 1, f);
            fwrite(&lastH, 4, 1, f);
            fwrite(last.data(), 1, last.size(), f);
            fclose(f);
        }
        fprintf(results, "%s size=%ux%u fnv=%016llx\n", name.c_str(), lastW, lastH, static_cast<unsigned long long>(Fnv(last.data(), last.size())));
    };
    // Drain until the window has been quiet for `quietMs`.
    const auto drain = [&](int quietMs) {
        for (int i = 0; i < 200; ++i)
            if (cap.Next(quietMs, true, onFrame) != WindowCapture::Result::Frame) break;
    };

    // P1: a still window. The first frame arrives at once; after it nothing, until the window changes.
    WindowCapture::Result r = WindowCapture::Result::Timeout;
    for (int i = 0; i < 40 && r != WindowCapture::Result::Frame; ++i) r = cap.Next(100, true, onFrame);
    fprintf(results, "first_frame result=%s\n", Name(r));
    drain(300);
    save("win_static");
    fprintf(results, "still_next %s\n", Name(cap.Next(300, true, onFrame)));

    // P2: an animated window (a timer repaints it ~30 times a second) for 3 s, read.
    sizes.clear();
    callbacks = 0;
    g_animate = true;
    uint64_t frames = 0;
    auto t0 = std::chrono::steady_clock::now();
    while (Ms(t0) < 3000)
        if (cap.Next(100, true, onFrame) == WindowCapture::Result::Frame) ++frames;
    fprintf(results, "anim_3s_read sizes=%s frames_ok=%d callbacks_eq_frames=%d\n", join(sizes).c_str(), frames >= 45 ? 1 : 0, callbacks == frames ? 1 : 0);
    fprintf(timing, "anim_3s_read frames=%llu callbacks=%llu\n", static_cast<unsigned long long>(frames), static_cast<unsigned long long>(callbacks));
    // The same without reading: frames are released unread, the callback is never called.
    callbacks = 0;
    frames = 0;
    t0 = std::chrono::steady_clock::now();
    while (Ms(t0) < 1000)
        if (cap.Next(100, false, onFrame) == WindowCapture::Result::Frame) ++frames;
    fprintf(results, "anim_1s_unread frames_ok=%d callbacks=%llu\n", frames >= 15 ? 1 : 0, static_cast<unsigned long long>(callbacks));
    fprintf(timing, "anim_1s_unread frames=%llu\n", static_cast<unsigned long long>(frames));
    // Stop the animation and let the last frames through.
    g_animate = false;
    g_step = 0;
    InvalidateRect(hwnd, nullptr, FALSE);
    drain(300);

    // P3: resizes. The pool is recreated at the new size; frames in between are cut to what fits.
    const auto resize = [&](const char* name, int w, int h) {
        sizes.clear();
        SetWindowPos(hwnd, nullptr, 0, 0, w, h, SWP_NOMOVE | SWP_NOZORDER | SWP_NOACTIVATE);
        uint32_t qw = 0, qh = 0;
        CaptureWindowSize(hwnd, qw, qh);
        t0 = std::chrono::steady_clock::now();
        int guard = 0;
        // The window repaints all the time, so frames keep coming at the new size (after the pool is
        // recreated nothing is sent for a window that does not change).
        while (Ms(t0) < 1500 && guard++ < 200)
        {
            InvalidateRect(hwnd, nullptr, FALSE);
            cap.Next(150, true, onFrame);
        }
        fprintf(results, "%s capture_window_size=%ux%u final=%ux%u\n", name, qw, qh, lastW, lastH);
        fprintf(timing, "%s sizes=%s\n", name, join(sizes).c_str());
        save(std::string("win_") + name);
    };
    resize("resize_up", 520, 340);
    resize("resize_down", 300, 200);
    resize("resize_odd", 333, 221);

    // P4: the window closes. The "Closed" notice (the item's Closed event) is raised through a WinEvent
    // hook, so it only arrives while the thread that opened the capture pumps messages: first 2 s
    // without a pump, then with one.
    t0 = std::chrono::steady_clock::now();
    PostMessageW(hwnd, WM_APP + 1, 0, 0);
    WindowCapture::Result cr = WindowCapture::Result::Frame;
    bool closedNoPump = false;
    while (Ms(t0) < 2000)
        if ((cr = cap.Next(150, true, onFrame)) == WindowCapture::Result::Closed) { closedNoPump = true; break; }
    fprintf(results, "destroy_without_message_pump closed=%d\n", closedNoPump ? 1 : 0);
    t0 = std::chrono::steady_clock::now();
    for (int i = 0; i < 100 && cr != WindowCapture::Result::Closed; ++i)
    {
        MSG pm;
        while (PeekMessageW(&pm, nullptr, 0, 0, PM_REMOVE)) { TranslateMessage(&pm); DispatchMessageW(&pm); }
        cr = cap.Next(150, true, onFrame);
    }
    fprintf(timing, "closed_after_pump %.0f ms\n", Ms(t0));
    fprintf(results, "after_destroy last=%s again=%s\n", Name(cr), Name(cap.Next(100, true, onFrame)));
    cap.Close();
    fprintf(results, "after_close_next %s\n", Name(cap.Next(50, true, onFrame)));
    // Opening a window that is gone, and no window at all: the Vietnamese message.
    std::wstring e1, e2;
    const bool o1 = cap.Open(hwnd, e1);
    const bool o2 = cap.Open(nullptr, e2);
    fprintf(results, "open_destroyed ok=%d msg=%s\n", o1 ? 1 : 0, Narrow(e1).c_str());
    fprintf(results, "open_null ok=%d msg=%s\n", o2 ? 1 : 0, Narrow(e2).c_str());
    if (tw.thread.joinable()) tw.thread.join();
    fclose(timing);
    return 0;
}

// A window of its own in this process for another process to capture (the engine captures other
// processes' windows): prints nothing, writes the handle to <outdir>\hwnd.txt, destroys the window
// after `seconds` and exits.
int CmdWindowHost(const std::string& outdir, int x, int y, int seconds)
{
    if (x == INT_MIN) DefaultSpot(x, y);
    TestWindow tw;
    tw.Start(x, y, 400, 300);
    FILE* f = fopen((outdir + "\\hwnd.txt").c_str(), "w");
    if (f)
    {
        fprintf(f, "%lld\n", static_cast<long long>(reinterpret_cast<intptr_t>(g_hwnd)));
        fclose(f);
    }
    g_animate = true;
    std::this_thread::sleep_for(std::chrono::seconds(seconds));
    PostMessageW(g_hwnd, WM_APP + 1, 0, 0);
    if (tw.thread.joinable()) tw.thread.join();
    return 0;
}

// Captures another process's window (see CmdWindowHost) and reports what Next() says when that window
// is destroyed, with no message pump on this thread (the engine's capture thread has none).
int CmdCloseWatch(FILE* results, long long hwndValue, FILE* timing)
{
    HWND hwnd = reinterpret_cast<HWND>(static_cast<intptr_t>(hwndValue));
    WindowCapture cap;
    std::wstring error;
    if (!cap.Open(hwnd, error))
    {
        fprintf(results, "closewatch open FAILED %s\n", Narrow(error).c_str());
        return 1;
    }
    uint64_t frames = 0;
    const auto onFrame = [&](const uint8_t*, uint32_t, uint32_t, uint32_t) { ++frames; };
    const auto t0 = std::chrono::steady_clock::now();
    WindowCapture::Result r = WindowCapture::Result::Frame;
    std::string seq;
    while (Ms(t0) < 14000)
    {
        r = cap.Next(100, true, onFrame);
        const std::string n = Name(r);
        if (seq.size() < n.size() || seq.substr(seq.size() - n.size()) != n) seq += (seq.empty() ? "" : ">") + n;
        if (r == WindowCapture::Result::Closed || r == WindowCapture::Result::Error) break;
    }
    fprintf(results, "closewatch last=%s frames_seen=%d\n", Name(r), frames > 10 ? 1 : 0);
    fprintf(timing, "closewatch after %.0f ms seq=%s\n", Ms(t0), seq.c_str());
    return 0;
}
