param(
    [string]$BuildDir = "build",
    [string]$Version = "",
    [ValidateSet("windows-x86_64", "windows-arm64")][string]$Platform = "windows-x86_64",
    [ValidateSet("prod", "dev")][string]$Channel = "prod"
)

# Stages the Windows on-device bottle (GGUF only; MLX is Apple-only) into the
# same layout the unix packager and install.ps1 use:
#   wally-<platform>/bin/wally.exe (+ kit runtime DLLs), README.md
# then zips it with a sha256 sidecar. dev differs only by the -dev suffix.

$ErrorActionPreference = "Stop"
Set-StrictMode -Version Latest

$Root = (Resolve-Path (Join-Path $PSScriptRoot "..")).Path
if (-not [IO.Path]::IsPathRooted($BuildDir)) { $BuildDir = Join-Path $Root $BuildDir }

if ([string]::IsNullOrWhiteSpace($Version)) {
    $vg = Get-Content (Join-Path $Root "version/version.go") -Raw
    if ($vg -match 'Version\s*=\s*"([0-9]+\.[0-9]+\.[0-9]+)') { $Version = $Matches[1] }
}
$Version = $Version.TrimStart("v")
if ([string]::IsNullOrWhiteSpace($Version)) { throw "cannot resolve wally version" }
$Suffix = if ($Channel -eq "dev") { "-dev" } else { "" }

$Bin = @(
    (Join-Path $BuildDir "wally.exe"),
    (Join-Path $BuildDir "Release\wally.exe")
) | Where-Object { Test-Path $_ } | Select-Object -First 1
if (-not $Bin) { throw "wally.exe not found under $BuildDir" }

$Dist = Join-Path $Root "dist"
$Stage = Join-Path $Dist "stage/wally-$Platform"
if (Test-Path $Stage) { Remove-Item -Recurse -Force $Stage }
New-Item -ItemType Directory -Force -Path (Join-Path $Stage "bin") | Out-Null
Copy-Item $Bin (Join-Path $Stage "bin/wally.exe")
if (Test-Path (Join-Path $Root "README.md")) {
    Copy-Item (Join-Path $Root "README.md") (Join-Path $Stage "README.md")
}

$Kit = $env:WALLY_SDK_KIT
if ($Kit -and (Test-Path (Join-Path $Kit "third_party"))) {
    Get-ChildItem (Join-Path $Kit "third_party") -Filter *.dll -ErrorAction SilentlyContinue | ForEach-Object {
        Copy-Item $_.FullName (Join-Path $Stage "bin/")
    }
}

New-Item -ItemType Directory -Force -Path $Dist | Out-Null
$Zip = Join-Path $Dist "wally-$Version-$Platform$Suffix.zip"
if (Test-Path $Zip) { Remove-Item -Force $Zip }
Compress-Archive -Path $Stage -DestinationPath $Zip
$Hash = (Get-FileHash -Algorithm SHA256 $Zip).Hash.ToLower()
"$Hash  $(Split-Path $Zip -Leaf)" | Set-Content -NoNewline -Encoding ascii "$Zip.sha256"
Write-Host "packaged $Zip"
