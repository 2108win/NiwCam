# Compares the Rust phone crate with the C++ engine on the same inputs. Run from anywhere:
#   .\rust\phone\xcheck\run.ps1 [-SkipBuild] [-Long]
# Needs VS Build Tools + cargo + build\Release\niwcam-fake-phone.exe / niwcam-usb-check.exe (the C++ build).
# Uses loopback port 47999 (never 47321/47322), a temp data folder and temp working directories, and
# never starts or stops an adb server, nor a process it did not start.
# Exit code 0 = everything matched.
param([switch]$SkipBuild, [switch]$Long)
$ErrorActionPreference = 'Continue'
$root = Split-Path -Parent (Split-Path -Parent (Split-Path -Parent $PSScriptRoot))
$out = Join-Path $root 'build\xcheck-phone'
$env:Path += ";$env:USERPROFILE\.cargo\bin"
$env:CARGO_TARGET_DIR = Join-Path $root 'build\rust-phone'
if (-not $SkipBuild) {
    & (Join-Path $PSScriptRoot 'build_cpp.ps1')
    Push-Location (Join-Path $root 'rust')
    cargo build -q -p niwcam-phone --examples
    if ($LASTEXITCODE -ne 0) { throw 'cargo build failed' }
    Pop-Location
}
$cpp = Join-Path $out 'cpp_phone.exe'
$rs = Join-Path $env:CARGO_TARGET_DIR 'debug\examples\phone_driver.exe'
$fakeExe = Join-Path $root 'build\Release\niwcam-fake-phone.exe'
$work = Join-Path $out ('run-' + $PID)
Remove-Item -Recurse -Force $work -ErrorAction SilentlyContinue
New-Item -ItemType Directory -Force $work | Out-Null
$PORT = 47999
$bad = 0

function Lines($exe, [string[]]$a) {
    # stdout lines without CR, stderr dropped
    & $exe @a 2>$null | ForEach-Object { $_.ToString().TrimEnd("`r") }
}
function Check($name, $a, $b) {
    $d = Compare-Object @($a) @($b)
    if ($d) { Write-Host "DIFFERENT  $name"; $d | Out-String | Write-Host; $script:bad++ }
    else { Write-Host "identical  $name ($(@($a).Count) lines)" }
}
function CheckTrue($name, $cond, $detail = '') {
    if ($cond) { Write-Host "ok         $name" } else { Write-Host "FAILED     $name $detail"; $script:bad++ }
}
function Start-Fake($cwd, [string[]]$extra = @()) {
    New-Item -ItemType Directory -Force $cwd | Out-Null
    $log = Join-Path $cwd ('fake-' + [guid]::NewGuid().ToString('N').Substring(0, 6) + '.log')
    $p = Start-Process -FilePath $fakeExe -ArgumentList (@('--port', $PORT) + $extra) -WorkingDirectory $cwd -RedirectStandardOutput $log -PassThru -WindowStyle Hidden
    for ($i = 0; $i -lt 100; $i++) { if ((Test-Path $log) -and ((Get-Content $log -Raw) -match 'listening')) { break }; Start-Sleep -Milliseconds 100 }
    return @{ Proc = $p; Log = $log }
}
function Stop-Fake($f) {
    Stop-Process -Id $f.Proc.Id -Force -ErrorAction SilentlyContinue
    $f.Proc.WaitForExit(3000) | Out-Null
    return (Get-Content $f.Log -ErrorAction SilentlyContinue)
}

# ---- 1. data folder, identity -------------------------------------------------------------
$d1 = Join-Path $work 'data1'
$env:CAMSTUDIO_DATA_DIR = $d1
Write-Host '== data folder and identity'
Check 'datadir (CAMSTUDIO_DATA_DIR honoured)' (Lines $cpp @('datadir')) (Lines $rs @('datadir'))
Check 'clientname' (Lines $cpp @('clientname')) (Lines $rs @('clientname'))
$id1 = Lines $cpp @('clientid')            # the C++ creates client-id.txt
Check 'clientid (Rust reads the C++ file)' $id1 (Lines $rs @('clientid'))
Remove-Item Env:CAMSTUDIO_DATA_DIR
# Without the variable the C++ default is %LOCALAPPDATA%NiwCam (a typo in AppDataDir.h, copied on purpose).
Check 'datadir default without CAMSTUDIO_DATA_DIR (the C++ typo copied)' (Lines $cpp @('datadir')) (Lines $rs @('datadir'))
Write-Host ('   ' + (Lines $rs @('datadir')))
$env:CAMSTUDIO_DATA_DIR = $d1

# ---- 2. key store, both directions --------------------------------------------------------
Write-Host '== pairing key store'
$k1 = '00112233445566778899aabbccddeeff00112233445566778899aabbccddeeff'
$k2 = 'ffeeddccbbaa99887766554433221100ffeeddccbbaa99887766554433221100'
$null = Lines $cpp @('keysave', 'DEV-A-1', $k1)
Check 'C++ saved, Rust loads' @("key=$k1") (Lines $rs @('keyload', 'DEV-A-1'))
$null = Lines $rs @('keysave', 'DEV-2', $k2)
Check 'Rust saved, C++ loads' @("key=$k2") (Lines $cpp @('keyload', 'DEV-2'))
$sha = [System.Security.Cryptography.SHA256]::Create()
foreach ($id in 'DEV-A-1', 'DEV-2') {
    $h = ([BitConverter]::ToString($sha.ComputeHash([Text.Encoding]::UTF8.GetBytes($id))) -replace '-', '').ToLower().Substring(0, 32)
    CheckTrue "pairings\$h.key exists for $id" (Test-Path (Join-Path $d1 "pairings\$h.key"))
}
Check 'has: C++ file seen by Rust' @('has=1') (Lines $rs @('keyhas', 'DEV-A-1'))
Check 'has: Rust file seen by C++' @('has=1') (Lines $cpp @('keyhas', 'DEV-2'))
$null = Lines $rs @('keyforget', 'DEV-A-1')
$null = Lines $cpp @('keyforget', 'DEV-2')
Check 'forget: Rust deleted the C++ file' @('has=0') (Lines $cpp @('keyhas', 'DEV-A-1'))
Check 'forget: C++ deleted the Rust file' @('has=0') (Lines $rs @('keyhas', 'DEV-2'))

# ---- 3. pairing with the fake phone: no new code needed the other way -----------------------
Write-Host '== pairing with the fake phone (C++ pairs, Rust connects; Rust pairs, C++ connects)'
$f = Start-Fake (Join-Path $work 'fpA')
$c = Lines $cpp @('connect', '127.0.0.1', $PORT, 'pair')
Write-Host "   C++:  $c"
$r = Lines $rs @('connect', '127.0.0.1', $PORT, 'known')
Write-Host "   Rust: $r"
$log = Stop-Fake $f
CheckTrue 'A: Rust connected with the key the C++ saved' ($r -match 'connect ok')
CheckTrue 'A: exactly one pairing on the phone' ((@($log | Where-Object { $_ -match 'Pairing code' })).Count -eq 1)
CheckTrue 'A: the code the C++ showed is the one the phone printed' ((@($log | Where-Object { $_ -match 'Pairing code: (\d{6})' }) -join '') -match ($c | Where-Object { $_ -match '^pairing code' } | ForEach-Object { $_ -replace 'pairing code ', '' }))

$d2 = Join-Path $work 'data2'
$env:CAMSTUDIO_DATA_DIR = $d2
$f = Start-Fake (Join-Path $work 'fpB')
$r = Lines $rs @('connect', '127.0.0.1', $PORT, 'pair')
Write-Host "   Rust: $r"
$c = Lines $cpp @('connect', '127.0.0.1', $PORT, 'known')
Write-Host "   C++:  $c"
$log = Stop-Fake $f
CheckTrue 'B: C++ connected with the key Rust saved' ($c -match 'connect ok')
CheckTrue 'B: exactly one pairing on the phone' ((@($log | Where-Object { $_ -match 'Pairing code' })).Count -eq 1)
Check 'B: client id the same in both' (Lines $cpp @('clientid')) (Lines $rs @('clientid'))
CheckTrue 'B: the code Rust showed is the one the phone printed' ((@($log | Where-Object { $_ -match 'Pairing code: (\d{6})' }) -join '') -match ($r | Where-Object { $_ -match '^pairing code' } | ForEach-Object { $_ -replace 'pairing code ', '' }))

# ---- 4. the same stream through both ------------------------------------------------------
Write-Host '== stream 100 frames through C++ PhoneLink and Rust PhoneLink'
$env:CAMSTUDIO_DATA_DIR = $d1
$f = Start-Fake (Join-Path $work 'fpA') @('--audio')   # fpA holds the key the C++ paired with
$cs = Lines $cpp @('stream', '127.0.0.1', $PORT, '100', 'audio')
$rr = Lines $rs @('stream', '127.0.0.1', $PORT, '100', 'audio')
Check 'stream with audio' $cs $rr
$cs2 = Lines $cpp @('stream', '127.0.0.1', $PORT, '100')
$rr2 = Lines $rs @('stream', '127.0.0.1', $PORT, '100')
Check 'stream without audio' $cs2 $rr2
$log = Stop-Fake $f
$cl = @($log | Where-Object { $_ -match '^(start|set|stats):' })
CheckTrue 'phone log: C++ and Rust sent the same start/set/stats text (audio run)' ($cl.Count -eq 16 -and (($cl[0..3]) -join '|') -eq (($cl[4..7]) -join '|')) (($cl -join '|'))
CheckTrue 'phone log: C++ and Rust sent the same start/set/stats text (plain run)' ($cl.Count -eq 16 -and (($cl[8..11]) -join '|') -eq (($cl[12..15]) -join '|'))

# ---- 5. manual phone list ---------------------------------------------------------------------
Write-Host '== manual phones (phones.txt)'
Check 'known/AddManual/RemoveManual' (Lines $cpp @('known', '127.0.0.1', '47999')) (Lines $rs @('known', '127.0.0.1', '47999'))

# ---- 6. usb-check (a phone may or may not be on the cable; nothing is opened) ----------------------
Write-Host '== usb-check'
$cppUsb = Join-Path $root 'build\Release\niwcam-usb-check.exe'
$rsUsb = Join-Path $env:CARGO_TARGET_DIR 'debug\examples\usb_check.exe'
foreach ($a in @('list'), @('watch', '5'), @('hello', 'BOGUS'), @('probe', 'BOGUS', '47321', 'close')) {
    Check ('usb-check ' + ($a -join ' ')) (Lines $cppUsb $a) (Lines $rsUsb $a)
}

# ---- 7. mDNS ---------------------------------------------------------------------------------
Write-Host '== mDNS browse of _camstudio._tcp (6 s each)'
$cd = Lines (Join-Path $out 'cpp_discovery.exe') @('browse', '6') | Select-Object -Skip 1
$rd = Lines (Join-Path $env:CARGO_TARGET_DIR 'debug\examples\discovery_browse.exe') @('browse', '6') | Select-Object -Skip 1
Check 'discovery' $cd $rd

# ---- 8. the Rust tests -------------------------------------------------------------------------
Write-Host '== cargo test (fake phone, stubs, connector)'
Remove-Item Env:CAMSTUDIO_DATA_DIR
if ($Long) { $env:NIWCAM_PHONE_LONG = '1' }
Push-Location (Join-Path $root 'rust')
cargo test -q -p niwcam-phone -- --test-threads=1
if ($LASTEXITCODE -ne 0) { $bad++ }
Pop-Location

Remove-Item -Recurse -Force $work -ErrorAction SilentlyContinue
if ($bad) { Write-Host "FAILURES: $bad"; exit 1 }
Write-Host 'ALL MATCH'
