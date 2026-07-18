# Signs one .exe/.msi via Azure Trusted Signing. Repo-local copy of LXB's
# Sign-LxbFile.ps1 so CI and contributor machines need nothing outside this
# repo. Needs the .NET "sign" CLI (dotnet tool install --global sign) and
# AZURE_TENANT_ID / AZURE_CLIENT_ID / AZURE_CLIENT_SECRET in the environment;
# without the creds it warns and exits 0 so unsigned builds still work.
# The account/profile/endpoint are LXB's defaults, overridable via env for
# forks that sign with their own Trusted Signing account.
# ASCII only: Windows PowerShell 5.1 reads BOM-less scripts as ANSI.
param(
    [Parameter(Mandatory=$true)]  [string]$FilePath,
    [Parameter(Mandatory=$true)]  [string]$ProductName,
    [Parameter(Mandatory=$true)]  [string]$ProductUrl
)

$AccountName = if ($env:LXB_SIGN_ACCOUNT)  { $env:LXB_SIGN_ACCOUNT }  else { "LXBStudioLLC" }
$CertProfile = if ($env:LXB_SIGN_PROFILE)  { $env:LXB_SIGN_PROFILE }  else { "lxb-public-trust" }
$Endpoint    = if ($env:LXB_SIGN_ENDPOINT) { $env:LXB_SIGN_ENDPOINT } else { "https://eus.codesigning.azure.net/" }

if (-not $env:AZURE_TENANT_ID -or -not $env:AZURE_CLIENT_ID -or -not $env:AZURE_CLIENT_SECRET) {
    Write-Warning "Azure credentials not set; skipping signing of $FilePath (unsigned build)."
    exit 0
}

if (-not (Test-Path $FilePath)) {
    Write-Error "File not found: $FilePath"
    exit 1
}

$resolved = (Resolve-Path $FilePath).Path
$dir  = Split-Path $resolved -Parent
$file = Split-Path $resolved -Leaf

Write-Host "Signing $resolved" -ForegroundColor Yellow

Push-Location $dir
try {
    sign code artifact-signing `
        --artifact-signing-account $AccountName `
        --artifact-signing-certificate-profile $CertProfile `
        --artifact-signing-endpoint $Endpoint `
        --description $ProductName `
        --description-url $ProductUrl `
        --base-directory $dir `
        $file

    if ($LASTEXITCODE -ne 0) {
        Write-Error "Signing failed for $resolved (exit $LASTEXITCODE)"
        exit 1
    }
}
finally {
    Pop-Location
}

Write-Host "Signed: $resolved" -ForegroundColor Green
