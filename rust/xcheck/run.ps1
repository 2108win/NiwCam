# Builds the C++ reference driver (MSVC) and the Rust one, runs both, diffs the lines.
# Run from anywhere; needs VS Build Tools and cargo. Exit code 0 = identical.
$ErrorActionPreference = 'Stop'
$root = Split-Path -Parent (Split-Path -Parent $PSScriptRoot)
$out = Join-Path $root 'build\xcheck'
New-Item -ItemType Directory -Force $out | Out-Null
$env:Path += ";$env:USERPROFILE\.cargo\bin"

$vs = & "${env:ProgramFiles(x86)}\Microsoft Visual Studio\Installer\vswhere.exe" -latest -products * -property installationPath
$env:Path += ";${env:ProgramFiles(x86)}\Microsoft Visual Studio\Installer" # vcvars calls vswhere by name
$cpp = Join-Path $PSScriptRoot 'cpp_hashes.cpp'
$vcvars = Join-Path $vs 'VC\Auxiliary\Build\vcvars64.bat'
$exe = Join-Path $out 'cpp_hashes.exe'
$bat = Join-Path $out 'build.bat'
Set-Content -Encoding ascii $bat -Value @(
    '@echo off',
    "call `"$vcvars`" >nul",
    "cl /nologo /std:c++20 /O1 /EHsc /Fo`"$out\\`" /Fe`"$exe`" `"$cpp`""
)
cmd /c $bat
if ($LASTEXITCODE -ne 0) { throw 'C++ build failed' }

Push-Location (Join-Path $root 'rust')
cargo run -q --release --example hashes | Set-Content -Encoding ascii (Join-Path $out 'rust.txt')
if ($LASTEXITCODE -ne 0) { throw 'Rust run failed' }
Pop-Location
& $exe | Set-Content -Encoding ascii (Join-Path $out 'cpp.txt')

$a = Get-Content (Join-Path $out 'cpp.txt'); $b = Get-Content (Join-Path $out 'rust.txt')
$d = Compare-Object $a $b
if ($d) { $d | Select-Object -First 30 | Out-String | Write-Host; Write-Host "DIFFERENT: $($d.Count) lines"; exit 1 }
Write-Host "IDENTICAL: $($a.Count) lines"
