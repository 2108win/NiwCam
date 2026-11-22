# Downloads the two optional models of the "Chất lượng cao" background/face effects into
# app\Assets\Models and checks their SHA-256 (docs/third-party-models.md says what they are).
#
#   powershell -ExecutionPolicy Bypass -File scripts\fetch-models.ps1 [-Force]
#
# They are NOT in git (26 MB of binary would stay in the history for good) and the release script does
# not call this: they only matter once the Rust engine ships. Run it before building such a release.
param([switch]$Force)
$ErrorActionPreference = 'Stop'
$dir = Join-Path (Split-Path -Parent $PSScriptRoot) 'app\Assets\Models'
New-Item -ItemType Directory -Force $dir | Out-Null

$models = @(
    @{ Name = 'modnet_webcam.onnx'
       Url = 'https://github.com/yakhyo/modnet/releases/download/weights/modnet_webcam.onnx'
       Sha256 = 'de03cc16f3c91f25b7c2f0b42ea1a8d34f40a752234f3887572655e744e55306' },
    @{ Name = 'face_detection_yunet_2026may.onnx'
       # Git LFS file of opencv_zoo: the media host serves the real bytes (raw.githubusercontent gives a pointer).
       Url = 'https://media.githubusercontent.com/media/opencv/opencv_zoo/main/models/face_detection_yunet/face_detection_yunet_2026may.onnx'
       Sha256 = 'ebafce4e3c118d6554634be5c27ab333b4c047a9a8c3faf1d7cf93101c22f0f0' }
)

foreach ($m in $models) {
    $path = Join-Path $dir $m.Name
    if ((Test-Path $path) -and -not $Force -and ((Get-FileHash $path -Algorithm SHA256).Hash.ToLower() -eq $m.Sha256)) {
        Write-Host "ok      $($m.Name)"
        continue
    }
    $tmp = "$path.download"
    Write-Host "fetch   $($m.Name)"
    Invoke-WebRequest -Uri $m.Url -OutFile $tmp -UseBasicParsing -TimeoutSec 600
    $hash = (Get-FileHash $tmp -Algorithm SHA256).Hash.ToLower()
    if ($hash -ne $m.Sha256) {
        Remove-Item $tmp -Force
        throw "$($m.Name): SHA-256 is $hash, expected $($m.Sha256). Not installed."
    }
    Move-Item $tmp $path -Force
    Write-Host "        $($m.Name) verified"
}
