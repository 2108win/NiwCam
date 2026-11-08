#pragma once
// The folder where NiwCam keeps its files (pairing keys, remembered phones, settings dumps).
// The app picks it (it moves the old %LOCALAPPDATA%\CamStudio folder over, or keeps using it when
// that fails) and hands it to the engine through CAMSTUDIO_DATA_DIR; tools that run without the
// app use %LOCALAPPDATA%\NiwCam.
#include <windows.h>

#include <string>

inline std::wstring AppDataDir()
{
    wchar_t env[MAX_PATH];
    const DWORD n = GetEnvironmentVariableW(L"CAMSTUDIO_DATA_DIR", env, MAX_PATH);
    if (n > 0 && n < MAX_PATH) return env;
    wchar_t local[MAX_PATH] = {};
    GetEnvironmentVariableW(L"LOCALAPPDATA", local, MAX_PATH);
    return std::wstring(local) + L"\NiwCam";
}
