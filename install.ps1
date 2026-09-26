$ErrorActionPreference = 'Stop'

$Repo = 'RunanywhereAI/wally'
# There is no Homebrew here, so this installer does the whole job itself rather
# than handing off to a package manager: download the release zip, check it,
# unpack it, put it on PATH.
$InstallDir = Join-Path $env:LOCALAPPDATA 'Programs\wally'

# Windows PowerShell picks the older protocols on some builds and api.github.com
# refuses anything below TLS 1.2.
[Net.ServicePointManager]::SecurityProtocol = [Net.SecurityProtocolType]::Tls12
# Invoke-WebRequest spends most of a large download redrawing its progress bar.
$ProgressPreference = 'SilentlyContinue'

function Write-Info([string]$Message) {
    Write-Host '==> ' -ForegroundColor Blue -NoNewline
    Write-Host $Message
}
function Write-Ok([string]$Message) {
    Write-Host '==> ' -ForegroundColor Green -NoNewline
    Write-Host $Message
}
function Write-Warn([string]$Message) {
    Write-Host 'Warning: ' -ForegroundColor Yellow -NoNewline
    Write-Host $Message
}
# throw rather than exit, because the documented way to run this is
# `irm ... | iex`: an exit there closes the user's console, taking the error
# message with it.
function Fail([string]$Message) {
    throw "Error: $Message"
}

# Two overrides, for release tests and mirrors rather than everyday use --
# the same shape and names as install.sh's:
#   WALLY_INSTALL_VERSION      install that release instead of the latest
#   WALLY_INSTALL_BASE_URL     fetch the zip and its .sha256 from <url>/
#                              instead of the GitHub release (http, https or
#                              file); needs WALLY_INSTALL_VERSION
#
# $Release stays $null on this path: there is no releases-API lookup to ask
# "does this asset exist" of, so an asset that is not actually at the given
# version/URL is discovered the same way it would be anyway -- the download
# below fails.
if ($env:WALLY_INSTALL_VERSION) {
    $Version = $env:WALLY_INSTALL_VERSION -replace '^v', ''
    if ($Version -notmatch '^\d+\.\d+\.\d+$') {
        Fail "WALLY_INSTALL_VERSION must look like 1.2.3, not '$($env:WALLY_INSTALL_VERSION)'"
    }
    $Release = $null
    Write-Info "Installing v$Version"
} else {
    if ($env:WALLY_INSTALL_BASE_URL) {
        Fail 'WALLY_INSTALL_BASE_URL needs WALLY_INSTALL_VERSION: a mirror has no latest-release lookup'
    }
    Write-Info 'Checking latest Wally release...'
    try {
        $Release = Invoke-RestMethod "https://api.github.com/repos/$Repo/releases/latest"
    } catch {
        Fail 'Could not determine latest release version. Check your internet connection.'
    }
    $Version = "$($Release.tag_name)" -replace '^v', ''
    if (-not $Version) { Fail 'Could not determine latest release version. Check your internet connection.' }
    Write-Info "Latest version: v$Version"
}

# PROCESSOR_ARCHITECTURE reports the process, not the machine, so a 32-bit host
# under WOW64 says x86 while ARCHITEW6432 names what is really underneath.
$Arch = if ($env:PROCESSOR_ARCHITEW6432) { $env:PROCESSOR_ARCHITEW6432 } else { $env:PROCESSOR_ARCHITECTURE }
# Prism on Windows ARM64 runs the x64 zip. Native arm64 zips are preferred when
# $Release can say one exists; an explicit version/mirror is trusted to have
# what it says, the same as install.sh trusts WALLY_INSTALL_VERSION's platform.
$AssetName = "wally-$Version-windows-x86_64.zip"
if ($Arch -eq 'ARM64') {
    $ArmAssetName = "wally-$Version-windows-arm64.zip"
    $ArmAvailable = if ($Release) {
        [bool]($Release.assets | Where-Object { $_.name -eq $ArmAssetName } | Select-Object -First 1)
    } else {
        $true
    }
    if ($ArmAvailable) {
        $AssetName = $ArmAssetName
    } else {
        Write-Warn "No native ARM64 zip; installing the x64 build (Windows on ARM can run it)."
    }
} elseif ($Arch -ne 'AMD64') {
    Fail "Wally requires 64-bit Windows. Detected: $Arch"
}

# Where the asset and its checksum come from: the release's own asset list
# when $Release was looked up, or WALLY_INSTALL_BASE_URL (falling back to the
# normal GitHub release URL for the given version) when it was not.
function Resolve-AssetUrl([string]$Name) {
    if ($Release) {
        $Found = $Release.assets | Where-Object { $_.name -eq $Name } | Select-Object -First 1
        if (-not $Found) { return $null }
        return $Found.browser_download_url
    }
    $Base = if ($env:WALLY_INSTALL_BASE_URL) { $env:WALLY_INSTALL_BASE_URL.TrimEnd('/') } else { "https://github.com/$Repo/releases/download/v$Version" }
    return "$Base/$Name"
}

$AssetUrl = Resolve-AssetUrl $AssetName
if (-not $AssetUrl) {
    Fail "v$Version does not publish $AssetName. Open an issue at https://github.com/$Repo/issues"
}
$ShaAssetName = "$AssetName.sha256"
$ShaUrl = Resolve-AssetUrl $ShaAssetName
if (-not $ShaUrl) {
    Fail "v$Version does not publish $ShaAssetName. Refusing an unverified download."
}

$Temp = Join-Path ([IO.Path]::GetTempPath()) ('wally-' + [Guid]::NewGuid().ToString('N'))
New-Item -ItemType Directory -Path $Temp -Force | Out-Null
try {
    $Zip = Join-Path $Temp $AssetName
    Write-Info "Downloading $AssetName..."
    try {
        Invoke-WebRequest -Uri $AssetUrl -OutFile $Zip
    } catch {
        Fail "Could not download $AssetUrl"
    }

    # Through a file rather than straight into a variable: the asset is served
    # as octet-stream and the web cmdlets hand back bytes rather than text.
    $ShaFile = Join-Path $Temp $ShaAssetName
    Invoke-WebRequest -Uri $ShaUrl -OutFile $ShaFile
    $ShaLine = (Get-Content -Raw -LiteralPath $ShaFile).Trim()
    if ($ShaLine -notmatch '^([0-9A-Fa-f]{64})\s+\*?([^\r\n]+)$') {
        Fail "$ShaAssetName is not a valid SHA-256 sidecar."
    }
    $Expected = $Matches[1].ToLowerInvariant()
    $ListedAsset = $Matches[2].Trim()
    if ($ListedAsset -ne $AssetName) {
        Fail "$ShaAssetName names $ListedAsset instead of $AssetName."
    }
    $Actual = (Get-FileHash -LiteralPath $Zip -Algorithm SHA256).Hash.ToLowerInvariant()
    if ($Actual -ne $Expected) {
        Fail "Checksum mismatch on $AssetName. Expected $Expected, got $Actual. Do not use this download."
    }
    Write-Ok 'Checksum verified'

    Write-Info "Installing Wally v$Version to $InstallDir..."
    Expand-Archive -LiteralPath $Zip -DestinationPath $Temp -Force
    # The packager stages the tree as wally-<platform>, without the version
    # (scripts/build/package-wally-windows.ps1, and install.sh expects the same),
    # so the folder is not the asset name minus .zip. The versioned spelling is
    # still accepted in case a future archive adopts it.
    $Platform = $AssetName -replace "^wally-$([Regex]::Escape($Version))-", '' -replace '(-dev)?\.zip$', ''
    $Unpacked = @(
        (Join-Path $Temp "wally-$Platform\bin"),
        (Join-Path $Temp ([IO.Path]::GetFileNameWithoutExtension($AssetName) + '\bin'))
    ) | Where-Object { Test-Path -LiteralPath (Join-Path $_ 'wally.exe') } | Select-Object -First 1
    if (-not $Unpacked) {
        Fail "$AssetName does not have the layout this installer expects. Open an issue at https://github.com/$Repo/issues"
    }

    # Validate a complete candidate before replacing a working installation.
    # wally.exe and its DLLs stay together exactly as they are in archive bin/.
    $Candidate = Join-Path $Temp 'install-candidate'
    New-Item -ItemType Directory -Path $Candidate -Force | Out-Null
    Copy-Item -Path (Join-Path $Unpacked '*') -Destination $Candidate -Recurse -Force
    $CandidateExe = Join-Path $Candidate 'wally.exe'
    if (-not (Test-Path -LiteralPath $CandidateExe)) {
        Fail "$AssetName is missing bin\wally.exe."
    }
    $VersionOutput = @(& $CandidateExe --version 2>&1)
    if ($LASTEXITCODE -ne 0) {
        Fail 'The downloaded wally.exe does not run; the existing installation was left unchanged.'
    }
    $EscapedVersion = [Regex]::Escape($Version)
    if (($VersionOutput -join "`n") -notmatch "(?m)^wally\s+$EscapedVersion(?:\s|$)") {
        Fail "The downloaded executable does not report Wally v$Version; the existing installation was left unchanged."
    }

    $InstallParent = Split-Path $InstallDir -Parent
    New-Item -ItemType Directory -Path $InstallParent -Force | Out-Null
    $Backup = "$InstallDir.previous"
    Remove-Item -LiteralPath $Backup -Recurse -Force -ErrorAction SilentlyContinue
    if (Test-Path -LiteralPath $InstallDir) {
        Move-Item -LiteralPath $InstallDir -Destination $Backup
    }
    try {
        Move-Item -LiteralPath $Candidate -Destination $InstallDir
    } catch {
        if (Test-Path -LiteralPath $Backup) {
            Move-Item -LiteralPath $Backup -Destination $InstallDir
        }
        throw
    }
    Remove-Item -LiteralPath $Backup -Recurse -Force -ErrorAction SilentlyContinue
} finally {
    Remove-Item -LiteralPath $Temp -Recurse -Force -ErrorAction SilentlyContinue
}

$Exe = Join-Path $InstallDir 'wally.exe'
if (-not (Test-Path -LiteralPath $Exe)) { Fail "Installation failed. wally.exe is not in $InstallDir." }
& $Exe --version | Out-Null
if ($LASTEXITCODE -ne 0) { Fail "Installation failed. wally.exe is installed but does not run." }

# The user's own PATH, never the machine's: this installs under LOCALAPPDATA for
# one account and needs no administrator to do it.
$UserPath = [Environment]::GetEnvironmentVariable('Path', 'User')
$Entries = @()
if ($UserPath) { $Entries = @($UserPath -split ';' | Where-Object { $_ }) }
if ($Entries -contains $InstallDir) {
    Write-Ok "$InstallDir is already on your PATH"
} else {
    [Environment]::SetEnvironmentVariable('Path', (($Entries + $InstallDir) -join ';'), 'User')
    Write-Ok "Added $InstallDir to your PATH"
}

Write-Ok "Wally v$Version installed successfully"
Write-Host ''
Write-Warn 'Open a new terminal before running wally. This one was started with the old PATH.'
Write-Host ''
Write-Info 'Getting started:'
Write-Host '    wally models list --all       every model in the catalog'
Write-Host '    wally models pull qwen3-0.6b  download one'
Write-Host '    wally run qwen3-0.6b          talk to it, /? for commands'
Write-Host '    wally backends                which engines this build linked'
Write-Host ''
Write-Host '  Models download on demand into %LOCALAPPDATA%\RunAnywhere'
