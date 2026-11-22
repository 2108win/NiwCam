# Makes a scratch copy of the C++ CamStudioDShow.dll whose LOCAL channel names are different, so a test never
# touches the real channel of a running NiwCam: the UTF-16 strings "Local\CamStudioVCam_Frames_v2" and
# "Local\CamStudioVCam_NewFrame_v2" get "VCam" replaced by a 4-letter tag (same length, nothing else changes).
# The original DLL is only read. Usage: patch-dll.ps1 <source dll> <target dll> <4-letter tag>
param([string]$Source, [string]$Target, [string]$Tag)
$ErrorActionPreference = 'Stop'
if ($Tag.Length -ne 4) { throw 'tag must be 4 characters' }
$bytes = [System.IO.File]::ReadAllBytes($Source)
$enc = [System.Text.Encoding]::Unicode
function Replace-All([byte[]]$data, [string]$from, [string]$to) {
    $f = $enc.GetBytes($from); $t = $enc.GetBytes($to)
    if ($f.Length -ne $t.Length) { throw 'length differs' }
    $n = 0
    for ($i = 0; $i -le $data.Length - $f.Length; $i++) {
        if ($data[$i] -ne $f[0]) { continue }
        $ok = $true
        for ($j = 1; $j -lt $f.Length; $j++) { if ($data[$i + $j] -ne $f[$j]) { $ok = $false; break } }
        if ($ok) { [Array]::Copy($t, 0, $data, $i, $t.Length); $n++; $i += $f.Length - 1 }
    }
    return $n
}
$a = Replace-All $bytes 'Local\CamStudioVCam_Frames_v2' ("Local\CamStudio${Tag}_Frames_v2")
$b = Replace-All $bytes 'Local\CamStudioVCam_NewFrame_v2' ("Local\CamStudio${Tag}_NewFrame_v2")
if ($a -ne 1 -or $b -ne 1) { throw "expected one occurrence of each name, found $a and $b" }
New-Item -ItemType Directory -Force (Split-Path -Parent $Target) | Out-Null
[System.IO.File]::WriteAllBytes($Target, $bytes)
Write-Host "patched $Target ($a + $b names)"
