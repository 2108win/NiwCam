# Draws the NiwCam icon (a lens on a deep-blue tile) and writes
#   app\Assets\NiwCam.ico  (PNG-compressed, 16-256 px) and app\Assets\NiwCam.png (256 px),
#   app\Assets\NiwCam-live.ico (the same with a red dot: the tray icon while an app uses the camera) and
#   app\Assets\NiwCam-dot.ico (the red dot alone: the badge on the taskbar button),
#   ios\Resources\Assets.xcassets\AppIcon.appiconset\AppIcon.png (1024 px, square, no alpha: iOS rounds it),
#   site-icon.png in build\ (256 px, for the winlax.id.vn product page).
# The Android launcher icon (android\app\src\main\res\drawable\ic_launcher.xml) is the same mark as a
# vector: keep the colours and proportions in step with this file.
$ErrorActionPreference = "Stop"
Add-Type -AssemblyName System.Drawing

$root = Split-Path -Parent $PSScriptRoot
$assets = Join-Path $root "app\Assets"
New-Item -ItemType Directory -Force $assets | Out-Null

function New-RoundRect([float]$x, [float]$y, [float]$w, [float]$h, [float]$r) {
    $p = New-Object System.Drawing.Drawing2D.GraphicsPath
    $d = 2 * $r
    $p.AddArc($x, $y, $d, $d, 180, 90)
    $p.AddArc($x + $w - $d, $y, $d, $d, 270, 90)
    $p.AddArc($x + $w - $d, $y + $h - $d, $d, $d, 0, 90)
    $p.AddArc($x, $y + $h - $d, $d, $d, 90, 90)
    $p.CloseFigure()
    return $p
}

function C([int]$r, [int]$g, [int]$b, [int]$a = 255) { return [System.Drawing.Color]::FromArgb($a, $r, $g, $b) }

# $square: the iOS icon (full-bleed tile, no rounded corners).
# Red dot with a white ring, so it reads on a dark and on a light taskbar. Same red as the app's status colour.
function Add-LiveDot($g, [float]$cx, [float]$cy, [float]$r) {
    $ringR = $r + [Math]::Max(1.0, $r * 0.28)
    $g.FillEllipse((New-Object System.Drawing.SolidBrush (C 255 255 255)), $cx - $ringR, $cy - $ringR, 2 * $ringR, 2 * $ringR)
    $g.FillEllipse((New-Object System.Drawing.SolidBrush (C 229 72 77)), $cx - $r, $cy - $r, 2 * $r, 2 * $r)
}

# $mode: "app" the lens, "live" the lens with the dot (tray, an app uses the camera), "dot" the dot alone (taskbar badge).
function New-IconBitmap([int]$s, [bool]$square = $false, [string]$mode = "app") {
    $format = if ($square) { [System.Drawing.Imaging.PixelFormat]::Format24bppRgb } else { [System.Drawing.Imaging.PixelFormat]::Format32bppArgb }
    $bmp = New-Object System.Drawing.Bitmap $s, $s, $format
    $g = [System.Drawing.Graphics]::FromImage($bmp)
    $g.SmoothingMode = "AntiAlias"
    $g.Clear($(if ($square) { C 7 15 38 } else { [System.Drawing.Color]::Transparent }))
    if ($mode -eq "dot") {
        Add-LiveDot $g ($s * 0.5) ($s * 0.5) ($s * 0.34)
        $g.Dispose()
        return $bmp
    }

    # Tile: deep blue, lighter at the top left.
    $rect = New-Object System.Drawing.RectangleF 0, 0, $s, $s
    $grad = New-Object System.Drawing.Drawing2D.LinearGradientBrush $rect, (C 22 41 90), (C 7 15 38), 45.0
    if ($square) { $g.FillRectangle($grad, $rect) }
    else { $g.FillPath($grad, (New-RoundRect 0 0 $s $s ($s * 0.22))) }

    # Lens ring.
    $ringW = [Math]::Max(1.0, $s * 0.06)
    $rr = $s * 0.33
    $pen = New-Object System.Drawing.Pen (C 233 240 255), $ringW
    $g.DrawEllipse($pen, $s * 0.5 - $rr, $s * 0.5 - $rr, 2 * $rr, 2 * $rr)

    # Glass: radial blue, brightest up and to the left.
    $gr = $s * 0.22
    $glass = New-Object System.Drawing.Drawing2D.GraphicsPath
    $glass.AddEllipse($s * 0.5 - $gr, $s * 0.5 - $gr, 2 * $gr, 2 * $gr)
    $pg = New-Object System.Drawing.Drawing2D.PathGradientBrush $glass
    $pg.CenterPoint = New-Object System.Drawing.PointF ($s * 0.44), ($s * 0.43)
    $blend = New-Object System.Drawing.Drawing2D.ColorBlend 3
    $blend.Colors = @((C 10 29 102), (C 27 85 214), (C 111 176 255))
    $blend.Positions = @(0.0, 0.5, 1.0)
    $pg.InterpolationColors = $blend
    $g.FillPath($pg, $glass)

    # Pupil and the highlight.
    $pr = $s * 0.09
    $g.FillEllipse((New-Object System.Drawing.SolidBrush (C 7 18 54)), $s * 0.5 - $pr, $s * 0.5 - $pr, 2 * $pr, 2 * $pr)
    $hr = $s * 0.045
    $g.FillEllipse((New-Object System.Drawing.SolidBrush (C 255 255 255 230)), $s * 0.41 - $hr, $s * 0.40 - $hr, 2 * $hr, 2 * $hr)
    if ($mode -eq "live") { Add-LiveDot $g ($s * 0.73) ($s * 0.73) ($s * 0.19) }
    $g.Dispose()
    return $bmp
}

$sizes = 16, 20, 24, 32, 40, 48, 64, 256
$pngs = foreach ($s in $sizes) {
    $bmp = New-IconBitmap $s
    $ms = New-Object System.IO.MemoryStream
    $bmp.Save($ms, [System.Drawing.Imaging.ImageFormat]::Png)
    if ($s -eq 256) {
        $bmp.Save((Join-Path $assets "NiwCam.png"), [System.Drawing.Imaging.ImageFormat]::Png)
        $buildDir = Join-Path $root "build"
        New-Item -ItemType Directory -Force $buildDir | Out-Null
        $bmp.Save((Join-Path $buildDir "site-icon.png"), [System.Drawing.Imaging.ImageFormat]::Png)
    }
    $bmp.Dispose()
    , $ms.ToArray()
}

# iOS: one 1024 px square, no alpha.
$ios = Join-Path $root "ios\Resources\Assets.xcassets\AppIcon.appiconset\AppIcon.png"
$big = New-IconBitmap 1024 $true
$big.Save($ios, [System.Drawing.Imaging.ImageFormat]::Png)
$big.Dispose()
Write-Host "Wrote $ios"

# ICO container: header, one directory entry per size, then the PNG blobs.
$out = New-Object System.IO.MemoryStream
$w = New-Object System.IO.BinaryWriter $out
$w.Write([uint16]0); $w.Write([uint16]1); $w.Write([uint16]$sizes.Count)
$offset = 6 + 16 * $sizes.Count
for ($i = 0; $i -lt $sizes.Count; $i++) {
    $s = $sizes[$i]
    $w.Write([byte]($s % 256)); $w.Write([byte]($s % 256))   # 256 is stored as 0
    $w.Write([byte]0); $w.Write([byte]0)
    $w.Write([uint16]1); $w.Write([uint16]32)
    $w.Write([uint32]$pngs[$i].Length); $w.Write([uint32]$offset)
    $offset += $pngs[$i].Length
}
foreach ($p in $pngs) { $w.Write($p) }
[System.IO.File]::WriteAllBytes((Join-Path $assets "NiwCam.ico"), $out.ToArray())
Write-Host "Wrote $assets\NiwCam.ico"

# The in-use variants: a small ICO each (the tray picks the size of the notification area).
function Write-Ico([string]$path, [string]$mode, [int[]]$list) {
    $blobs = foreach ($s in $list) {
        $b = New-IconBitmap $s $false $mode
        $m = New-Object System.IO.MemoryStream
        $b.Save($m, [System.Drawing.Imaging.ImageFormat]::Png)
        $b.Dispose()
        , $m.ToArray()
    }
    $o = New-Object System.IO.MemoryStream
    $bw = New-Object System.IO.BinaryWriter $o
    $bw.Write([uint16]0); $bw.Write([uint16]1); $bw.Write([uint16]$list.Count)
    $off = 6 + 16 * $list.Count
    for ($i = 0; $i -lt $list.Count; $i++) {
        $bw.Write([byte]($list[$i] % 256)); $bw.Write([byte]($list[$i] % 256))
        $bw.Write([byte]0); $bw.Write([byte]0)
        $bw.Write([uint16]1); $bw.Write([uint16]32)
        $bw.Write([uint32]$blobs[$i].Length); $bw.Write([uint32]$off)
        $off += $blobs[$i].Length
    }
    foreach ($p in $blobs) { $bw.Write($p) }
    [System.IO.File]::WriteAllBytes($path, $o.ToArray())
    Write-Host "Wrote $path"
}
Write-Ico (Join-Path $assets "NiwCam-live.ico") "live" @(16, 20, 24, 32, 48, 64)
Write-Ico (Join-Path $assets "NiwCam-dot.ico") "dot" @(16, 20, 24, 32)
