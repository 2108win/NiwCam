// Test-only: the four kernel-object names of the C++ media source, taken from NIWCAM_TEST_SUFFIX
// (the same session-local names the Rust crate's `test-names` feature and niwcam_shm::Names::for_test
// use). run.ps1 patches the #defines of a *copy* of SharedFrame.h to call CsName(n).
#include <windows.h>
#include <string>

extern "C" const wchar_t* CsName(int n)
{
    static std::wstring names[4];
    static bool ready = false;
    if (!ready)
    {
        wchar_t buf[256];
        const DWORD len = GetEnvironmentVariableW(L"NIWCAM_TEST_SUFFIX", buf, 256);
        const std::wstring s = (len > 0 && len < 256) ? std::wstring(buf, len) : L"noenv_" + std::to_wstring(GetCurrentProcessId());
        names[0] = L"Local\\NiwCamTest_" + s + L"_G_Frames";
        names[1] = L"Local\\NiwCamTest_" + s + L"_G_NewFrame";
        names[2] = L"Local\\NiwCamTest_" + s + L"_L_Frames";
        names[3] = L"Local\\NiwCamTest_" + s + L"_L_NewFrame";
        ready = true;
    }
    return names[n].c_str();
}
