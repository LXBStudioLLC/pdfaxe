# Regenerates assets/pdfaxe.ico and assets/logo.png from the brand logo SVG.
# The SVG is the founder's asset and lives OUTSIDE the repo; pass -LogoSvg to
# point elsewhere. Chrome renders it on black and on white, gen-icon.rs
# recovers alpha by difference matting and packs the multi-size .ico.
# ASCII only: Windows PowerShell 5.1 reads BOM-less scripts as ANSI.
param(
    [string]$LogoSvg = "$env:USERPROFILE\OneDrive\Desktop\LXBStudio\-=PDFAXE=-\PDF AXE logo.svg",
    [string]$Chrome = "C:\Program Files\Google\Chrome\Application\chrome.exe"
)
$ErrorActionPreference = "Stop"
if (-not (Test-Path $LogoSvg)) { throw "logo SVG not found: $LogoSvg" }
if (-not (Test-Path $Chrome)) { throw "Chrome not found: $Chrome" }

$tmp = Join-Path $env:TEMP "pdfaxe-icon"
New-Item -ItemType Directory -Force $tmp | Out-Null
$uri = "file:///" + (((Resolve-Path $LogoSvg).Path -replace '\\', '/') -replace ' ', '%20')

Remove-Item "$tmp\logo-black.png", "$tmp\logo-white.png" -Force -ErrorAction SilentlyContinue
& $Chrome --headless --disable-gpu --screenshot="$tmp\logo-black.png" --window-size=1024,1024 --default-background-color=FF000000 $uri 2>$null
& $Chrome --headless --disable-gpu --screenshot="$tmp\logo-white.png" --window-size=1024,1024 --default-background-color=FFFFFFFF $uri 2>$null
# Chrome exits before the screenshot hits disk; wait for both files.
$deadline = (Get-Date).AddSeconds(30)
while ((Get-Date) -lt $deadline) {
    if ((Test-Path "$tmp\logo-black.png") -and (Test-Path "$tmp\logo-white.png")) { break }
    Start-Sleep -Milliseconds 250
}
Start-Sleep -Seconds 1

Add-Type -AssemblyName System.Drawing
foreach ($n in "black", "white") {
    $bmp = [System.Drawing.Bitmap]::FromFile("$tmp\logo-$n.png")
    if ($bmp.Width -ne 1024 -or $bmp.Height -ne 1024) { throw "expected 1024x1024 render, got $($bmp.Width)x$($bmp.Height)" }
    $rect = New-Object System.Drawing.Rectangle(0, 0, 1024, 1024)
    $data = $bmp.LockBits($rect, [System.Drawing.Imaging.ImageLockMode]::ReadOnly, [System.Drawing.Imaging.PixelFormat]::Format32bppArgb)
    $bytes = New-Object byte[] (1024 * 1024 * 4)
    [System.Runtime.InteropServices.Marshal]::Copy($data.Scan0, $bytes, 0, $bytes.Length)
    $bmp.UnlockBits($data)
    $bmp.Dispose()
    [System.IO.File]::WriteAllBytes("$tmp\logo-$n.bgra", $bytes)
}

rustc -O (Join-Path $PSScriptRoot "gen-icon.rs") -o "$tmp\gen-icon.exe"
if (-not $?) { throw "gen-icon build failed" }
& "$tmp\gen-icon.exe" "$tmp\logo-black.bgra" "$tmp\logo-white.bgra" (Join-Path $PSScriptRoot "pdfaxe.ico") "$tmp\logo-matted.bmp"
if (-not $?) { throw "gen-icon run failed" }

# Straight-alpha PNG of the full matted logo, for the repo / future website use.
$m = [System.Drawing.Bitmap]::FromFile("$tmp\logo-matted.bmp")
$m.Save((Join-Path $PSScriptRoot "logo.png"), [System.Drawing.Imaging.ImageFormat]::Png)
$m.Dispose()
Write-Host "wrote assets\pdfaxe.ico and assets\logo.png"
