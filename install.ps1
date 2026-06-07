# AgentStateDeveloper (ASD) Windows installer
#
# Usage (PowerShell):
#   Invoke-WebRequest https://raw.githubusercontent.com/agentstatelabs/asd/main/install.ps1 | Invoke-Expression
#   # or, shorter:
#   iwr https://raw.githubusercontent.com/agentstatelabs/asd/main/install.ps1 | iex
#
# Downloads asd.exe / asd-mcp.exe / asd-serve.exe from the latest GitHub
# release and drops them in %LOCALAPPDATA%\asd\bin, which it adds to your
# user PATH.
#
# Environment:
#   $env:ASD_VERSION       — release tag to install (default: latest)
#   $env:ASD_GITHUB_REPO   — override the GitHub repo path (default: agentstatelabs/asd)
#
# Plan N t-001 (1.1.13): frictionless distribution. CTXone parity.

$ErrorActionPreference = "Stop"

$Repo = if ($env:ASD_GITHUB_REPO) { $env:ASD_GITHUB_REPO } else { "agentstatelabs/asd" }
$InstallDir = Join-Path $env:LOCALAPPDATA "asd\bin"
$Bins = @("asd", "asd-mcp", "asd-serve")

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
    Write-Host "Fetching latest release..."
    try {
        $Tag = (Invoke-RestMethod -Uri "https://api.github.com/repos/$Repo/releases/latest").tag_name
    } catch {
        Write-Error "Could not fetch latest release. Check network or set ASD_GITHUB_REPO."
        exit 1
    }
    if (-not $Tag) {
        Write-Host "No releases found. Build from source:" -ForegroundColor Yellow
        Write-Host ""
        Write-Host "  git clone https://github.com/$Repo.git"
        Write-Host "  cd asd"
        Write-Host "  cargo install --path crates/agentstatedeveloper-cli"
        Write-Host "  cargo install --path crates/agentstatedeveloper-mcp"
        exit 1
    }
    Write-Host "  Latest: $Tag"
}

# ─── Download each binary ───────────────────────────────────────────────────
Write-Host ""
Write-Host "Installing ASD $Tag..." -ForegroundColor Cyan

foreach ($bin in $Bins) {
    $Url = "https://github.com/$Repo/releases/download/$Tag/$bin-$Target.exe"
    $Dest = Join-Path $InstallDir "$bin.exe"
    Write-Host "  - $bin.exe"
    try {
        Invoke-WebRequest -Uri $Url -OutFile $Dest -UseBasicParsing
    } catch {
        Write-Error "Failed to download $Url"
        Write-Host "  (the release may not include this binary for $Target)" -ForegroundColor Yellow
        exit 1
    }
}

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

# ─── Next steps ─────────────────────────────────────────────────────────────
Write-Host ""
Write-Host "Get started:" -ForegroundColor Cyan
Write-Host "  # Index a repo"
Write-Host "  asd index ."
Write-Host ""
Write-Host "  # Register with your agent's MCP config"
Write-Host "  asd install claude    # or codex, cursor, gemini"
Write-Host ""
Write-Host "  # First context query"
Write-Host '  asd prepare-change "<describe your change>"'
Write-Host ""
Write-Host "Docs: https://github.com/$Repo#readme"
Write-Host ""
