$ErrorActionPreference = "Stop"
Set-StrictMode -Version Latest

# Proves install.ps1's WALLY_INSTALL_VERSION / WALLY_INSTALL_BASE_URL overrides
# (the same shape and names as install.sh's) without touching a real GitHub
# release, a real Program Files/LOCALAPPDATA install, or this machine's real
# user PATH:
#   - WALLY_INSTALL_BASE_URL alone is refused (a mirror has no latest-release
#     lookup, so it needs an explicit version).
#   - a malformed WALLY_INSTALL_VERSION is refused before any network call.
#   - both set together installs from a local mirror -- a plain directory
#     served over file:// -- verifying the .sha256 exactly as the GitHub path
#     does.
#   - a corrupted .sha256 at the mirror is still caught.
#   - neither set still takes the original (pre-override) branch.
#
# Every invocation below gets its own isolated $env:LOCALAPPDATA, and this
# script restores the real per-user PATH after every single call -- not just
# at the end -- because install.ps1 writes PATH via
# [Environment]::SetEnvironmentVariable(..., 'User'), which is a machine-wide
# registry value install.ps1 does not scope to LOCALAPPDATA the way the
# install directory itself is scoped.

$RepoRoot = (Resolve-Path (Join-Path $PSScriptRoot '..\..')).Path
$InstallScript = Join-Path $RepoRoot 'install.ps1'
$Work = Join-Path ([IO.Path]::GetTempPath()) ('wally-install-ps1-test-' + [Guid]::NewGuid().ToString('N'))
New-Item -ItemType Directory -Path $Work -Force | Out-Null

$fails = 0
function Check([string]$Name, [bool]$Ok) {
    if ($Ok) { Write-Host "ok   $Name" } else { Write-Host "FAIL $Name"; $script:fails++ }
}

# A stub wally.exe that answers --version, built without depending on the
# build tree's compiler setup -- Add-Type's C# compiler is part of every
# PowerShell 5.1+ install.
function New-StubWally([string]$Path) {
    New-Item -ItemType Directory -Path (Split-Path $Path -Parent) -Force | Out-Null
    Add-Type -OutputType ConsoleApplication -OutputAssembly $Path -TypeDefinition @'
using System;
class StubWally {
    static int Main(string[] args) {
        if (args.Length >= 1 && args[0] == "--version") {
            Console.WriteLine("wally 9.9.9 (stub)");
            return 0;
        }
        Console.Error.WriteLine("stub wally: unexpected args");
        return 2;
    }
}
'@
}

# Same arch resolution install.ps1 itself uses, so the fixture's asset name
# matches whatever this runner (x64 or ARM64) will actually ask for.
$Arch = if ($env:PROCESSOR_ARCHITEW6432) { $env:PROCESSOR_ARCHITEW6432 } else { $env:PROCESSOR_ARCHITECTURE }
$Plat = if ($Arch -eq 'ARM64') { 'windows-arm64' } else { 'windows-x86_64' }
$AssetName = "wally-9.9.9-$Plat.zip"

$Fixture = Join-Path $Work 'fixture'
$StageDir = Join-Path $Fixture "wally-$Plat"
New-StubWally (Join-Path $StageDir 'bin\wally.exe')
$MirrorDir = Join-Path $Fixture 'mirror'
New-Item -ItemType Directory -Path $MirrorDir -Force | Out-Null
$ZipPath = Join-Path $MirrorDir $AssetName
Compress-Archive -Path $StageDir -DestinationPath $ZipPath -Force
$Hash = (Get-FileHash -LiteralPath $ZipPath -Algorithm SHA256).Hash.ToLowerInvariant()
"$Hash *$AssetName" | Set-Content -LiteralPath "$ZipPath.sha256" -Encoding ascii -NoNewline
$MirrorUrl = 'file:///' + ($MirrorDir -replace '\\', '/')

# Runs install.ps1 once with the given env overrides, in an isolated
# LOCALAPPDATA, and puts the real per-user PATH back the way it found it
# whether or not the run succeeded.
function Invoke-Install([hashtable]$EnvOverrides) {
    $savedVars = @{}
    foreach ($k in 'WALLY_INSTALL_VERSION', 'WALLY_INSTALL_BASE_URL') {
        $savedVars[$k] = [Environment]::GetEnvironmentVariable($k, 'Process')
        Remove-Item -Path "env:$k" -ErrorAction SilentlyContinue
    }
    foreach ($k in $EnvOverrides.Keys) { Set-Item -Path "env:$k" $EnvOverrides[$k] }
    $isolated = Join-Path $Work ('isolated-' + [Guid]::NewGuid().ToString('N'))
    New-Item -ItemType Directory -Path $isolated -Force | Out-Null
    $savedLocalAppData = $env:LOCALAPPDATA
    $env:LOCALAPPDATA = $isolated
    $savedUserPath = [Environment]::GetEnvironmentVariable('Path', 'User')
    $result = @{ Ok = $true; Output = $null; Error = $null; InstallDir = (Join-Path $isolated 'Programs\wally') }
    try {
        # *>&1 rather than 2>&1: install.ps1's own status lines are
        # Write-Host (the Information stream, 6), which plain 2>&1 does not
        # bring into the pipeline Out-String reads.
        $result.Output = & $InstallScript *>&1 | Out-String
    } catch {
        $result.Ok = $false
        $result.Error = $_.Exception.Message
    } finally {
        foreach ($k in $savedVars.Keys) {
            if ($null -eq $savedVars[$k]) { Remove-Item -Path "env:$k" -ErrorAction SilentlyContinue }
            else { Set-Item -Path "env:$k" $savedVars[$k] }
        }
        $env:LOCALAPPDATA = $savedLocalAppData
        [Environment]::SetEnvironmentVariable('Path', $savedUserPath, 'User')
    }
    return $result
}

try {
    # --- WALLY_INSTALL_BASE_URL without WALLY_INSTALL_VERSION is refused ----
    $r = Invoke-Install @{ WALLY_INSTALL_BASE_URL = $MirrorUrl }
    Check "BASE_URL alone fails" (-not $r.Ok)
    Check "BASE_URL alone names the reason" ($r.Error -match 'needs WALLY_INSTALL_VERSION')

    # --- a malformed WALLY_INSTALL_VERSION is refused ------------------------
    $r = Invoke-Install @{ WALLY_INSTALL_VERSION = 'not-a-version' }
    Check "malformed version fails" (-not $r.Ok)
    Check "malformed version names the reason" ($r.Error -match 'must look like')

    # --- both set installs from the local mirror -----------------------------
    $r = Invoke-Install @{ WALLY_INSTALL_VERSION = '9.9.9'; WALLY_INSTALL_BASE_URL = $MirrorUrl }
    Check "mirror install succeeds" $r.Ok
    if (-not $r.Ok) { Write-Host "  error: $($r.Error)" }
    $installedExe = Join-Path $r.InstallDir 'wally.exe'
    Check "wally.exe landed under the isolated install dir" (Test-Path -LiteralPath $installedExe)
    if (Test-Path -LiteralPath $installedExe) {
        $out = & $installedExe --version
        Check "installed binary reports the mirrored version" ($out -match '9\.9\.9')
    }

    # --- a corrupted .sha256 at the mirror is still caught --------------------
    $badMirror = Join-Path $Work 'mirror-badsha'
    New-Item -ItemType Directory -Path $badMirror -Force | Out-Null
    Copy-Item $ZipPath $badMirror
    "0" * 64 + " *$AssetName" | Set-Content -LiteralPath (Join-Path $badMirror "$AssetName.sha256") -Encoding ascii -NoNewline
    $badMirrorUrl = 'file:///' + ($badMirror -replace '\\', '/')
    $r = Invoke-Install @{ WALLY_INSTALL_VERSION = '9.9.9'; WALLY_INSTALL_BASE_URL = $badMirrorUrl }
    Check "checksum mismatch is refused" (-not $r.Ok)
    Check "checksum mismatch names the reason" ($r.Error -match 'Checksum mismatch')

    # --- neither override set still takes the pre-existing branch ------------
    # No fixture covers this: it is either a real GitHub lookup (CI has
    # network) or the same "check your internet connection" failure the
    # unmodified script always gave. Either output proves the override
    # branch above is not what unset environment runs through.
    $r = Invoke-Install @{}
    Check "unset overrides still take the original latest-release branch" `
        ($r.Output -match 'Checking latest Wally release' -or $r.Error -match 'Could not determine latest release version')
} finally {
    Remove-Item -LiteralPath $Work -Recurse -Force -ErrorAction SilentlyContinue
}

if ($fails -eq 0) {
    Write-Host "all install.ps1 override cases pass"
} else {
    Write-Host "$fails install.ps1 override case(s) failed"
    exit 1
}
