# AgentStateDeveloper (ASD) Windows installer
#
# Usage (PowerShell):
#   iwr https://raw.githubusercontent.com/agentstatelabs/AgentStateDeveloper/main/install.ps1 | iex
#
# Downloads the platform-specific tarball from the
# agentstatelabs/agentstatedeveloper-releases mirror, extracts the binaries,
# and drops them in %LOCALAPPDATA%\asd\bin (added to your user PATH).
#
# Environment:
#   $env:ASD_VERSION         — release tag to install (default: latest, e.g. "v1.1.14")
#   $env:ASD_RELEASES_REPO   — GitHub repo hosting release artifacts
#                              (default: agentstatelabs/agentstatedeveloper-releases)
#
# Plan N t-001 (1.1.14): frictionless distribution. CTXone parity.

$ErrorActionPreference = "Stop"

$ReleasesRepo = if ($env:ASD_RELEASES_REPO) { $env:ASD_RELEASES_REPO } else { "agentstatelabs/agentstatedeveloper-releases" }
$SourceRepo = "agentstatelabs/AgentStateDeveloper"
$InstallDir = Join-Path $env:LOCALAPPDATA "asd\bin"

Write-Host ""
Write-Host "AgentStateDeveloper installer (Windows)" -ForegroundColor Cyan
Write-Host ""

# ─── Architecture detection ─────────────────────────────────────────────────
$Arch = switch ($env:PROCESSOR_ARCHITECTURE) {
    "AMD64" { "x86_64" }
    "ARM64" {
        Write-Host "Note: ARM64 Windows isn't a release target yet." -ForegroundColor Yellow
        Write-Host "The x86_64 binaries should run under Windows ARM emulation." -ForegroundColor Yellow
        "x86_64"
    }
    default {
        Write-Error "Unsupported architecture: $env:PROCESSOR_ARCHITECTURE"
        exit 1
    }
}
$Target = "$Arch-pc-windows-msvc"

Write-Host "  Target: $Target"
Write-Host "  Dir:    $InstallDir"
Write-Host ""

New-Item -ItemType Directory -Force -Path $InstallDir | Out-Null

# ─── Resolve release tag ────────────────────────────────────────────────────
if ($env:ASD_VERSION) {
    $Tag = $env:ASD_VERSION
    Write-Host "  Pinned: $Tag"
} else {
    Write-Host "Fetching latest release from $ReleasesRepo..."
    try {
        $Tag = (Invoke-RestMethod -Uri "https://api.github.com/repos/$ReleasesRepo/releases/latest").tag_name
    } catch {
        Write-Error "Could not fetch latest release. Check network or set ASD_RELEASES_REPO."
        exit 1
    }
    if (-not $Tag) {
        Write-Host "No releases found. Build from source:" -ForegroundColor Yellow
        Write-Host ""
        Write-Host "  git clone https://github.com/$SourceRepo.git"
        Write-Host "  cd AgentStateDeveloper"
        Write-Host "  cargo install --path crates/agentstatedeveloper-cli"
        Write-Host "  cargo install --path crates/agentstatedeveloper-mcp"
        exit 1
    }
    Write-Host "  Latest: $Tag"
}

# ─── Download + extract the tarball ────────────────────────────────────────
$Tarball = "asd-$Tag-$Target.tar.gz"
$Url = "https://github.com/$ReleasesRepo/releases/download/$Tag/$Tarball"
$Tmp = Join-Path $env:TEMP "asd-install-$Tag"
New-Item -ItemType Directory -Force -Path $Tmp | Out-Null

Write-Host ""
Write-Host "Downloading $Tag..." -ForegroundColor Cyan
Write-Host "  $Url"
try {
    Invoke-WebRequest -Uri $Url -OutFile (Join-Path $Tmp $Tarball) -UseBasicParsing
} catch {
    Write-Error "Failed to download $Url"
    Write-Host "  (the release may not include a Windows binary yet)" -ForegroundColor Yellow
    Write-Host "  Build from source: see the docs at https://github.com/$SourceRepo" -ForegroundColor Yellow
    exit 1
}

# ─── Verify the download ────────────────────────────────────────────────────
# The Homebrew formula pins a sha256 per target and refuses on mismatch; this
# path had no equivalent and trusted TLS alone. The release publishes
# SHA256SUMS beside the tarballs, so verify against it.
#
# A release cut before SHA256SUMS existed has no such file: warn there so a
# pinned older -Version still installs. A file that IS present and does NOT
# match is a hard failure.
$SumsUrl = "https://github.com/$ReleasesRepo/releases/download/$Tag/SHA256SUMS"
$SumsPath = Join-Path $Tmp "SHA256SUMS"
$HaveSums = $true
try {
    Invoke-WebRequest -Uri $SumsUrl -OutFile $SumsPath -UseBasicParsing
} catch {
    $HaveSums = $false
    Write-Host "  ! No SHA256SUMS published for $Tag - cannot verify the download." -ForegroundColor Yellow
}
if ($HaveSums) {
    $Expected = $null
    foreach ($line in Get-Content $SumsPath) {
        # "<64 hex>  <name>", optionally "*<name>" for binary mode.
        if ($line -match "^([0-9a-fA-F]{64})\s+\*?(\./)?" + [regex]::Escape($Tarball) + "$") {
            $Expected = $Matches[1]
            break
        }
    }
    if (-not $Expected) {
        Write-Error "SHA256SUMS for $Tag does not list $Tarball. Refusing to install."
        exit 1
    }
    $Actual = (Get-FileHash -Algorithm SHA256 -Path (Join-Path $Tmp $Tarball)).Hash
    if ($Actual -ine $Expected) {
        Write-Host "  expected $Expected"
        Write-Host "  actual   $Actual"
        Write-Error "Checksum mismatch for $Tarball. Refusing to install."
        exit 1
    }
    Write-Host "  checksum verified" -ForegroundColor Green
}

# tar is available on Windows 10 1803+ / Windows 11 by default.
Write-Host "  Extracting..."
& tar -xzf (Join-Path $Tmp $Tarball) -C $Tmp --strip-components=1
if ($LASTEXITCODE -ne 0) {
    Write-Error "Extraction failed (tar exit $LASTEXITCODE)."
    exit 1
}

foreach ($bin in @("asd.exe", "asd-mcp.exe", "asd-serve.exe")) {
    $src = Join-Path $Tmp $bin
    if (-not (Test-Path $src)) {
        Write-Error "Tarball is missing $bin — release artifact is malformed."
        exit 1
    }
    Copy-Item -Force $src (Join-Path $InstallDir $bin)
}

Remove-Item -Recurse -Force $Tmp -ErrorAction SilentlyContinue

Write-Host ""
Write-Host "Installed to $InstallDir" -ForegroundColor Green

# ─── PATH update (user-level, no admin required) ───────────────────────────
$UserPath = [Environment]::GetEnvironmentVariable("Path", "User")
if (-not $UserPath) { $UserPath = "" }
$PathEntries = $UserPath -split ';' | Where-Object { $_ -ne "" }

if ($PathEntries -notcontains $InstallDir) {
    $NewPath = ($PathEntries + $InstallDir) -join ';'
    [Environment]::SetEnvironmentVariable("Path", $NewPath, "User")
    Write-Host ""
    Write-Host "Added $InstallDir to your user PATH." -ForegroundColor Green
    Write-Host "Open a new PowerShell window for it to take effect." -ForegroundColor Yellow
} else {
    Write-Host "(already on PATH)" -ForegroundColor DarkGray
}

Write-Host ""
Write-Host "Get started:" -ForegroundColor Cyan
Write-Host "  # Index a repo"
Write-Host "  asd index ."
Write-Host ""
Write-Host "  # Register with your agent's MCP config"
Write-Host "  asd install claude    # or codex, cursor, gemini (Plan N t-005)"
Write-Host ""
Write-Host "  # First context query"
Write-Host '  asd prepare-change "<describe your change>"'
Write-Host ""
Write-Host "Docs:     https://github.com/$SourceRepo#readme"
Write-Host "Releases: https://github.com/$ReleasesRepo/releases"
Write-Host ""
