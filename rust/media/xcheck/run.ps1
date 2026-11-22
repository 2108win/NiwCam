# niwcam-media cross-check: the C++ (src/engine, src/common) and the Rust crate on the same inputs.
#   .\run.ps1                 everything
#   .\run.ps1 -Stage decode   one of: decode, devices, audio, record
# Needs VS Build Tools and cargo. Exit code 0 = every stage matched.
# Safe on a PC in use: no real speakers (wav: target; a silent buffer to a virtual cable at most),
# no microphone, no registered camera touched, cameras are only enumerated and their formats read.
param([string[]]$Stage = @('decode', 'devices', 'audio', 'record'), [switch]$Cable, [string]$CableName = "CABLE Input")
$ErrorActionPreference = 'Stop'
$root = Split-Path -Parent (Split-Path -Parent (Split-Path -Parent $PSScriptRoot))
$out = Join-Path $root 'build\media-xcheck'
$rustTarget = Join-Path $root 'build\rust-media'
New-Item -ItemType Directory -Force $out | Out-Null
$env:Path += ";$env:USERPROFILE\.cargo\bin"

$vs = & "${env:ProgramFiles(x86)}\Microsoft Visual Studio\Installer\vswhere.exe" -latest -products * -property installationPath
$env:Path += ";${env:ProgramFiles(x86)}\Microsoft Visual Studio\Installer" # vcvars calls vswhere by name
$vcvars = Join-Path $vs 'VC\Auxiliary\Build\vcvars64.bat'

function Build-Cpp([string]$name) {
    $src = Join-Path $PSScriptRoot "$name.cpp"
    $exe = Join-Path $out "$name.exe"
    if ((Test-Path $exe) -and ((Get-Item $exe).LastWriteTime -gt (Get-ChildItem (Join-Path $root 'src') -Recurse -Include *.cpp, *.h | Sort-Object LastWriteTime -Descending | Select-Object -First 1).LastWriteTime) -and ((Get-Item $exe).LastWriteTime -gt (Get-Item $src).LastWriteTime)) { return $exe }
    $bat = Join-Path $out "build-$name.bat"
    Set-Content -Encoding ascii $bat -Value @(
        '@echo off',
        "call `"$vcvars`" >nul",
        "cl /nologo /std:c++20 /O1 /EHsc /utf-8 /W3 /DNOMINMAX /D_CRT_SECURE_NO_WARNINGS /Fo`"$out\\$name.obj`" /Fe`"$exe`" `"$src`""
    )
    cmd /c "`"$bat`"" | Out-Host
    if ($LASTEXITCODE -ne 0) { throw "C++ build failed: $name" }
    return $exe
}

function Run-Capture([string]$exe, [string[]]$args_) {
    # Native stderr must not turn into PowerShell errors in 5.1.
    $old = $ErrorActionPreference; $ErrorActionPreference = 'Continue'
    $r = & $exe @args_ 2>&1 | ForEach-Object { "$_" }
    $ErrorActionPreference = $old
    return $r
}

$failed = 0
function Compare-Lines([string]$what, $a, $b) {
    $d = Compare-Object $a $b
    if ($d) {
        $d | Select-Object -First 20 | Out-String | Write-Host
        Write-Host "DIFFERENT ($what): $($d.Count) lines" -ForegroundColor Red
        $script:failed++
    } else {
        Write-Host "IDENTICAL ($what): $($a.Count) lines" -ForegroundColor Green
    }
}

# Rust examples (release, own target dir).
$env:CARGO_TARGET_DIR = $rustTarget
Push-Location (Join-Path $root 'rust')
$old = $ErrorActionPreference; $ErrorActionPreference = 'Continue'
cargo build -q --release --examples -p niwcam-media 2>&1 | Where-Object { "$_" -notmatch 'missing `\[lints\]`|^\s*(-->|=|\||help|\d+ [~+])|generated 1 warning|^\s*$' } | Out-Host
$cargoExit = $LASTEXITCODE
$ErrorActionPreference = $old
Pop-Location
if ($cargoExit -ne 0) { throw 'cargo build failed' }
$rex = Join-Path $rustTarget 'release\examples'

if ($Stage -contains 'decode') {
    Write-Host '== VideoDecoder'
    $gen = Build-Cpp 'gen_stream'
    $cppdec = Build-Cpp 'cpp_decode'
    # name, codec, width, height, frames. 1080p H.264 has a 1088-row decoder surface (repack path).
    foreach ($c in @(@('h264-640x480', 'h264', 640, 480, 40), @('h264-1920x1080', 'h264', 1920, 1080, 40), @('hevc-1280x720', 'hevc', 1280, 720, 40), @('hevc-1920x1080', 'hevc', 1920, 1080, 30))) {
        $name = $c[0]; $bin = Join-Path $out "$name.bin"
        $g = Run-Capture $gen @($c[1], $c[2], $c[3], $c[4], $bin)
        if ($LASTEXITCODE -ne 0 -or -not (Test-Path $bin)) { Write-Host "SKIP ($name): $($g -join ' | ')" -ForegroundColor Yellow; continue }
        Write-Host "  stream $name : $($g -join ' | ')"
        $a = Run-Capture $cppdec @($c[1], $c[2], $c[3], $bin)
        $b = Run-Capture (Join-Path $rex 'decode.exe') @($c[1], $c[2], $c[3], $bin)
        $frames = @($a | Where-Object { $_ -like 'frame *' }).Count
        Write-Host "  C++ decoded $frames frames; $($a | Where-Object { $_ -like 'init*' })"
        if ($frames -eq 0) { Write-Host "  (no frames decoded by the C++: error lines are compared instead)" -ForegroundColor Yellow }
        Compare-Lines "decode $name" $a $b
    }

    # The stream changes size half way (a phone switching mode): the output type is renegotiated. And a
    # stream decoded with a wrong size given to Init.
    $b1 = Join-Path $out 'h264-640x480.bin'; $b2 = Join-Path $out 'h264-1920x1080.bin'
    if ((Test-Path $b1) -and (Test-Path $b2)) {
        $cat = Join-Path $out 'h264-640-then-1080.bin'
        [IO.File]::WriteAllBytes($cat, ([IO.File]::ReadAllBytes($b1) + [IO.File]::ReadAllBytes($b2)))
        $cat2 = Join-Path $out 'h264-1080-then-640.bin'
        [IO.File]::WriteAllBytes($cat2, ([IO.File]::ReadAllBytes($b2) + [IO.File]::ReadAllBytes($b1)))
        foreach ($c in @(@('size change 640x480 -> 1920x1080', $cat, 640, 480), @('size change 1920x1080 -> 640x480', $cat2, 1920, 1080), @('wrong size given to Init (1280x720)', $b1, 1280, 720))) {
            $a = Run-Capture $cppdec @('h264', $c[2], $c[3], $c[1])
            $b = Run-Capture (Join-Path $rex 'decode.exe') @('h264', $c[2], $c[3], $c[1])
            $sizes = ($a | Where-Object { $_ -like 'frame *' } | ForEach-Object { ($_ -split ' ')[2] } | Select-Object -Unique) -join ' then '
            Write-Host "  C++ frame sizes: $sizes"
            Compare-Lines "decode $($c[0])" $a $b
        }
    }
}

if ($Stage -contains 'devices') {
    Write-Host '== MfDevices'
    $a = Run-Capture (Build-Cpp 'cpp_mf') @()
    $b = Run-Capture (Join-Path $rex 'devices.exe') @()
    Write-Host "  C++ listed: $($a | Where-Object { $_ -like 'camera *' } | ForEach-Object { $_ -replace ' link=.*','' })"
    Compare-Lines 'devices' $a $b
}

if ($Stage -contains 'audio') {
    Write-Host '== AudioOut (wav: target, device list, bad device; no sound)'
    $dc = Join-Path $out 'audio-cpp'; $dr = Join-Path $out 'audio-rust'
    foreach ($d in @($dc, $dr)) { Remove-Item -Recurse -Force $d -ErrorAction SilentlyContinue; New-Item -ItemType Directory -Force $d | Out-Null }
    $extra = @(); if ($Cable) { $extra = @('cable', $CableName) }
    $a = Run-Capture (Build-Cpp 'cpp_audio') (@($dc) + $extra)
    $b = Run-Capture (Join-Path $rex 'audio.exe') (@($dr) + $extra)
    $a | Where-Object { $_ -notlike 'output id*' } | ForEach-Object { Write-Host "  $_" }
    Compare-Lines 'audio' $a $b
    foreach ($n in 'a.wav', 'b.wav') {
        $x = Get-FileHash (Join-Path $dc $n); $y = Get-FileHash (Join-Path $dr $n)
        if ($x.Hash -eq $y.Hash) { Write-Host "IDENTICAL BYTES ($n): $((Get-Item (Join-Path $dc $n)).Length) bytes" -ForegroundColor Green }
        else { Write-Host "DIFFERENT BYTES ($n)" -ForegroundColor Red; $failed++ }
    }
    foreach ($n in 'c.wav', 'd.wav') {
        $e1 = Test-Path (Join-Path $dc $n); $e2 = Test-Path (Join-Path $dr $n)
        if ($e1 -eq $e2) { Write-Host "SAME EXISTENCE ($n): $e1" -ForegroundColor Green } else { Write-Host "DIFFERENT EXISTENCE ($n)" -ForegroundColor Red; $failed++ }
    }
}

if ($Stage -contains 'record') {
    Write-Host '== Recorder (3 s moving picture, microphone off) and SavePng'
    $dc = Join-Path $out 'record-cpp'; $dr = Join-Path $out 'record-rust'
    foreach ($d in @($dc, $dr)) { Remove-Item -Recurse -Force $d -ErrorAction SilentlyContinue; New-Item -ItemType Directory -Force $d | Out-Null }
    $probe = Build-Cpp 'mediaprobe'
    $a = Run-Capture (Build-Cpp 'cpp_record') @($dc)
    $b = Run-Capture (Join-Path $rex 'record.exe') @($dr)
    $a | ForEach-Object { Write-Host "  C++  : $_" }
    $b | ForEach-Object { Write-Host "  Rust : $_" }
    Compare-Lines 'record API results' ($a | Where-Object { $_ -notlike 'pushed*' }) ($b | Where-Object { $_ -notlike 'pushed*' })
    # The containers: same codec, size, rate; length and frame count within the real-time jitter.
    $pa = Run-Capture $probe @('mp4', (Join-Path $dc 'rec.mp4'), 1280, 720)
    $pb = Run-Capture $probe @('mp4', (Join-Path $dr 'rec.mp4'), 1280, 720)
    $pa | ForEach-Object { Write-Host "  C++  mp4: $_" }
    $pb | ForEach-Object { Write-Host "  Rust mp4: $_" }
    # The muxer writes the frame rate it measured from the sample times (30/1 or 625000/20831): compare numerically.
    $stable = { param($l) $l | Where-Object { $_ -match '^(video-stream|audio-stream|streams|decoded-size)' } | ForEach-Object { $_ -replace ' fps \d+/\d+', '' } }
    Compare-Lines 'mp4 codec/size/streams' (& $stable $pa) (& $stable $pb)
    $fps = { param($l) $m = [regex]::Match(($l | Where-Object { $_ -like 'video-stream*' }), 'fps (\d+)/(\d+)'); [double]$m.Groups[1].Value / [double]$m.Groups[2].Value }
    $fa = & $fps $pa; $fb = & $fps $pb
    if ([math]::Abs($fa - 30) -lt 0.3 -and [math]::Abs($fb - 30) -lt 0.3) { Write-Host ("OK (mp4 frame rate): C++ {0:N3} fps, Rust {1:N3} fps (set: 30)" -f $fa, $fb) -ForegroundColor Green }
    else { Write-Host ("BAD (mp4 frame rate): C++ {0:N3}, Rust {1:N3}" -f $fa, $fb) -ForegroundColor Red; $failed++ }
    foreach ($tag in @(@('C++', $pa), @('Rust', $pb))) {
        $l = $tag[1]
        $dur = [int](($l | Where-Object { $_ -like 'duration-ms*' }) -replace 'duration-ms ', '')
        $fr = [int]((($l | Where-Object { $_ -like 'frames *' }) -split ' ')[1])
        $m = ($l | Where-Object { $_ -like 'frames-matching*' })
        $m -match 'pattern\(psnr>=25dB\) (\d+), fitted-with-bars (\d+), other (\d+), of (\d+).*backwards (\d+)' | Out-Null
        $ok = [int]$Matches[1]; $bars = [int]$Matches[2]; $other = [int]$Matches[3]; $back = [int]$Matches[5]
        # 90 frames pushed in 3 s, 6 of them 640x480 (fitted); a few may be skipped by the frame-rate rule.
        $good = ($dur -ge 2700 -and $dur -le 3500 -and $fr -ge 80 -and $fr -le 95 -and $other -eq 0 -and $bars -ge 3 -and $bars -le 6 -and $back -eq 0)
        if ($good) { Write-Host "OK ($($tag[0]) mp4): duration $dur ms, $fr frames ($ok match the picture, $bars fitted with bars), nothing else, in order" -ForegroundColor Green }
        else { Write-Host "BAD ($($tag[0]) mp4): duration $dur ms, $fr frames, $m" -ForegroundColor Red; $failed++ }
    }
    # The PNGs: decoded pixels identical (and the files too).
    foreach ($n in 'snap.png', 'snap-small.png') {
        $x = Run-Capture $probe @('png', (Join-Path $dc $n)); $y = Run-Capture $probe @('png', (Join-Path $dr $n))
        Compare-Lines "png pixels $n ($($x -join ' '))" $x $y
        if ((Get-FileHash (Join-Path $dc $n)).Hash -eq (Get-FileHash (Join-Path $dr $n)).Hash) { Write-Host "IDENTICAL BYTES ($n): $((Get-Item (Join-Path $dc $n)).Length) bytes" -ForegroundColor Green }
        else { Write-Host "(files differ byte for byte: $n)" -ForegroundColor Yellow }
    }
    foreach ($n in 'snap-tiny.png', 'snap-null.png', 'x.mp4') {
        $e1 = Test-Path (Join-Path $dc $n); $e2 = Test-Path (Join-Path $dr $n)
        if ($e1 -eq $e2) { Write-Host "SAME EXISTENCE ($n): $e1" -ForegroundColor Green } else { Write-Host "DIFFERENT EXISTENCE ($n)" -ForegroundColor Red; $failed++ }
    }
}

if ($failed) { Write-Host "FAILED: $failed stage(s) differ" -ForegroundColor Red; exit 1 }
Write-Host 'ALL MATCH'
