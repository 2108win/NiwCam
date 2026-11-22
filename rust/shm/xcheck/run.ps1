# Cross-check of niwcam-shm against the C++ FrameWriter / FrameReader / SharedFrameHeader.
#
# 1. Copies src/common, src/vcam/{Common.h,FrameReader.*} and src/engine/FrameWriter.h into
#    build\xcheck-shm\src and patches ONLY the four kernel-object-name #defines of the copy of
#    SharedFrame.h so the names carry a per-test suffix (the originals are not touched).
# 2. Builds cpp_shm.dll (the real C++ writer and reader behind flat C functions).
# 3. Runs the Rust integration tests, which drive the C++ DLL and the Rust code on the same names
#    in both directions (Rust writer + C++ reader, C++ writer + Rust reader, layout, local channel,
#    seqlock stress). Exit code 0 = everything equal.
#
# Never uses the real Global\ / Local\CamStudioVCam_* names: the running app owns them.
$ErrorActionPreference = 'Stop'
$root = Split-Path -Parent (Split-Path -Parent (Split-Path -Parent $PSScriptRoot))
$out = Join-Path $root 'build\xcheck-shm'
$src = Join-Path $out 'src'
Remove-Item -Recurse -Force $src -ErrorAction SilentlyContinue
foreach ($d in 'common', 'vcam', 'engine') { New-Item -ItemType Directory -Force (Join-Path $src $d) | Out-Null }
Copy-Item (Join-Path $root 'src\common\*.h') (Join-Path $src 'common')
Copy-Item (Join-Path $root 'src\vcam\Common.h'), (Join-Path $root 'src\vcam\FrameReader.h'), (Join-Path $root 'src\vcam\FrameReader.cpp') (Join-Path $src 'vcam')
Copy-Item (Join-Path $root 'src\engine\FrameWriter.h') (Join-Path $src 'engine')

# Patch the names in the copy.
$sf = Join-Path $src 'common\SharedFrame.h'
$t = Get-Content -Raw $sf
$map = @(
    @('CAMSTUDIO_MAPPING_NAME', 0), @('CAMSTUDIO_EVENT_NAME', 1),
    @('CAMSTUDIO_LOCAL_MAPPING_NAME', 2), @('CAMSTUDIO_LOCAL_EVENT_NAME', 3))
foreach ($m in $map) {
    $re = '(?m)^#define ' + $m[0] + '\s+L"[^"]*"\s*$'
    if ($t -notmatch $re) { throw "name macro $($m[0]) not found in SharedFrame.h" }
    $t = [regex]::Replace($t, $re, '#define ' + $m[0] + ' CsName(' + $m[1] + ')')
}
$t = $t.Replace('#pragma once', "#pragma once`nextern `"C`" const wchar_t* CsName(int);")
Set-Content -Encoding utf8 $sf $t

$vs = & "${env:ProgramFiles(x86)}\Microsoft Visual Studio\Installer\vswhere.exe" -latest -products * -property installationPath
$env:Path += ";${env:ProgramFiles(x86)}\Microsoft Visual Studio\Installer" # vcvars calls vswhere by name
$vcvars = Join-Path $vs 'VC\Auxiliary\Build\vcvars64.bat'
$dll = Join-Path $out 'cpp_shm.dll'
$bat = Join-Path $out 'build.bat'
Set-Content -Encoding ascii $bat -Value @(
    '@echo off',
    "call `"$vcvars`" >nul",
    "cl /nologo /LD /std:c++20 /O2 /EHsc /permissive- /utf-8 /DUNICODE /D_UNICODE /DNOMINMAX /DWIN32_LEAN_AND_MEAN /D_WIN32_WINNT=0x0A00 /I`"$src`" /Fo`"$out\\`" /Fe`"$dll`" `"$(Join-Path $PSScriptRoot 'cpp_shm.cpp')`" `"$(Join-Path $src 'vcam\FrameReader.cpp')`" /link mfplat.lib mfuuid.lib advapi32.lib ole32.lib propsys.lib"
)
cmd /c $bat
if ($LASTEXITCODE -ne 0) { throw 'C++ build failed' }

$env:Path += ";$env:USERPROFILE\.cargo\bin"
$env:CARGO_TARGET_DIR = Join-Path $root 'build\rust-shm'
$env:NIWCAM_SHM_CPP_DLL = $dll
Push-Location (Join-Path $root 'rust')
try {
    cargo test -p niwcam-shm --release --color never -- --test-threads=1 --nocapture
    if ($LASTEXITCODE -ne 0) { throw "cargo test failed ($LASTEXITCODE)" }
}
finally { Pop-Location }
Write-Host 'SHM XCHECK: ALL PASS'
