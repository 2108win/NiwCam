# Builds fx_cpp.exe (the C++ reference driver) from the real engine sources with the engine's CMake flags.
param([string]$Out = (Join-Path (Split-Path -Parent (Split-Path -Parent (Split-Path -Parent $PSScriptRoot))) 'build\xcheck-fx'))
$ErrorActionPreference = 'Stop'
$root = Split-Path -Parent (Split-Path -Parent (Split-Path -Parent $PSScriptRoot))
New-Item -ItemType Directory -Force $Out | Out-Null
$env:Path += ";${env:ProgramFiles(x86)}\Microsoft Visual Studio\Installer" # vcvars calls vswhere by name
$vs = & "${env:ProgramFiles(x86)}\Microsoft Visual Studio\Installer\vswhere.exe" -latest -products * -property installationPath
$vcvars = Join-Path $vs 'VC\Auxiliary\Build\vcvars64.bat'
$eng = Join-Path $root 'src\engine'
$sources = @()
foreach ($f in 'fx_cpp.cpp', 'window_cpp.cpp') { $sources += "`"$(Join-Path $PSScriptRoot $f)`"" }
foreach ($f in 'StillPicture.cpp', 'Segmenter.cpp', 'FaceDetect.cpp', 'WindowSource.cpp') { $sources += "`"$(Join-Path $eng $f)`"" }
$bat = Join-Path $Out 'build.bat'
Set-Content -Encoding ascii $bat -Value @(
    '@echo off',
    "call `"$vcvars`" >nul",
    ("cl /nologo /std:c++20 /EHsc /utf-8 /bigobj /MT /O1 /W3 /permissive- /DUNICODE /D_UNICODE /DNOMINMAX /DWIN32_LEAN_AND_MEAN " +
     "/D_WIN32_WINNT=0x0A00 /D_CRT_SECURE_NO_WARNINGS /Fo$Out\ /Fe`"$Out\fx_cpp.exe`" " + ($sources -join ' ') +
     " /link d2d1.lib dwrite.lib windowscodecs.lib windowsapp.lib ole32.lib d3d11.lib dxgi.lib dwmapi.lib user32.lib gdi32.lib")
)
cmd /c $bat
if ($LASTEXITCODE -ne 0) { throw 'C++ build failed' }
