# Regenerates the WiX dialog art from the real logo (assets\logo.png):
#   banner.bmp  493x58   top strip on progress/exit dialogs, logo at the right
#   dialog.bmp  493x312  full background of the welcome/EULA dialog; the left
#                        ~173px strip stays visible next to the license box
# Outputs are committed - rerun only if the logo changes.
# ASCII only: Windows PowerShell 5.1 reads BOM-less scripts as ANSI.
$ErrorActionPreference = "Stop"
Add-Type -AssemblyName System.Drawing

$logoPath = Join-Path $PSScriptRoot "..\assets\logo.png"
$logo = [System.Drawing.Image]::FromFile($logoPath)

function New-Art([int]$w, [int]$h, [scriptblock]$draw, [string]$out) {
    $bmp = New-Object System.Drawing.Bitmap($w, $h, [System.Drawing.Imaging.PixelFormat]::Format24bppRgb)
    $g = [System.Drawing.Graphics]::FromImage($bmp)
    $g.InterpolationMode = [System.Drawing.Drawing2D.InterpolationMode]::HighQualityBicubic
    $g.SmoothingMode = [System.Drawing.Drawing2D.SmoothingMode]::AntiAlias
    $g.Clear([System.Drawing.Color]::White)
    & $draw $g
    $g.Dispose()
    $bmp.Save($out, [System.Drawing.Imaging.ImageFormat]::Bmp)
    $bmp.Dispose()
    Write-Host "Wrote $out"
}

New-Art 493 58 {
    param($g)
    $g.DrawImage($logo, (New-Object System.Drawing.Rectangle(440, 5, 48, 48)))
} (Join-Path $PSScriptRoot "banner.bmp")

New-Art 493 312 {
    param($g)
    $g.DrawImage($logo, (New-Object System.Drawing.Rectangle(17, 86, 140, 140)))
} (Join-Path $PSScriptRoot "dialog.bmp")

$logo.Dispose()
