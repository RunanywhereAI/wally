param([Parameter(Mandatory = $true)][string]$Zip)
$ErrorActionPreference = "Stop"
Set-StrictMode -Version Latest

# Run the *shipped* zip the way a clean user machine does: extract it to a fresh
# dir and launch wally.exe with only its own bin and the base system on PATH --
# no toolchain, no kit, no build tree. The build-tree smokes run under the MSVC
# dev environment, where a runtime DLL missing from the archive still resolves
# on PATH and never fails. This is what catches it (0xC0000135 on a clean arm64
# machine with no VC++ redistributable installed).

if (-not (Test-Path $Zip)) { throw "archive not found: $Zip" }
$Root = Join-Path ([IO.Path]::GetTempPath()) ([Guid]::NewGuid().ToString())
Expand-Archive -Path $Zip -DestinationPath $Root -Force
$Exe = Get-ChildItem $Root -Filter wally.exe -File -Recurse | Select-Object -First 1
if (-not $Exe) { throw "wally.exe not found in $Zip" }
$Bin = Split-Path $Exe.FullName -Parent

# System32 stays on PATH so the base OS resolves, but the MSVC runtime can be
# deployed centrally into System32, so a launch alone would pass even if the zip
# omits it. Assert the bundled DLLs are physically in the archive, so the guard
# proves the archive is self-contained rather than what the runner happens to have.
foreach ($pat in @("vcruntime140.dll", "msvcp140.dll", "libssl-3-*.dll", "libcrypto-3-*.dll")) {
    if (-not (Get-ChildItem -Path $Bin -Filter $pat -File -ErrorAction SilentlyContinue)) {
        throw "archive is missing '$pat' in bin/ (a user machine would fail even where CI's System32 hides it)"
    }
}

$Saved = $env:PATH
$env:PATH = "$Bin;$env:SystemRoot\System32;$env:SystemRoot"
try {
    & $Exe.FullName version
    if ($LASTEXITCODE -ne 0) { throw "clean-PATH launch exited $LASTEXITCODE (missing runtime DLL?)" }
    & $Exe.FullName about --json | Out-Null
    if ($LASTEXITCODE -ne 0) { throw "about --json exited $LASTEXITCODE" }
    Write-Host "clean-room launch ok: $($Exe.FullName)"
} catch {
    $env:PATH = $Saved                        # restore before the dump needs its own DLLs
    $d = Get-Command dumpbin.exe -ErrorAction SilentlyContinue
    if ($d) { & $d.Source /dependents $Exe.FullName }
    throw
} finally {
    $env:PATH = $Saved
    Remove-Item $Root -Recurse -Force -ErrorAction SilentlyContinue
}
