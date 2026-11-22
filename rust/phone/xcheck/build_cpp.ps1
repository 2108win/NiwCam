# Builds the C++ reference drivers for rust/phone into build\xcheck-phone (MSVC, from the real
# src/engine files): cpp_phone.exe (PhoneLink, key store) and, for Discovery, cpp_discovery.exe.
$ErrorActionPreference = 'Stop'
$root = Split-Path -Parent (Split-Path -Parent (Split-Path -Parent $PSScriptRoot))
$out = Join-Path $root 'build\xcheck-phone'
New-Item -ItemType Directory -Force $out | Out-Null
$vs = & "${env:ProgramFiles(x86)}\Microsoft Visual Studio\Installer\vswhere.exe" -latest -products * -property installationPath
$env:Path += ";${env:ProgramFiles(x86)}\Microsoft Visual Studio\Installer"
$vcvars = Join-Path $vs 'VC\Auxiliary\Build\vcvars64.bat'
$eng = Join-Path $root 'src\engine'
$bat = Join-Path $out 'build.bat'
$libs = 'ws2_32.lib dnsapi.lib cfgmgr32.lib crypt32.lib ole32.lib bcrypt.lib'
$srcs = "`"$eng\PhoneLink.cpp`" `"$eng\Crypto.cpp`" `"$eng\Discovery.cpp`" `"$eng\UsbTransport.cpp`""
Set-Content -Encoding ascii $bat -Value @(
    '@echo off',
    "call `"$vcvars`" >nul",
    "cl /nologo /std:c++20 /O1 /EHsc /DUNICODE /D_UNICODE /DNOMINMAX /DWIN32_LEAN_AND_MEAN /D_WIN32_WINNT=0x0A00 /permissive- /utf-8 /Fo`"$out\\`" /Fe`"$out\cpp_phone.exe`" `"$PSScriptRoot\cpp_phone.cpp`" $srcs $libs",
    "cl /nologo /std:c++20 /O1 /EHsc /DUNICODE /D_UNICODE /DNOMINMAX /DWIN32_LEAN_AND_MEAN /D_WIN32_WINNT=0x0A00 /permissive- /utf-8 /Fo`"$out\\`" /Fe`"$out\cpp_discovery.exe`" `"$PSScriptRoot\cpp_discovery.cpp`" ws2_32.lib dnsapi.lib"
)
cmd /c $bat
if ($LASTEXITCODE -ne 0) { throw 'C++ build failed' }
Write-Host "built $out"
