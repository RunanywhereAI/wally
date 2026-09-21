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

Write-Info 'Checking latest Wally release...'
try {
    $Release = Invoke-RestMethod "https://api.github.com/repos/$Repo/releases/latest"
} catch {
    Fail 'Could not determine latest release version. Check your internet connection.'
}
$Version = "$($Release.tag_name)" -replace '^v', ''
if (-not $Version) { Fail 'Could not determine latest release version. Check your internet connection.' }
Write-Info "Latest version: v$Version"

# PROCESSOR_ARCHITECTURE reports the process, not the machine, so a 32-bit host
# under WOW64 says x86 while ARCHITEW6432 names what is really underneath.
$Arch = if ($env:PROCESSOR_ARCHITEW6432) { $env:PROCESSOR_ARCHITEW6432 } else { $env:PROCESSOR_ARCHITECTURE }
# Prism on Windows ARM64 runs the x64 zip. Native arm64 zips are preferred when present.
$AssetName = "wally-$Version-windows-x86_64.zip"
if ($Arch -eq 'ARM64') {
    $ArmAsset = $Release.assets | Where-Object { $_.name -eq "wally-$Version-windows-arm64.zip" } | Select-Object -First 1
    if ($ArmAsset) {
        $AssetName = "wally-$Version-windows-arm64.zip"
    } else {
        Write-Warn "No native ARM64 zip; installing the x64 build (Windows on ARM can run it)."
    }
} elseif ($Arch -ne 'AMD64') {
    Fail "Wally requires 64-bit Windows. Detected: $Arch"
}
$Asset = $Release.assets | Where-Object { $_.name -eq $AssetName } | Select-Object -First 1
if (-not $Asset) {
    Fail "v$Version does not publish $AssetName. Open an issue at https://github.com/$Repo/issues"
}
$ShaAsset = $Release.assets | Where-Object { $_.name -eq "$AssetName.sha256" } | Select-Object -First 1
if (-not $ShaAsset) {
    Fail "v$Version does not publish $AssetName.sha256. Refusing an unverified download."
}

$Temp = Join-Path ([IO.Path]::GetTempPath()) ('wally-' + [Guid]::NewGuid().ToString('N'))
New-Item -ItemType Directory -Path $Temp -Force | Out-Null
try {
    $Zip = Join-Path $Temp $AssetName
    Write-Info "Downloading $AssetName..."
    try {
        Invoke-WebRequest -Uri $Asset.browser_download_url -OutFile $Zip
    } catch {
        Fail "Could not download $($Asset.browser_download_url)"
    }

    # Through a file rather than straight into a variable: the asset is served
    # as octet-stream and the web cmdlets hand back bytes rather than text.
    $ShaFile = Join-Path $Temp $ShaAsset.name
    Invoke-WebRequest -Uri $ShaAsset.browser_download_url -OutFile $ShaFile
    $ShaLine = (Get-Content -Raw -LiteralPath $ShaFile).Trim()
    if ($ShaLine -notmatch '^([0-9A-Fa-f]{64})\s+\*?([^\r\n]+)$') {
        Fail "$($ShaAsset.name) is not a valid SHA-256 sidecar."
    }
    $Expected = $Matches[1].ToLowerInvariant()
    $ListedAsset = $Matches[2].Trim()
    if ($ListedAsset -ne $AssetName) {
        Fail "$($ShaAsset.name) names $ListedAsset instead of $AssetName."
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
