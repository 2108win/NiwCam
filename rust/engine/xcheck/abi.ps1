# ABI check of the engine DLL: struct layouts (C++ header vs Rust) and the exported function names
# (dumpbin /exports of the C++ NiwCamEngine.dll vs the Rust one, when it has been built).
# Exit code 0 = identical.
$ErrorActionPreference = 'Stop'
$root = Split-Path -Parent (Split-Path -Parent (Split-Path -Parent $PSScriptRoot))
$out = Join-Path $root 'build\xcheck-abi'
New-Item -ItemType Directory -Force $out | Out-Null
$env:Path += ";$env:USERPROFILE\.cargo\bin;${env:ProgramFiles(x86)}\Microsoft Visual Studio\Installer"
$vs = & "${env:ProgramFiles(x86)}\Microsoft Visual Studio\Installer\vswhere.exe" -latest -products * -property installationPath
$vcvars = Join-Path $vs 'VC\Auxiliary\Build\vcvars64.bat'
$cpp = Join-Path $PSScriptRoot 'abi_cpp.cpp'
$exe = Join-Path $out 'abi_cpp.exe'
$cppDll = Join-Path $root 'build\Release\NiwCamEngine.dll'
$rustDll = Join-Path $root 'build\rust-engine\release\NiwCamEngine.dll'
$bat = Join-Path $out 'abi.bat'
Set-Content -Encoding ascii $bat -Value @(
    '@echo off',
    "call `"$vcvars`" >nul",
    "cl /nologo /std:c++20 /EHsc /Fo`"$out\\`" /Fe`"$exe`" `"$cpp`" >nul",
    "dumpbin /nologo /exports `"$cppDll`" > `"$out\cpp-exports.raw`""
)
if (Test-Path $rustDll) { Add-Content -Encoding ascii $bat -Value "dumpbin /nologo /exports `"$rustDll`" > `"$out\rust-exports.raw`"" }
cmd /c $bat | Out-Null
if (-not (Test-Path $exe)) { throw 'C++ build failed' }

$env:CARGO_TARGET_DIR = Join-Path $root 'build\rust-engine'
Push-Location (Join-Path $root 'rust')
$rustLayout = cargo run -q --example abi_layout -p niwcam-engine
if ($LASTEXITCODE -ne 0) { throw 'Rust run failed' }
Pop-Location
$cppLayout = & $exe
$d = Compare-Object $cppLayout $rustLayout
$fail = 0
if ($d) { $d | Out-String | Write-Host; Write-Host "LAYOUT DIFFERENT: $($d.Count) lines"; $fail = 1 } else { Write-Host "layout identical: $($cppLayout.Count) lines" }

function Names($f) {
    Get-Content $f | ForEach-Object { if ($_ -match '^\s+\d+\s+[0-9A-F]+\s+[0-9A-F]+\s+(\S+)') { $Matches[1] } } | Where-Object { $_ -like 'cs_*' } | Sort-Object
}
if (Test-Path "$out\rust-exports.raw") {
    # Functions only the Rust engine has, added next to the C++ API (the app looks them up before calling).
    $extra = @('cs_set_background_quality')
    $a = Names "$out\cpp-exports.raw"; $b = Names "$out\rust-exports.raw" | Where-Object { $extra -notcontains $_ }
    $e = Compare-Object $a $b
    if ($e) { $e | Out-String | Write-Host; Write-Host "EXPORTS DIFFERENT: $($e.Count)"; $fail = 1 } else { Write-Host "exports identical: $($a.Count) cs_* functions" }
} else { Write-Host "(no Rust NiwCamEngine.dll yet: exports not compared; C++ has $((Names "$out\cpp-exports.raw").Count) cs_* functions)" }
exit $fail
