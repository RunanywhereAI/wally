param(
    [Parameter(Mandatory = $false)]
    [string]$BuildDir = "build",

    [Parameter(Mandatory = $false)]
    [string]$Version = "",

    [Parameter(Mandatory = $false)]
    [string]$KitDir = "",

    # Names the archive. install.ps1 asks for wally-<ver>-windows-arm64.zip on an
    # ARM64 host and falls back to x86_64, so the arm64 build must use exactly
    # that spelling or the native archive is never found.
    [Parameter(Mandatory = $false)]
    [ValidateSet("windows-x86_64", "windows-arm64")]
    [string]$Platform = "windows-x86_64",

    # "prod" for the production bottle, "dev" for the dev-endpoint bottle. Only
    # the archive filename changes (-dev); the staged tree stays wally-<platform>.
    [Parameter(Mandatory = $false)]
    [ValidateSet("prod", "dev")]
    [string]$Channel = "prod"
)
$Suffix = if ($Channel -eq "dev") { "-dev" } else { "" }

$ErrorActionPreference = "Stop"
Set-StrictMode -Version Latest

# This script lives in scripts/build, so the repo root is two levels up, not
# one. At one level $CliRoot was scripts/, and a relative -BuildDir "build"
# resolved to scripts/build, where no wally.exe exists -- the packaging step
# failed with "wally.exe was not found under ...\scripts\build". Matches the
# bash packager's ROOT=$SCRIPT_DIR/../.. .
$CliRoot = (Resolve-Path (Join-Path $PSScriptRoot "..\..")).Path
if (-not [IO.Path]::IsPathRooted($BuildDir)) {
    $BuildDir = Join-Path $CliRoot $BuildDir
}
$BuildDir = (Resolve-Path $BuildDir).Path

if ([string]::IsNullOrWhiteSpace($Version)) {
    $cm = Get-Content (Join-Path $CliRoot "CMakeLists.txt") -Raw
    if ($cm -match 'project\(wally VERSION ([0-9.]+)') { $Version = $Matches[1] }
}
$Version = $Version.TrimStart("v")
if ([string]::IsNullOrWhiteSpace($Version)) { throw "cannot resolve Wally version" }

$Binary = @(
    (Join-Path $BuildDir "wally.exe"),
    (Join-Path $BuildDir "Release\wally.exe"),
    (Join-Path $BuildDir "RelWithDebInfo\wally.exe")
) | Where-Object { Test-Path $_ } | Select-Object -First 1
if (-not $Binary) {
    $Binary = Get-ChildItem -Path $BuildDir -Filter "wally.exe" -File -Recurse |
        Select-Object -ExpandProperty FullName -First 1
}
if (-not $Binary) { throw "wally.exe was not found under $BuildDir" }

if ([string]::IsNullOrWhiteSpace($KitDir)) {
    $KitDir = $env:WALLY_SDK_KIT
}
if ([string]::IsNullOrWhiteSpace($KitDir)) {
    $KitDir = $env:CMAKE_PREFIX_PATH
}

$DistDir = Join-Path $CliRoot "dist"
$StageRoot = Join-Path $DistDir "stage"
$Stage = Join-Path $StageRoot "wally-$Platform"
$BinDir = Join-Path $Stage "bin"
$Zip = Join-Path $DistDir "wally-$Version-$Platform$Suffix.zip"

Remove-Item $Stage -Recurse -Force -ErrorAction SilentlyContinue
New-Item $BinDir -ItemType Directory -Force | Out-Null
Copy-Item $Binary (Join-Path $BinDir "wally.exe")
$Readme = Join-Path $CliRoot "README.md"
if (Test-Path $Readme) { Copy-Item $Readme (Join-Path $Stage "README.md") }

function Copy-Dll([string]$Src) {
    if ([string]::IsNullOrWhiteSpace($Src) -or -not (Test-Path $Src)) { return }
    $dest = Join-Path $BinDir (Split-Path $Src -Leaf)
    if (-not (Test-Path $dest)) { Copy-Item $Src $dest }
}

# Kit third_party (onnxruntime.dll) plus anything next to the built exe.
if ($KitDir -and (Test-Path (Join-Path $KitDir "third_party"))) {
    Get-ChildItem (Join-Path $KitDir "third_party") -Filter "*.dll" -File -ErrorAction SilentlyContinue |
        ForEach-Object { Copy-Dll $_.FullName }
}
Get-ChildItem -Path (Split-Path $Binary -Parent) -Filter "*.dll" -File -ErrorAction SilentlyContinue |
    ForEach-Object { Copy-Dll $_.FullName }

$OldPath = $env:PATH
try {
    $env:PATH = "$BinDir;$OldPath"
    & (Join-Path $BinDir "wally.exe") version
    if ($LASTEXITCODE -ne 0) { throw "packaged wally version smoke failed" }

    # The archive name says which flavour this is; the binary has to agree. A dev
    # job whose endpoint variables were unset used to produce a `-dev` archive
    # that defaults to production, and nothing noticed (wally #87). Asked in an
    # empty profile with the runtime overrides cleared, so a signed-in account or
    # a stray WALLY_CONSOLE_URL on the build machine cannot answer for the bake.
    $ProbeProfile = Join-Path ([System.IO.Path]::GetTempPath()) ([System.Guid]::NewGuid().ToString())
    New-Item $ProbeProfile -ItemType Directory -Force | Out-Null
    $SavedEnv = @{}
    foreach ($name in @("WALLY_CONSOLE_URL", "WALLY_CONSOLE_WEB_URL", "RCLI_CONSOLE_URL",
                        "RCLI_CONSOLE_WEB_URL", "WALLY_PROFILE_DIR")) {
        $SavedEnv[$name] = [Environment]::GetEnvironmentVariable($name)
        Remove-Item "env:$name" -ErrorAction SilentlyContinue
    }
    $env:WALLY_PROFILE_DIR = $ProbeProfile
    try {
        $About = & (Join-Path $BinDir "wally.exe") about --json
        if ($LASTEXITCODE -ne 0) { throw "packaged wally about --json failed" }
    } finally {
        foreach ($name in $SavedEnv.Keys) {
            if ($null -eq $SavedEnv[$name]) { Remove-Item "env:$name" -ErrorAction SilentlyContinue }
            else { Set-Item "env:$name" $SavedEnv[$name] }
        }
        Remove-Item $ProbeProfile -Recurse -Force -ErrorAction SilentlyContinue
    }
    $BuiltChannel = ([regex]::Match(($About -join ""), '"channel":"([^"]*)"')).Groups[1].Value
    $WantChannel = if ($Channel -eq "dev") { "development" } else { "production" }
    if ($BuiltChannel -ne $WantChannel) {
        throw ("packaging a '$Channel' archive from a '$BuiltChannel' binary; expected " +
               "channel '$WantChannel'. Set WALLY_CHANNEL and the baked endpoint variables " +
               "in the configure environment, or package the matching build.")
    }
} finally {
    $env:PATH = $OldPath
}

New-Item $DistDir -ItemType Directory -Force | Out-Null
Remove-Item $Zip, "$Zip.sha256" -Force -ErrorAction SilentlyContinue
Compress-Archive -Path $Stage -DestinationPath $Zip -CompressionLevel Optimal
$Hash = (Get-FileHash $Zip -Algorithm SHA256).Hash.ToLowerInvariant()
"$Hash  $([IO.Path]::GetFileName($Zip))" |
    Set-Content -Path "$Zip.sha256" -Encoding ascii -NoNewline
Write-Host "Packaged: $Zip"
