# Registers a vcam DLL for the real Frame Server (needs Administrator; the caller clicks the UAC prompt)
# and puts the previous registration back. Nothing is deleted: the installed C++ DLL stays where it is.
#   frameserver.ps1 -Register <dll>     copy the dll next to the installed one under a unique name, regsvr32 it
#   frameserver.ps1 -Restore            regsvr32 the DLL that was registered before -Register
param([string]$Register = '', [switch]$Restore)
$ErrorActionPreference = 'Stop'
$root = Split-Path -Parent (Split-Path -Parent (Split-Path -Parent $PSScriptRoot))
$log = Join-Path $root 'build\frameserver.log'
$prev = Join-Path $root 'build\vcam-previous.txt'
Start-Transcript -Path $log -Append | Out-Null
try {
    $principal = New-Object Security.Principal.WindowsPrincipal([Security.Principal.WindowsIdentity]::GetCurrent())
    if (-not $principal.IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)) { throw 'Run elevated.' }
    $key = 'HKLM:\SOFTWARE\Classes\CLSID\{72F5B2FD-C48E-449B-95CE-E8F13BF200CB}\InprocServer32'
    $current = (Get-ItemProperty $key).'(default)'
    foreach ($svc in 'FrameServerMonitor', 'FrameServer') {
        $s = Get-Service $svc -ErrorAction SilentlyContinue
        if ($s -and $s.Status -eq 'Running') { Stop-Service $svc -Force }
    }
    if ($Register) {
        if (-not (Test-Path $prev)) { Set-Content -Encoding ascii $prev $current }
        $stamp = Get-Date -Format 'yyyyMMdd-HHmmss'
        $target = Join-Path (Join-Path $env:ProgramFiles 'CamStudio') "CamStudioVCam-rust-$stamp.dll"
        Copy-Item $Register $target
        $p = Start-Process "$env:SystemRoot\System32\regsvr32.exe" -ArgumentList '/s', "`"$target`"" -Wait -PassThru
        if ($p.ExitCode -ne 0) { throw "regsvr32 failed: $($p.ExitCode)" }
    } elseif ($Restore) {
        if (-not (Test-Path $prev)) { throw 'nothing to restore' }
        $old = (Get-Content $prev).Trim()
        $p = Start-Process "$env:SystemRoot\System32\regsvr32.exe" -ArgumentList '/s', "`"$old`"" -Wait -PassThru
        if ($p.ExitCode -ne 0) { throw "regsvr32 failed: $($p.ExitCode)" }
        Remove-Item $prev -Force
        Get-ChildItem (Join-Path $env:ProgramFiles 'CamStudio') -Filter 'CamStudioVCam-rust-*.dll' | ForEach-Object { try { Remove-Item $_.FullName -Force } catch { Write-Host "still in use: $($_.Name)" } }
    }
    $reg = Get-ItemProperty $key
    Write-Host "Registered now: $($reg.'(default)') (ThreadingModel=$($reg.ThreadingModel))"
} finally { Stop-Transcript | Out-Null }
