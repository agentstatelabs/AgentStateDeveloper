# Verify a PUBLISHED release actually installs on Windows.
#
# WHY THIS EXISTS
# ---------------
# scripts/test-install-ps1.ps1 (plan install-path-verification t-001) proves
# install.ps1's LOGIC against a local fixture release. It cannot prove that a
# REAL release installs, because the fixture supplies its own tarball and its
# own SHA256SUMS.
#
# GitLab's `verify-install` job covers the shell path only - it runs in a Linux
# container with no PowerShell. So until this existed, nothing at tag time
# checked that asd-<tag>-x86_64-pc-windows-msvc.tar.gz downloads, that its
# sha256 matches the published sums, that tar extracts it, that the three .exe
# files land, or that asd.exe reports the tag.
#
# The Windows tarball is also the ONE artifact whose published sum nothing
# else cross-checks: the Homebrew formula pins a sha256 per target but has no
# Windows bottle, so verify-release.sh's formula-vs-SHA256SUMS comparison skips
# it entirely.
#
# This runs install.ps1 UNMODIFIED against the real release. ASD_DOWNLOAD_BASE
# is explicitly cleared - a stray value would silently turn this back into a
# fixture test, which is exactly the failure it exists to rule out.
#
# Usage:
#   pwsh -File scripts/verify-release-windows.ps1 -Tag v1.2.0
#   pwsh -File scripts/verify-release-windows.ps1 -Tag v1.2.0 -Pwsh powershell

[CmdletBinding()]
param(
    [Parameter(Mandatory = $true)][string]$Tag,
    [string]$Pwsh = "pwsh",
    [string]$ReleasesRepo = "agentstatelabs/agentstatedeveloper-releases",
    [string]$InstallScript = (Join-Path $PSScriptRoot "..\install.ps1"),
    [int]$WaitSeconds = 300
)

$ErrorActionPreference = "Stop"

if (-not (Test-Path $InstallScript)) { throw "install script not found: $InstallScript" }
$InstallScript = (Resolve-Path $InstallScript).Path

$Target   = "x86_64-pc-windows-msvc"
$Tarball  = "asd-$Tag-$Target.tar.gz"
$Base     = "https://github.com/$ReleasesRepo/releases/download/$Tag"
$BinDir   = Join-Path $env:LOCALAPPDATA "asd\bin"
$Binaries = @("asd.exe", "asd-mcp.exe", "asd-serve.exe")

$Failures = New-Object System.Collections.Generic.List[string]
function Fail { param([string]$m) Write-Host "  FAIL  $m" -ForegroundColor Red; $Failures.Add($m) }
function Pass { param([string]$m) Write-Host "  PASS  $m" -ForegroundColor Green }

Write-Host ""
Write-Host "Verifying published release on Windows"
Write-Host "  tag        : $Tag"
Write-Host "  repo       : $ReleasesRepo"
Write-Host "  shell      : $Pwsh"
Write-Host "  installer  : $InstallScript"
Write-Host ""

# --- Preconditions, asserted rather than assumed ---------------------------
# A job that would pass on a dirty runner reproduces the blind spot instead of
# closing it: an asd already on PATH means a later version check could be
# satisfied by a PREVIOUS install rather than by this one.
Write-Host "Preconditions (a dirty runner would make a green meaningless):"
$existing = Get-Command asd -ErrorAction SilentlyContinue
if ($existing) {
    Fail "asd is already on PATH at $($existing.Source) - this runner is not clean"
} else {
    Pass "no asd on PATH"
}
$present = @($Binaries | Where-Object { Test-Path (Join-Path $BinDir $_) })
if ($present.Count -gt 0) {
    Fail "$BinDir already contains $($present -join ', ') - this runner is not clean"
} else {
    Pass "$BinDir has no asd binaries"
}

# --- Wait for the assets to be reachable -----------------------------------
# Publishing is cross-repo, so allow for propagation. Distinguish "not there
# yet" from "wrong", the lesson of the shell-side verify-install false red:
# an ambiguous timeout message gets the whole check ignored.
Write-Host ""
Write-Host "Release assets:"
function Wait-Url {
    param([string]$Url, [string]$Label)
    $waited = 0
    while ($true) {
        try {
            Invoke-WebRequest -Uri $Url -Method Head -UseBasicParsing -TimeoutSec 15 | Out-Null
            Pass "$Label reachable$(if ($waited) { " after ${waited}s" })"
            return $true
        } catch {
            if ($waited -ge $WaitSeconds) {
                Fail "$Label not reachable after ${waited}s: $Url"
                Write-Host "        if it resolves later this was propagation, not a bad release -" -ForegroundColor Yellow
                Write-Host "        raise -WaitSeconds rather than assuming the publish failed" -ForegroundColor Yellow
                return $false
            }
            Start-Sleep -Seconds 15
            $waited += 15
        }
    }
}
$haveTarball = Wait-Url -Url "$Base/$Tarball"     -Label "$Tarball"
$haveSums    = Wait-Url -Url "$Base/SHA256SUMS"   -Label "SHA256SUMS"

if (-not ($haveTarball -and $haveSums)) {
    Write-Host ""
    Write-Host "FAILED: release assets missing; not attempting the install." -ForegroundColor Red
    exit 1
}

# --- Install from the real release -----------------------------------------
Write-Host ""
Write-Host "Installing from the published release:"
$env:ASD_VERSION       = $Tag
$env:ASD_RELEASES_REPO = $ReleasesRepo
# Load-bearing: any value here would point the installer at a fixture and make
# this test prove nothing about the actual release.
Remove-Item Env:\ASD_DOWNLOAD_BASE -ErrorAction SilentlyContinue

# Native stderr through 2>&1 is a TERMINATING error under EAP=Stop in Windows
# PowerShell 5.1, so relax it around the call and put it straight back.
$savedEap = $ErrorActionPreference
$ErrorActionPreference = "Continue"
try {
    $out  = (& $Pwsh -NoProfile -File $InstallScript 2>&1 | Out-String)
    $code = $LASTEXITCODE
} finally {
    $ErrorActionPreference = $savedEap
}

if ($code -eq 0) { Pass "installer exited 0" } else { Fail "installer exited $code, expected 0" }

# The fail-open hazard this whole line of work is about: a missing SHA256SUMS
# only WARNS, so a broken publish step degrades to no verification at all with
# no error. Seeing the warning on a release that should carry sums is itself
# the failure.
if ($out -match "checksum verified") {
    Pass "checksum verified against the published SHA256SUMS"
} else {
    Fail "installer did not print 'checksum verified'"
}
if ($out -match "No SHA256SUMS published") {
    Fail "installer took the soft no-sums path - the publish step did not ship SHA256SUMS"
} else {
    Pass "did not fall back to the unverified path"
}

foreach ($b in $Binaries) {
    if (Test-Path (Join-Path $BinDir $b)) { Pass "$b installed" } else { Fail "$b missing from $BinDir" }
}

# --- Version, by full path -------------------------------------------------
# Deliberately NOT via PATH: install.ps1 edits the user PATH, which does not
# affect this already-running process, and resolving through PATH could pick
# up some other asd entirely.
$asd = Join-Path $BinDir "asd.exe"
if (Test-Path $asd) {
    $expected = $Tag -replace '^v', ''
    $ErrorActionPreference = "Continue"
    try { $ver = (& $asd --version 2>&1 | Out-String).Trim() } finally { $ErrorActionPreference = $savedEap }
    if ($ver -match [regex]::Escape($expected)) {
        Pass "asd.exe --version reports $expected ($ver)"
    } else {
        Fail "asd.exe --version reported '$ver', expected it to contain '$expected'"
    }
} else {
    Fail "cannot check version: $asd does not exist"
}

Write-Host ""
if ($Failures.Count -gt 0) {
    Write-Host "FAILED ($($Failures.Count)):" -ForegroundColor Red
    foreach ($f in $Failures) { Write-Host "  - $f" -ForegroundColor Red }
    Write-Host ""
    Write-Host "---- installer output ----" -ForegroundColor DarkGray
    foreach ($l in ($out -split "`r?`n")) { Write-Host "  $l" -ForegroundColor DarkGray }
    exit 1
}
Write-Host "Published release installs and verifies on Windows ($Pwsh)." -ForegroundColor Green
exit 0
