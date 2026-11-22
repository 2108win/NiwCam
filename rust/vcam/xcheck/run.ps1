# Cross-check of the Rust media source (niwcam-vcam) against the C++ one (src/vcam), without
# registering anything: no admin, no COM registration, the real camera and Frame Server are not
# touched. Real loading by Frame Server is NOT tested here (it needs an admin registration).
#
#  1. C++: copies src/vcam and src/common to build\xcheck-vcam\src, patches ONLY the four kernel-object
#     name #defines of the copy of SharedFrame.h (names from NIWCAM_TEST_SUFFIX, session-local) and
#     builds CamStudioVCam.dll from the copy with the same libraries as CMakeLists.txt.
#  2. Rust: the shipped DLL (no feature), and a test DLL with `--features test-names`.
#  3. dumpbin /exports of the C++ DLL and the shipped Rust DLL must list the same names and ordinals.
#  4. fs_sim (a Frame Server stand-in) loads each test DLL, drives the media source and writes a
#     report; the two reports are diffed (lines starting with # are informational).
#  5. Pacing statistics of the two DLLs are compared with tolerances.
#  6. cargo test --features test-names (DllRegisterServer into HKCU\Software\NiwCamTest_*, unit tests).
$ErrorActionPreference = 'Stop'
$root = Split-Path -Parent (Split-Path -Parent (Split-Path -Parent $PSScriptRoot))
$out = Join-Path $root 'build\xcheck-vcam'
$src = Join-Path $out 'src'
$cppOut = Join-Path $out 'cpp'
Remove-Item -Recurse -Force $src -ErrorAction SilentlyContinue
foreach ($d in 'common', 'vcam') { New-Item -ItemType Directory -Force (Join-Path $src $d) | Out-Null }
New-Item -ItemType Directory -Force $cppOut | Out-Null
Copy-Item (Join-Path $root 'src\common\*.h') (Join-Path $src 'common')
Copy-Item (Join-Path $root 'src\vcam\*') (Join-Path $src 'vcam')

# ---- 1. C++ DLL with test names
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
$dllCpp = Join-Path $cppOut 'CamStudioVCam.dll'
$bat = Join-Path $out 'build.bat'
$cpps = (Get-ChildItem (Join-Path $src 'vcam') -Filter *.cpp | ForEach-Object { "`"$($_.FullName)`"" }) -join ' '
$names = Join-Path $PSScriptRoot 'vcam_test_names.cpp'
Set-Content -Encoding ascii $bat -Value @(
    '@echo off',
    "call `"$vcvars`" >nul",
    "cl /nologo /LD /std:c++20 /O2 /W4 /permissive- /utf-8 /EHsc /DUNICODE /D_UNICODE /DNOMINMAX /DWIN32_LEAN_AND_MEAN /D_WIN32_WINNT=0x0A00 /Fo`"$cppOut\\`" /Fe`"$dllCpp`" $cpps `"$names`" /link /DEF:`"$(Join-Path $src 'vcam\vcam.def')`" mfplat.lib mfuuid.lib mf.lib mfsensorgroup.lib propsys.lib advapi32.lib runtimeobject.lib"
)
cmd /c $bat
if ($LASTEXITCODE -ne 0) { throw 'C++ build failed' }

# ---- 2. Rust DLLs
$env:Path += ";$env:USERPROFILE\.cargo\bin"
Push-Location (Join-Path $root 'rust')
try {
    $env:CARGO_TARGET_DIR = Join-Path $root 'build\rust-vcam-ship'
    cargo build -p niwcam-vcam --release --color never
    if ($LASTEXITCODE -ne 0) { throw 'cargo build (ship) failed' }
    $env:CARGO_TARGET_DIR = Join-Path $root 'build\rust-vcam-test'
    cargo build -p niwcam-vcam --release --features test-names --color never
    if ($LASTEXITCODE -ne 0) { throw 'cargo build (test-names) failed' }
    cargo build -p niwcam-vcam --release --features test-names --example fs_sim --color never
    if ($LASTEXITCODE -ne 0) { throw 'cargo build (fs_sim) failed' }
}
finally { Pop-Location }
$dllShip = Join-Path $root 'build\rust-vcam-ship\release\CamStudioVCam.dll'
$dllRust = Join-Path $root 'build\rust-vcam-test\release\CamStudioVCam.dll'
$sim = Join-Path $root 'build\rust-vcam-test\release\examples\fs_sim.exe'

# ---- 3. exports
$dumpbin = (Get-ChildItem (Join-Path $vs 'VC\Tools\MSVC\*\bin\Hostx64\x64\dumpbin.exe'))[0].FullName
function Get-Exports($dll) {
    & $dumpbin /exports $dll | Where-Object { $_ -match '^\s+(\d+)\s+[0-9A-Fa-f]+\s+[0-9A-Fa-f]+\s+(\w+)' } |
        ForEach-Object { if ($_ -match '^\s+(\d+)\s+[0-9A-Fa-f]+\s+[0-9A-Fa-f]+\s+(\w+)') { "$($Matches[1]) $($Matches[2])" } }
}
$ec = Get-Exports (Join-Path $root 'build\Release\CamStudioVCam.dll')
$er = Get-Exports $dllShip
Write-Host "exports C++ (shipped build\Release): $($ec -join ', ')"
Write-Host "exports Rust (shipped build)       : $($er -join ', ')"
if (Compare-Object $ec $er) { throw 'EXPORTS DIFFER' }
$ec2 = Get-Exports $dllCpp
if (Compare-Object $ec2 $er) { throw 'EXPORTS DIFFER (xcheck C++ copy)' }
Write-Host 'EXPORTS IDENTICAL (names and ordinals)'

# ---- 4/5. Frame Server stand-in against both DLLs
function Run-Sim($dll, $tag) {
    $env:NIWCAM_TEST_SUFFIX = "$tag$([guid]::NewGuid().ToString('N').Substring(0,10))"
    $rep = Join-Path $out "report-$tag.txt"; $tim = Join-Path $out "timing-$tag.txt"
    & $sim $dll $rep $tim | Out-Host
    if ($LASTEXITCODE -ne 0) { Write-Host "fs_sim FAILED for $tag (exit $LASTEXITCODE)"; $script:failed = $true }
    return @($rep, $tim)
}
$script:failed = $false
$cpp = Run-Sim $dllCpp 'cpp'
$rs = Run-Sim $dllRust 'rust'

$a = Get-Content $cpp[0] | Where-Object { $_ -notmatch '^#' }
$b = Get-Content $rs[0] | Where-Object { $_ -notmatch '^#' }
$d = Compare-Object $a $b
if ($d) {
    $d | Select-Object -First 40 | Out-String | Write-Host
    Write-Host "REPORTS DIFFER: $($d.Count) lines"
    $script:failed = $true
} else { Write-Host "REPORTS IDENTICAL: $($a.Count) lines" }

function Parse-Timing($f) {
    $h = @{}
    Get-Content $f | Where-Object { $_ -match '^T ' } | ForEach-Object {
        $p = $_ -split ' '
        $o = @{}
        $p[2..($p.Length - 1)] | ForEach-Object { $kv = $_ -split '='; $o[$kv[0]] = [double]$kv[1] }
        $h[$p[1]] = $o
    }
    return $h
}
$tc = Parse-Timing $cpp[1]; $tr = Parse-Timing $rs[1]
foreach ($k in $tc.Keys | Sort-Object) {
    $c = $tc[$k]; $r = $tr[$k]
    Write-Host ("{0,-26} C++: n={1,4} mean={2,6:N2}ms sd={3,5:N2} max={4,6:N2} uniq={5,4} nosig={6}   Rust: n={7,4} mean={8,6:N2}ms sd={9,5:N2} max={10,6:N2} uniq={11,4} nosig={12}" -f $k, $c.samples, $c.mean_ms, $c.stddev_ms, $c.max_ms, $c.unique_frames, $c.nosignal, $r.samples, $r.mean_ms, $r.stddev_ms, $r.max_ms, $r.unique_frames, $r.nosignal)
    $okN = [math]::Abs($c.samples - $r.samples) -le [math]::Max(4, 0.05 * $c.samples)
    $okM = [math]::Abs($c.mean_ms - $r.mean_ms) -le [math]::Max(1.5, 0.05 * $c.mean_ms)
    $okU = [math]::Abs($c.unique_frames - $r.unique_frames) -le [math]::Max(4, 0.08 * $c.unique_frames)
    $okS = ($c.nosignal -eq 0 -and $r.nosignal -eq 0)
    if (-not ($okN -and $okM -and $okU -and $okS)) { Write-Host "  TIMING OUT OF TOLERANCE for $k (count $okN, mean $okM, unique $okU, nosignal $okS)"; $script:failed = $true }
}

# ---- 6. Rust tests (registration into a test key, unit tests)
Push-Location (Join-Path $root 'rust')
try {
    $env:CARGO_TARGET_DIR = Join-Path $root 'build\rust-vcam-test'
    cargo test -p niwcam-vcam --release --features test-names --color never -- --test-threads=1
    if ($LASTEXITCODE -ne 0) { Write-Host 'cargo test failed'; $script:failed = $true }
}
finally { Pop-Location }

if ($script:failed) { Write-Host 'VCAM XCHECK: FAILED'; exit 1 }
Write-Host 'VCAM XCHECK: ALL PASS (real Frame Server loading not tested)'
