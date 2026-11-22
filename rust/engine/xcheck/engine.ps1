# Runs the engine driver (one fixed scenario through the cs_* API) against the C++ NiwCamEngine.dll
# and the Rust one, N times each, and prints the lines that differ plus CPU/RAM side by side.
# Safe next to the user's running app: see examples/driver.rs. Exit code 0 = same behaviour.
param([int]$Runs = 3, [int]$Seconds = 8)
$ErrorActionPreference = 'Stop'
$root = Split-Path -Parent (Split-Path -Parent (Split-Path -Parent $PSScriptRoot))
$env:Path += ";$env:USERPROFILE\.cargo\bin"
$env:CARGO_TARGET_DIR = Join-Path $root 'build\rust-engine'
Push-Location (Join-Path $root 'rust')
$ErrorActionPreference = 'Continue'
cargo build -q -p niwcam-engine --release 2>$null
cargo build -q -p niwcam-engine --example driver 2>$null
if ($LASTEXITCODE -ne 0) { throw 'build failed' }
Pop-Location
$ErrorActionPreference = 'Stop'
$driver = Join-Path $root 'build\rust-engine\debug\examples\driver.exe'
$phone = Join-Path $root 'build\Release\niwcam-fake-phone.exe'
$dlls = [ordered]@{ cpp = Join-Path $root 'build\Release\NiwCamEngine.dll'; rust = Join-Path $root 'build\rust-engine\release\NiwCamEngine.dll' }
$out = Join-Path $root 'build\xcheck-engine'; New-Item -ItemType Directory -Force $out | Out-Null
$cpu = @{ cpp = @(); rust = @() }; $mem = @{ cpp = @(); rust = @() }; $fps = @{ cpp = @(); rust = @() }
for ($i = 1; $i -le $Runs; $i++) {
    foreach ($k in $dlls.Keys) {
        $f = Join-Path $out "$k-$i.txt"
        & $driver --dll $dlls[$k] --phone $phone --port 47997 --seconds $Seconds 2>&1 | Set-Content -Encoding utf8 $f
        $t = Get-Content $f
        $cpu[$k] += [double](($t | Select-String 'process_cpu_seconds=([\d.]+)').Matches[0].Groups[1].Value)
        $mem[$k] += [double](($t | Select-String 'peak_working_set_mb=(\d+)').Matches[0].Groups[1].Value)
        $fps[$k] += [double](($t | Select-String 'reader_fps=([\d.]+)').Matches[0].Groups[1].Value)
    }
}
# Behaviour: every line except the numbers that move with timing must be identical.
function Norm($f) { Get-Content $f | Where-Object { $_ -notmatch '^(dll=|t=\d|reader_|preview_|process_cpu)' } }
$bad = 0
for ($i = 1; $i -le $Runs; $i++) {
    $d = Compare-Object (Norm (Join-Path $out "cpp-$i.txt")) (Norm (Join-Path $out "rust-$i.txt"))
    if ($d) { $d | Out-String | Write-Host; $bad++ }
}
foreach ($k in $dlls.Keys) { '{0,-5} cpu(s) {1}   peak ws(MB) {2}   reader fps {3}' -f $k, (($cpu[$k] | ForEach-Object { '{0:N2}' -f $_ }) -join ' '), ($mem[$k] -join ' '), (($fps[$k] | ForEach-Object { '{0:N1}' -f $_ }) -join ' ') }
if ($bad) { Write-Host "BEHAVIOUR DIFFERENT in $bad run(s)"; exit 1 }
Write-Host "BEHAVIOUR SAME in $Runs run(s)"
