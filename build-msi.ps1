# Builds the PDFAxe per-user MSI into <target>\wix\.
# Needs: cargo-wix (cargo install cargo-wix) and the WiX 3.14 binaries
# (unzipped, no installer required - override the location with $env:PDFAXE_WIX_BIN).
# ASCII only: Windows PowerShell 5.1 reads BOM-less scripts as ANSI.
$ErrorActionPreference = "Stop"
Set-Location $PSScriptRoot

$wix = if ($env:PDFAXE_WIX_BIN) { $env:PDFAXE_WIX_BIN } else { "$env:USERPROFILE\lxb-tools\wix314" }
if (-not (Test-Path "$wix\candle.exe")) {
    throw "WiX binaries not found at $wix - set PDFAXE_WIX_BIN or unzip wix314-binaries there."
}

cargo build --release --workspace
if (-not $?) { throw "release build failed" }

cargo wix --package pdfaxe-app --include wix\main.wxs --no-build --nocapture --bin-path $wix
