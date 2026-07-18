# Builds the signed PDFAxe per-user MSI into <target>\wix\.
# Pipeline: cargo build, sign both exes, cargo wix, sign the MSI.
# Needs: cargo-wix, the WiX 3.14 binaries (PDFAXE_WIX_BIN overrides the
# default location), and the LXB Azure Trusted Signing setup: Sign-LxbFile.ps1
# (LXB_SIGN_SCRIPT overrides) plus AZURE_TENANT_ID/CLIENT_ID/CLIENT_SECRET in
# the environment. Without the creds the sign script skips with a warning, so
# contributor builds still work - release builds from this machine are signed.
# ASCII only: Windows PowerShell 5.1 reads BOM-less scripts as ANSI.
$ErrorActionPreference = "Stop"
Set-Location $PSScriptRoot

$wix = if ($env:PDFAXE_WIX_BIN) { $env:PDFAXE_WIX_BIN } else { "$env:USERPROFILE\lxb-tools\wix314" }
if (-not (Test-Path "$wix\candle.exe")) {
    throw "WiX binaries not found at $wix - set PDFAXE_WIX_BIN or unzip wix314-binaries there."
}
# Signer resolution: explicit override, then the LXB tools copy (this
# machine), then the repo-local copy (CI and contributor builds).
$signer = $env:LXB_SIGN_SCRIPT
if (-not $signer) { $signer = "$env:USERPROFILE\lxb-tools\Sign-LxbFile.ps1" }
if (-not (Test-Path $signer)) { $signer = Join-Path $PSScriptRoot "ci\sign.ps1" }
if (-not (Test-Path $signer)) {
    throw "No sign script found - set LXB_SIGN_SCRIPT."
}

function Sign-One([string]$Path, [string]$Name) {
    & $signer -FilePath $Path -ProductName $Name -ProductUrl "https://pdfaxe.app"
    if ($LASTEXITCODE -ne 0) { throw "signing failed for $Path" }
}

$meta = cargo metadata --format-version 1 --no-deps | ConvertFrom-Json
$bin = Join-Path $meta.target_directory "release"

cargo build --release --workspace
if (-not $?) { throw "release build failed" }

# Sign the exes BEFORE WiX packs them into the MSI cab.
Sign-One (Join-Path $bin "pdfaxe-app.exe") "PDFAxe"
Sign-One (Join-Path $bin "pdfaxe-cli.exe") "PDFAxe"

cargo wix --package pdfaxe-app --include wix\main.wxs --no-build --nocapture --bin-path $wix
if ($LASTEXITCODE -ne 0) { throw "cargo wix failed" }

$msi = Get-ChildItem (Join-Path $meta.target_directory "wix\*.msi") | Sort-Object LastWriteTime | Select-Object -Last 1
Sign-One $msi.FullName "PDFAxe Installer"

Write-Host ""
foreach ($f in (Join-Path $bin "pdfaxe-app.exe"), (Join-Path $bin "pdfaxe-cli.exe"), $msi.FullName) {
    $sig = Get-AuthenticodeSignature $f
    Write-Host ("{0,-12} {1}" -f $sig.Status, $f)
}
Write-Host "MSI: $($msi.FullName)"
