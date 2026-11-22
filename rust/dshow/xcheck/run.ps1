# Proves the Rust DirectShow filter (rust/dshow) against the C++ one (src/dshow), x64 and x86.
#
#   .\rust\dshow\xcheck\run.ps1 [-Fast] [-SkipBuild]
#
# 1. builds the C++ test driver (dshow_driver.cpp) for x64 and x86 (MSVC) and the Rust DLLs: the default
#    ones (the shipping build) and the test-names ones (object names get NIWCAM_TEST_SUFFIX appended);
# 2. compares `dumpbin /exports` of the C++ DLLs (build\Release, build-x86\Release) with the Rust ones;
# 3. makes scratch copies of the C++ DLLs with other LOCAL channel names (patch-dll.ps1: the originals are only
#    read), so nothing here touches the real Local\CamStudioVCam_* objects of a running NiwCam;
# 4. runs the same driver scenario against the C++ and the Rust DLL (x64 and x86): nothing is registered,
#    the DLL is loaded by path and the filter taken from its class factory;
# 5. diffs the deterministic lines ("D ...": return codes, media types, hashes of delivered frames, states) and
#    compares the timing lines ("T ...": fps, gaps) with a tolerance.
# Exit code 0 = no deterministic difference and timings within tolerance. Needs VS Build Tools and cargo.
param([switch]$Fast, [switch]$SkipBuild)
$ErrorActionPreference = 'Stop'
$root = Split-Path -Parent (Split-Path -Parent (Split-Path -Parent $PSScriptRoot))
$out = Join-Path $root 'build\xcheck-dshow'
New-Item -ItemType Directory -Force $out, "$out\x64", "$out\x86" | Out-Null
$env:Path += ";$env:USERPROFILE\.cargo\bin"
$vs = & "${env:ProgramFiles(x86)}\Microsoft Visual Studio\Installer\vswhere.exe" -latest -products * -property installationPath
$env:Path += ";${env:ProgramFiles(x86)}\Microsoft Visual Studio\Installer"
$vcvars64 = Join-Path $vs 'VC\Auxiliary\Build\vcvars64.bat'
$vcvars86 = Join-Path $vs 'VC\Auxiliary\Build\vcvarsamd64_x86.bat'
$driverCpp = Join-Path $PSScriptRoot 'dshow_driver.cpp'
$failed = $false

function Invoke-Vc([string]$vcvars, [string[]]$lines) {
    $bat = Join-Path $out 'step.bat'
    Set-Content -Encoding ascii $bat -Value (@('@echo off', "call `"$vcvars`" >nul") + $lines)
    cmd /c $bat
    if ($LASTEXITCODE -ne 0) { throw "step failed: $($lines -join ' & ')" }
}

# ---- 1. builds -------------------------------------------------------------------------------
if (-not $SkipBuild) {
    foreach ($arch in 'x64', 'x86') {
        $vc = if ($arch -eq 'x64') { $vcvars64 } else { $vcvars86 }
        Invoke-Vc $vc @("cl /nologo /std:c++17 /O1 /EHsc /W3 /Fo`"$out\$arch\\`" /Fe`"$out\$arch\dshow_driver.exe`" `"$driverCpp`"")
    }
    Push-Location (Join-Path $root 'rust')
    try {
        $env:CARGO_TARGET_DIR = Join-Path $root 'build\rust-dshow'
        cargo build -q -p niwcam-dshow --release; if ($LASTEXITCODE) { throw 'cargo build x64 failed' }
        cargo build -q -p niwcam-dshow --release --target i686-pc-windows-msvc; if ($LASTEXITCODE) { throw 'cargo build x86 failed' }
        $env:CARGO_TARGET_DIR = Join-Path $root 'build\rust-dshow-tn'
        cargo build -q -p niwcam-dshow --release --features test-names; if ($LASTEXITCODE) { throw 'cargo build test-names x64 failed' }
        cargo build -q -p niwcam-dshow --release --features test-names --target i686-pc-windows-msvc; if ($LASTEXITCODE) { throw 'cargo build test-names x86 failed' }
    } finally { Pop-Location; Remove-Item Env:CARGO_TARGET_DIR -ErrorAction SilentlyContinue }
}

$cppDll = @{ x64 = "$root\build\Release\CamStudioDShow.dll"; x86 = "$root\build-x86\Release\CamStudioDShow.dll" }
$rustDll = @{ x64 = "$root\build\rust-dshow\release\CamStudioDShow.dll"; x86 = "$root\build\rust-dshow\i686-pc-windows-msvc\release\CamStudioDShow.dll" }
$rustTn = @{ x64 = "$root\build\rust-dshow-tn\release\CamStudioDShow.dll"; x86 = "$root\build\rust-dshow-tn\i686-pc-windows-msvc\release\CamStudioDShow.dll" }

# ---- 2. exports ------------------------------------------------------------------------------
function Get-Exports([string]$dll, [string]$vcvars) {
    $bat = Join-Path $out 'dump.bat'
    Set-Content -Encoding ascii $bat -Value @('@echo off', "call `"$vcvars`" >nul", "dumpbin /exports `"$dll`"", "dumpbin /headers `"$dll`" | findstr /i /c:`"machine`"")
    $text = cmd /c $bat
    $names = @($text | ForEach-Object { if ($_ -match '^\s+\d+\s+[0-9A-F]+\s+[0-9A-F]{8}\s+(\S+)') { $Matches[1] } })
    $machine = ($text | Select-String 'machine' | Select-Object -First 1).Line.Trim()
    [pscustomobject]@{ Names = $names; Machine = $machine }
}
Write-Host '== exports (dumpbin /exports) =='
foreach ($arch in 'x64', 'x86') {
    $vc = $vcvars64
    $c = Get-Exports $cppDll[$arch] $vc
    $r = Get-Exports $rustDll[$arch] $vc
    $same = (($c.Names -join ',') -eq ($r.Names -join ','))
    Write-Host ("{0}: C++ [{1}] ({2})   Rust [{3}] ({4})  -> {5}" -f $arch, ($c.Names -join ' '), $c.Machine, ($r.Names -join ' '), $r.Machine, $(if ($same -and $c.Names.Count -eq 4) { 'IDENTICAL' } else { 'DIFFERENT' }))
    if (-not $same -or $c.Names.Count -ne 4) { $failed = $true }
}
# the shipping build has no test hook; the test-names build has it
function Contains-Text([string]$file, [string]$text) {
    $bytes = [System.IO.File]::ReadAllBytes($file)
    foreach ($enc in [System.Text.Encoding]::ASCII, [System.Text.Encoding]::Unicode) {
        $pat = $enc.GetBytes($text)
        for ($i = 0; $i -le $bytes.Length - $pat.Length; $i++) {
            if ($bytes[$i] -ne $pat[0]) { continue }
            $ok = $true
            for ($j = 1; $j -lt $pat.Length; $j++) { if ($bytes[$i + $j] -ne $pat[$j]) { $ok = $false; break } }
            if ($ok) { return $true }
        }
    }
    return $false
}
foreach ($arch in 'x64', 'x86') {
    $d = Contains-Text $rustDll[$arch] 'NIWCAM_TEST_SUFFIX'
    $t = Contains-Text $rustTn[$arch] 'NIWCAM_TEST_SUFFIX'
    Write-Host ("{0}: NIWCAM_TEST_SUFFIX in the shipping DLL: {1} (must be False); in the test-names DLL: {2} (must be True)" -f $arch, $d, $t)
    if ($d -or -not $t) { $failed = $true }
}

# ---- 3 + 4. scenario runs --------------------------------------------------------------------
$runs = @(
    @{ Name = 'cpp-x64'; Arch = 'x64'; Kind = 'cpp'; Tag = 'Tc64' },
    @{ Name = 'rust-x64'; Arch = 'x64'; Kind = 'rust'; Tag = 'Tr64' },
    @{ Name = 'cpp-x86'; Arch = 'x86'; Kind = 'cpp'; Tag = 'Tc86' },
    @{ Name = 'rust-x86'; Arch = 'x86'; Kind = 'rust'; Tag = 'Tr86' }
)
$driverArgs = if ($Fast) { 'fast' } else { $null }
foreach ($run in $runs) {
    $dir = "$out\$($run.Name)"
    New-Item -ItemType Directory -Force $dir | Out-Null
    $dll = "$dir\CamStudioDShow.dll"
    $env:NIWCAM_TEST_SUFFIX = $null
    if ($run.Kind -eq 'cpp') {
        & "$PSScriptRoot\patch-dll.ps1" -Source $cppDll[$run.Arch] -Target $dll -Tag $run.Tag | Out-Null
        $map = "Local\CamStudio$($run.Tag)_Frames_v2"; $evt = "Local\CamStudio$($run.Tag)_NewFrame_v2"
    } else {
        Copy-Item $rustTn[$run.Arch] $dll -Force
        $suffix = "_$($run.Tag)"
        $env:NIWCAM_TEST_SUFFIX = $suffix
        $map = "Local\CamStudioVCam_Frames_v2$suffix"; $evt = "Local\CamStudioVCam_NewFrame_v2$suffix"
    }
    Write-Host ("== running {0} (channel {1}) ==" -f $run.Name, $map)
    $sw = [Diagnostics.Stopwatch]::StartNew()
    $arguments = @($dll, $map, $evt) + @($driverArgs | Where-Object { $_ })
    & "$out\$($run.Arch)\dshow_driver.exe" @arguments | Set-Content -Encoding ascii "$out\$($run.Name).txt"
    if ($LASTEXITCODE -ne 0) { Write-Host "  driver exit code $LASTEXITCODE"; $failed = $true }
    Remove-Item Env:NIWCAM_TEST_SUFFIX -ErrorAction SilentlyContinue
    Write-Host ("  {0} lines in {1:N0} s" -f (Get-Content "$out\$($run.Name).txt").Count, $sw.Elapsed.TotalSeconds)
}

# ---- 5. compare ------------------------------------------------------------------------------
function Get-Lines([string]$name, [string]$prefix) {
    Get-Content "$out\$name.txt" | Where-Object { $_ -match "^$prefix " }
}
function Get-Timing([string]$name) {
    $h = @{}
    foreach ($l in (Get-Lines $name 'T')) { if ($l -match '^T (\S+) = (\S+)$') { $h[$Matches[1]] = [double]$Matches[2] } }
    $h
}
foreach ($arch in 'x64', 'x86') {
    $c = Get-Lines "cpp-$arch" 'D'; $r = Get-Lines "rust-$arch" 'D'
    $diff = Compare-Object $c $r
    Write-Host ("== {0}: deterministic lines: C++ {1}, Rust {2}: {3} ==" -f $arch, $c.Count, $r.Count, $(if ($diff) { "$($diff.Count) DIFFERENT" } else { 'IDENTICAL' }))
    if ($diff) { $failed = $true; $diff | Select-Object -First 40 | ForEach-Object { Write-Host ("  {0} {1}" -f $_.SideIndicator, $_.InputObject.Substring(0, [Math]::Min(250, $_.InputObject.Length))) } }
    $tc = Get-Timing "cpp-$arch"; $tr = Get-Timing "rust-$arch"
    $bad = 0; $rows = @()
    foreach ($k in ($tc.Keys | Sort-Object)) {
        if (-not $tr.ContainsKey($k)) { $bad++; continue }
        $a = $tc[$k]; $b = $tr[$k]
        $ok = $true
        if ($k -match '\.fps$') { $ok = [Math]::Abs($a - $b) -le [Math]::Max(1.5, 0.15 * $a) }
        elseif ($k -match 'gap_mean_ms$') { $ok = [Math]::Abs($a - $b) -le [Math]::Max(4.0, 0.15 * $a) }
        elseif ($k -match 'gap_max_ms$') { $ok = $b -le (2 * $a + 40) }   # a scheduling hiccup is noise: only a much worse maximum counts
        elseif ($k -match 'cpu_pct$') { $ok = $b -le (1.5 * $a + 3.0) }   # whole driver process, % of one core
        elseif ($k -match '_s$') { $ok = [Math]::Abs($a - $b) -le 0.35 }
        if (-not $ok) { $bad++ }
        $rows += [pscustomobject]@{ Key = $k; Cpp = $a; Rust = $b; Ok = $ok }
    }
    $rows | Where-Object { $_.Key -match '\.fps$|first_nosignal|last_live|cpu_pct' } | Format-Table -AutoSize | Out-String -Width 200 | Write-Host
    Write-Host ("== {0}: timing lines outside tolerance: {1} of {2} ==" -f $arch, $bad, $rows.Count)
    $rows | Where-Object { -not $_.Ok } | Format-Table -AutoSize | Out-String -Width 200 | Write-Host
    if ($bad) { $failed = $true }
}
# the two architectures of the C++ DLL against each other (information only: x86 and x64 may differ in sizeof)
$cx = Get-Lines 'cpp-x64' 'D'; $cy = Get-Lines 'cpp-x86' 'D'
$dxy = Compare-Object $cx $cy
Write-Host ("(info) C++ x64 vs C++ x86 deterministic lines: {0} different" -f @($dxy).Count)
if ($failed) { Write-Host 'RESULT: DIFFERENCES FOUND'; exit 1 }
Write-Host 'RESULT: Rust == C++ (exports, every deterministic line, timings in tolerance), x64 and x86'
