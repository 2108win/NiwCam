# CPU use of the filter, C++ against Rust (x64 and x86): long steady streams through the same driver.
# Needs run.ps1 to have run once (it builds the driver and the DLLs and makes the patched C++ copies).
# The percentages are of the whole driver process (host writer + sample hashing are the same for both DLLs),
# one core = 100. Process times tick every 15.6 ms, hence the 8 s windows and the repeated runs.
param([int]$Repeat = 3)
$ErrorActionPreference = 'Stop'
$root = Split-Path -Parent (Split-Path -Parent (Split-Path -Parent $PSScriptRoot))
$out = Join-Path $root 'build\xcheck-dshow'
$driverCpp = Join-Path $PSScriptRoot 'dshow_driver.cpp'
$env:Path += ";${env:ProgramFiles(x86)}\Microsoft Visual Studio\Installer"
$vs = & "${env:ProgramFiles(x86)}\Microsoft Visual Studio\Installer\vswhere.exe" -latest -products * -property installationPath
foreach ($arch in 'x64', 'x86') {
    $vc = Join-Path $vs $(if ($arch -eq 'x64') { 'VC\Auxiliary\Build\vcvars64.bat' } else { 'VC\Auxiliary\Build\vcvarsamd64_x86.bat' })
    $bat = Join-Path $out 'step.bat'
    Set-Content -Encoding ascii $bat -Value @('@echo off', "call `"$vc`" >nul", "cl /nologo /std:c++17 /O1 /EHsc /W3 /Fo`"$out\$arch\\`" /Fe`"$out\$arch\dshow_driver.exe`" `"$driverCpp`"")
    cmd /c $bat | Out-Null
}
$results = @{}
foreach ($arch in 'x64', 'x86') {
    foreach ($kind in 'cpp', 'rust') {
        $run = "$kind-$arch"
        $tag = if ($arch -eq 'x64') { if ($kind -eq 'cpp') { 'Tc64' } else { 'Tr64' } } else { if ($kind -eq 'cpp') { 'Tc86' } else { 'Tr86' } }
        $dll = "$out\$run\CamStudioDShow.dll"
        if ($kind -eq 'cpp') { $map = "Local\CamStudio${tag}_Frames_v2"; $evt = "Local\CamStudio${tag}_NewFrame_v2"; $env:NIWCAM_TEST_SUFFIX = $null }
        else { $env:NIWCAM_TEST_SUFFIX = "_$tag"; $map = "Local\CamStudioVCam_Frames_v2_$tag"; $evt = "Local\CamStudioVCam_NewFrame_v2_$tag" }
        for ($i = 0; $i -lt $Repeat; $i++) {
            foreach ($l in (& "$out\$arch\dshow_driver.exe" $dll $map $evt cpu)) {
                if ($l -match '^T (cpu\.\S+)\.cpu_pct = (\S+)$') {
                    $k = "$arch $($Matches[1])"
                    if (-not $results[$k]) { $results[$k] = @{} }
                    if (-not $results[$k][$kind]) { $results[$k][$kind] = @() }
                    $results[$k][$kind] += [double]$Matches[2]
                }
            }
        }
        Remove-Item Env:NIWCAM_TEST_SUFFIX -ErrorAction SilentlyContinue
    }
}
"{0,-26} {1,-22} {2,-22}" -f 'scenario (whole process %)', 'C++ (runs, mean)', 'Rust (runs, mean)'
foreach ($k in ($results.Keys | Sort-Object)) {
    $c = $results[$k]['cpp']; $r = $results[$k]['rust']
    "{0,-26} {1,-22} {2,-22}" -f $k, (($c | ForEach-Object { '{0:N1}' -f $_ }) -join ' ' + ('  = {0:N1}' -f ($c | Measure-Object -Average).Average)), (($r | ForEach-Object { '{0:N1}' -f $_ }) -join ' ' + ('  = {0:N1}' -f ($r | Measure-Object -Average).Average))
}
