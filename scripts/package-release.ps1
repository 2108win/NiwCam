# Builds everything in Release and assembles the files for a GitHub release in build\dist:
#   NiwCam-Setup-<version>.exe     (one-click installer: app + virtual camera, Inno Setup)
#   NiwCam-<version>-android.apk
# plus the same two files under their pre-0.1.14 names (CamStudio-Setup-<version>.exe,
# CamStudio-<version>-android.apk): builds before 0.1.14 look for exactly those names to update.
# Needs Inno Setup 6 (winget install JRSoftware.InnoSetup).
# Usage: powershell -ExecutionPolicy Bypass -File scripts\package-release.ps1 [-Rust] [-SkipAndroid]
#   -Rust         put the Rust engine, virtual camera and DirectShow filters (rust\, ADR-001..011) in the installer
#                 in place of the C++ ones (same file names), and fetch the optional models first
#   -SkipAndroid  no APK (a local install, not a release)
param([switch]$Rust, [switch]$SkipAndroid)
$ErrorActionPreference = "Stop"
$ProgressPreference = "SilentlyContinue"
$root = Split-Path -Parent $PSScriptRoot

$version = ([xml](Get-Content (Join-Path $root "app\NiwCam.App.csproj"))).Project.PropertyGroup.Version | Where-Object { $_ } | Select-Object -First 1
if (-not $version) { throw "No <Version> in app\NiwCam.App.csproj" }
Write-Host "Packaging NiwCam $version"

# 1. Native parts (engine, virtual camera, tools).
& powershell -NoProfile -ExecutionPolicy Bypass -File (Join-Path $PSScriptRoot "build.ps1")
if ($LASTEXITCODE) { throw "build.ps1 failed" }

# 2. App: the self-contained build output (.NET + Windows App SDK included). Not `dotnet publish`:
#    for this unpackaged WinUI app it leaves out NiwCam.pri and the compiled XAML (*.xbf), and the
#    app then crashes at startup.
$out = Join-Path $root "build\dist"   # not build\release: paths ignore case and build\Release holds the CMake output
$stage = Join-Path $out "NiwCam-$version-win-x64"
if (Test-Path $out) { Remove-Item $out -Recurse -Force }
New-Item -ItemType Directory -Force $stage | Out-Null
$appOut = Join-Path $stage "NiwCam"
$bin = Join-Path $root "app\bin\x64\Release\net10.0-windows10.0.22621.0\win-x64"
Copy-Item $bin $appOut -Recurse
Get-ChildItem $appOut -Recurse -File -Filter *.pdb | Remove-Item -Force
# A build folder that was used before 0.1.14 still holds the old CamStudio.* output: not for the installer
# (it also deletes them from an old install).
Get-ChildItem $appOut -File | Where-Object { $_.Name -like "CamStudio.*" -or $_.Name -eq "CamStudioEngine.dll" } | Remove-Item -Force
Copy-Item (Join-Path $root "build\Release\CamStudioVCam.dll") $appOut
# Windows 10: the DirectShow filter, 64- and 32-bit (docs/windows10.md).
Copy-Item (Join-Path $root "build\Release\CamStudioDShow.dll") (Join-Path $appOut "CamStudioDShow64.dll")
Copy-Item (Join-Path $root "build-x86\Release\CamStudioDShow.dll") (Join-Path $appOut "CamStudioDShow32.dll")
# adb for Android phones on a USB cable (docs/protocol.md 1.2). The engine starts it only while such
# a phone is plugged in. From the Android SDK's platform-tools, with its NOTICE.txt (licences).
$platformTools = Join-Path $env:LOCALAPPDATA "Android\Sdk\platform-tools"
$adbOut = Join-Path $appOut "adb"
New-Item -ItemType Directory -Force $adbOut | Out-Null
foreach ($f in "adb.exe", "AdbWinApi.dll", "AdbWinUsbApi.dll", "NOTICE.txt") {
    $src = Join-Path $platformTools $f
    if (-not (Test-Path $src)) { throw "$src missing: run scripts\setup-android-sdk.ps1" }
    Copy-Item $src $adbOut
}
if ($Rust) {
    # The Rust core: same DLL names and exports as the C++ (checked by rust\*\xcheck). Own target folder.
    $env:Path += ";$env:USERPROFILE\.cargo\bin"
    $env:CARGO_TARGET_DIR = Join-Path $root "build\rust-ship"
    Push-Location (Join-Path $root "rust")
    try {
        & powershell -NoProfile -ExecutionPolicy Bypass -File (Join-Path $PSScriptRoot "fetch-models.ps1")
        if ($LASTEXITCODE) { throw "fetch-models.ps1 failed" }
        cargo build --release -p niwcam-engine -p niwcam-vcam -p niwcam-dshow
        if ($LASTEXITCODE) { throw "cargo build (x64) failed" }
        cargo build --release -p niwcam-dshow --target i686-pc-windows-msvc
        if ($LASTEXITCODE) { throw "cargo build (x86 dshow) failed" }
    } finally { Pop-Location }
    $rt = $env:CARGO_TARGET_DIR
    Copy-Item (Join-Path $rt "release\NiwCamEngine.dll") $appOut -Force
    Copy-Item (Join-Path $rt "release\CamStudioVCam.dll") $appOut -Force
    Copy-Item (Join-Path $rt "release\CamStudioDShow.dll") (Join-Path $appOut "CamStudioDShow64.dll") -Force
    Copy-Item (Join-Path $rt "i686-pc-windows-msvc\release\CamStudioDShow.dll") (Join-Path $appOut "CamStudioDShow32.dll") -Force
    Write-Host "Rust core in the installer"
}
foreach ($f in "NiwCam.exe", "NiwCam.pri", "MainWindow.xbf", "NiwCamEngine.dll", "adb\adb.exe", "CamStudioVCam.dll", "CamStudioDShow64.dll", "CamStudioDShow32.dll") {
    if (-not (Test-Path (Join-Path $appOut $f))) { throw "$f missing from the app folder" }
}

# 3. Installer (packaging\windows\NiwCam.iss).
$iscc = @("$env:LOCALAPPDATA\Programs\Inno Setup 6\ISCC.exe", "${env:ProgramFiles(x86)}\Inno Setup 6\ISCC.exe", "$env:ProgramFiles\Inno Setup 6\ISCC.exe") |
    Where-Object { Test-Path $_ } | Select-Object -First 1
if (-not $iscc) { throw "Inno Setup 6 not found: winget install JRSoftware.InnoSetup" }
& $iscc /Q "/DAppVersion=$version" "/DSourceDir=$appOut" "/DOutputDir=$out" (Join-Path $root "packaging\windows\NiwCam.iss")
if ($LASTEXITCODE) { throw "ISCC failed" }
Remove-Item $stage -Recurse -Force

# 4. Android.
if ($SkipAndroid) { Get-ChildItem $out -File | ForEach-Object { "{0,-40} {1,8:N1} MB" -f $_.Name, ($_.Length / 1MB) }; return }
& powershell -NoProfile -ExecutionPolicy Bypass -File (Join-Path $PSScriptRoot "build-android.ps1")
if ($LASTEXITCODE) { throw "build-android.ps1 failed" }
Copy-Item (Join-Path $root "build\CamStudioPhone.apk") (Join-Path $out "NiwCam-$version-android.apk")
# The names older builds look for (same bytes, so the same SHA-256).
Copy-Item (Join-Path $out "NiwCam-Setup-$version.exe") (Join-Path $out "CamStudio-Setup-$version.exe")
Copy-Item (Join-Path $out "NiwCam-$version-android.apk") (Join-Path $out "CamStudio-$version-android.apk")

Get-ChildItem $out -File | ForEach-Object { "{0,-40} {1,8:N1} MB" -f $_.Name, ($_.Length / 1MB) }

