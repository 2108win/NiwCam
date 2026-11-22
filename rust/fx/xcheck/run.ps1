# Cross-check of rust/fx against the C++ it ports (StillPicture, Segmenter, FaceDetect, WindowSource).
# Builds the C++ reference driver from the real src/engine sources (CMake flags) and the Rust driver
# (fx/examples/fx_rust), generates the inputs, runs every case in both and diffs the outputs.
#   .\run.ps1                     everything
#   .\run.ps1 -Only still,blend   some parts (strings, still, blend, seg, face, window, closewatch, cpu)
# Needs: VS Build Tools, cargo, python, ffmpeg (inputs + PNG decoding). The window parts show a small
# borderless test window of ours for a few seconds each, in the bottom right corner of the work area, below
# every other window and never activated (a window off screen is not composed, its frames would be empty).
# The face / segmenter inputs come from a stock portrait that ships with Windows (read in place,
# never copied into the repo); without it those parts are skipped.
# Exit code 0 = every selected part identical.
param(
    [string[]]$Only = @('strings', 'still', 'blend', 'seg', 'face', 'window', 'closewatch', 'cpu'),
    [string]$WindowX = 'auto',
    [string]$WindowY = 'auto'
)
$ErrorActionPreference = 'Continue' # native tools write warnings to stderr; exit codes are checked by hand
$here = $PSScriptRoot
$root = Split-Path -Parent (Split-Path -Parent (Split-Path -Parent $here))
$out = Join-Path $root 'build\xcheck-fx'
$in = Join-Path $out 'in'
New-Item -ItemType Directory -Force $out | Out-Null
$env:Path += ";$env:USERPROFILE\.cargo\bin"

$python = (Get-Command python -ErrorAction Stop).Source
$ffmpeg = (Get-Command ffmpeg -ErrorAction Stop).Source
$model = Join-Path $root 'app\Assets\Models\pphumanseg.onnx'

Write-Host '== inputs'
& $python -I (Join-Path $here 'gen_inputs.py') $in $ffmpeg | Out-Host
if ($LASTEXITCODE -ne 0) { throw 'input generation failed' }

Write-Host '== build C++ driver'
& (Join-Path $here 'build_cpp.ps1') -Out $out | Out-Null
if ($LASTEXITCODE -ne 0) { throw 'C++ build failed' }
Write-Host '== build Rust driver'
$env:CARGO_TARGET_DIR = Join-Path $root 'build\rust-fx'
Push-Location (Join-Path $root 'rust')
cargo build -q --release -p niwcam-fx --example fx_rust
if ($LASTEXITCODE -ne 0) { Pop-Location; throw 'Rust build failed' }
Pop-Location
$cppExe = Join-Path $out 'fx_cpp.exe'
$rustExe = Join-Path $env:CARGO_TARGET_DIR 'release\examples\fx_rust.exe'

function Run-Both($name, $extra) {
    foreach ($side in 'cpp', 'rust') {
        $exe = if ($side -eq 'cpp') { $cppExe } else { $rustExe }
        $dir = Join-Path (Join-Path $out $side) $name
        if (Test-Path $dir) { Remove-Item -Recurse -Force $dir }
        New-Item -ItemType Directory -Force $dir | Out-Null
        $cmd = if ($name -eq 'closewatch') { 'closewatch' } else { $name }
        & $exe $cmd $in $dir @extra 2>&1 | Where-Object { $_ -notmatch 'onnxruntime|^core \d|^$|^ +blend' } | Out-Null
    }
    & $python -I (Join-Path $here 'compare.py') $name (Join-Path (Join-Path $out 'cpp') $name) (Join-Path (Join-Path $out 'rust') $name) $ffmpeg | Out-Host
    return $LASTEXITCODE
}

$failed = @()
foreach ($part in $Only) {
    Write-Host "== $part"
    $rc = 0
    switch ($part) {
        'still' { $rc = Run-Both 'still' @() }
        'blend' { $rc = Run-Both 'blend' @() }
        'seg'   { $rc = Run-Both 'seg' @($model) }
        'face'  {
            if (Test-Path (Join-Path $in 'face_one_1280x720.nv12')) { $rc = Run-Both 'face' @() } else { Write-Host '   skipped: no portrait image on this PC' }
        }
        'strings' {
            # The Vietnamese the user sees: byte for byte in the Rust files too.
            & $python -I (Join-Path $here 'check_strings.py') $root | Out-Host
            $rc = $LASTEXITCODE
        }
        'cpu' {
            # Informational: CPU time per BlendBackground call, serial and with every core (not compared).
            foreach ($side in 'cpp', 'rust') {
                $exe = if ($side -eq 'cpp') { $cppExe } else { $rustExe }
                $dir = Join-Path (Join-Path $out $side) 'cpu'
                New-Item -ItemType Directory -Force $dir | Out-Null
                & $exe cpu $in $dir | Out-Null
                Write-Host "   --- $side"
                Get-Content (Join-Path $dir 'cpu.txt') | ForEach-Object { Write-Host "   $_" }
            }
        }
        'window' { $rc = Run-Both 'window' @($WindowX, $WindowY) }
        'closewatch' {
            # A window of another process (the C++ host), watched by each capturer in turn: what does Next()
            # say when it is destroyed, with no message pump on the capturing thread?
            foreach ($side in 'cpp', 'rust') {
                $exe = if ($side -eq 'cpp') { $cppExe } else { $rustExe }
                $hostDir = Join-Path $out "host-$side"
                if (Test-Path $hostDir) { Remove-Item -Recurse -Force $hostDir }
                New-Item -ItemType Directory -Force $hostDir | Out-Null
                $hostProc = Start-Process -FilePath $cppExe -ArgumentList "windowhost `"$in`" `"$hostDir`" $WindowX $WindowY 6" -PassThru -WindowStyle Hidden
                for ($i = 0; $i -lt 100 -and -not (Test-Path (Join-Path $hostDir 'hwnd.txt')); $i++) { Start-Sleep -Milliseconds 50 }
                Start-Sleep -Milliseconds 300
                $hwnd = (Get-Content (Join-Path $hostDir 'hwnd.txt') | Select-Object -First 1).Trim()
                $dir = Join-Path (Join-Path $out $side) 'closewatch'
                if (Test-Path $dir) { Remove-Item -Recurse -Force $dir }
                New-Item -ItemType Directory -Force $dir | Out-Null
                & $exe closewatch $in $dir $hwnd | Out-Null
                $hostProc.WaitForExit(15000) | Out-Null
            }
            $a = (Get-Content (Join-Path $out 'cpp\closewatch\results.txt')) -join ' | '
            $b = (Get-Content (Join-Path $out 'rust\closewatch\results.txt')) -join ' | '
            Write-Host "   C++ : $a"
            Write-Host "   Rust: $b"
            Write-Host ("   C++ timing : " + ((Get-Content (Join-Path $out 'cpp\closewatch\timing.txt')) -join ' '))
            Write-Host ("   Rust timing: " + ((Get-Content (Join-Path $out 'rust\closewatch\timing.txt')) -join ' '))
            if ($a -ne $b) { $rc = 1 }
        }
    }
    if ($rc -ne 0) { $failed += $part }
}
Write-Host ''
if ($failed.Count) { Write-Host "DIFFERENT: $($failed -join ', ')"; exit 1 }
Write-Host 'ALL IDENTICAL'
