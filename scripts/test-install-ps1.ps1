# Exercise install.ps1's checksum verification against a local fixture server.
#
# WHY THIS EXISTS
# ---------------
# install.ps1 gained sha256 verification in MR !38 (4f24c591), written to match
# install.sh's policy, but it had never been RUN - there is no PowerShell on the
# dev Mac, so the code was reasoned about, not executed. That matters more here
# than on the sh side because this path FAILS CLOSED: if the SHA256SUMS line
# never matches, the installer refuses and every Windows install dies. A bug
# here is not a degraded install, it is no install at all.
#
# A real release cannot exercise the interesting cases - you cannot ask GitHub
# for a tampered tarball or a truncated sums file. So this serves a fixture
# release over HTTP and points the installer at it with ASD_DOWNLOAD_BASE,
# which exists precisely so the script under test is the shipped file, byte for
# byte, rather than a copy with the URL edited out.
#
# The last three cases target the three things most likely to be wrong, called
# out when the task was filed:
#   * UPPERCASE sums   - Get-FileHash returns uppercase, sha256sum writes
#                        lowercase. The compare uses -ine deliberately; -cne
#                        would fail every install on letter case alone.
#   * CRLF sums        - sums are produced by sha256sum on ubuntu-22.04 (LF).
#                        A stray CR would sit between the filename and the
#                        regex's `$` anchor, which in .NET tolerates a trailing
#                        \n but NOT a trailing \r.
#   * binary-mode "*"  - sha256sum --binary writes "<hash> *<name>".
#
# Usage:
#   pwsh -File scripts/test-install-ps1.ps1
#   pwsh -File scripts/test-install-ps1.ps1 -Pwsh powershell   # Windows PS 5.1
#
# Requires python3 on PATH for the fixture server (present on GitHub's
# windows-latest runners).

[CmdletBinding()]
param(
    [string]$InstallScript = (Join-Path $PSScriptRoot "..\install.ps1"),
    [string]$Pwsh = "pwsh",
    [int]$Port = 8731
)

$ErrorActionPreference = "Stop"

$Tag     = "v9.9.9-test"
$Target  = "x86_64-pc-windows-msvc"
$Tarball = "asd-$Tag-$Target.tar.gz"

if (-not (Test-Path $InstallScript)) { throw "install script not found: $InstallScript" }
$InstallScript = (Resolve-Path $InstallScript).Path

# --- Encoding invariant ----------------------------------------------------
# Windows PowerShell 5.1 reads a .ps1 file in the system ANSI codepage unless
# the file carries a UTF-8 BOM. A UTF-8 file without one is therefore MOJIBAKE
# to 5.1 -- and the damage is not cosmetic: U+2500 (box drawing, "-") is the
# bytes E2 94 80, and 0x94 in CP1252 is a smart right-double-quote, which
# PowerShell treats as a STRING DELIMITER. install.ps1 once carried 235 of
# them in its section headers, so 5.1 could not parse the file at all.
#
# Asserted here as a cheap invariant rather than left to the behavioural cases,
# because a parse failure makes every case fail at once and buries the cause.
$InstallBytes = [System.IO.File]::ReadAllBytes($InstallScript)
$HasBom = ($InstallBytes.Length -ge 3) -and
          ($InstallBytes[0] -eq 0xEF) -and ($InstallBytes[1] -eq 0xBB) -and ($InstallBytes[2] -eq 0xBF)
$NonAscii = @($InstallBytes | Where-Object { $_ -gt 127 }).Count
if (-not $HasBom -and $NonAscii -gt 0) {
    Write-Host "  FAIL  encoding  install.ps1 has $NonAscii non-ASCII bytes and no UTF-8 BOM" -ForegroundColor Red
    Write-Host "        Windows PowerShell 5.1 will read it in the ANSI codepage and fail to parse." -ForegroundColor Red
    Write-Host "        Fix: keep the file pure ASCII, or write it with a UTF-8 BOM." -ForegroundColor Red
    exit 1
}
Write-Host ("encoding       : {0}, {1} non-ASCII bytes" -f $(if ($HasBom) { "UTF-8 BOM" } else { "no BOM" }), $NonAscii)

Write-Host "install script : $InstallScript"
Write-Host "powershell     : $Pwsh"
Write-Host ""

# --- Build the fixture release ---------------------------------------------
$Root = Join-Path ([System.IO.Path]::GetTempPath()) "asd-ps1-test-$(Get-Random)"
$Srv  = Join-Path $Root "srv"
New-Item -ItemType Directory -Force -Path $Srv | Out-Null

$Stage = Join-Path $Root "asd-$Tag-$Target"
New-Item -ItemType Directory -Force -Path $Stage | Out-Null
foreach ($b in @("asd.exe", "asd-mcp.exe", "asd-serve.exe")) {
    Set-Content -Path (Join-Path $Stage $b) -Value "fixture $b for $Tag" -Encoding ascii
}
Push-Location $Root
& tar -czf (Join-Path $Root $Tarball) "asd-$Tag-$Target"
Pop-Location
if (-not (Test-Path (Join-Path $Root $Tarball))) { throw "failed to build fixture tarball" }

$GoodHash = (Get-FileHash -Algorithm SHA256 -Path (Join-Path $Root $Tarball)).Hash.ToLower()
Write-Host "fixture sha256 : $GoodHash"

# Write a sums file with explicit bytes so line endings are exactly as intended
# - Set-Content would normalise them and quietly defeat the CRLF case.
function Write-Sums {
    param([string]$Path, [string]$Body)
    [System.IO.File]::WriteAllText($Path, $Body, (New-Object System.Text.UTF8Encoding $false))
}

function New-Case {
    param([string]$Name, [string]$Sums, [switch]$Tamper, [switch]$NoSums)
    $d = Join-Path $Srv $Name
    New-Item -ItemType Directory -Force -Path $d | Out-Null
    Copy-Item (Join-Path $Root $Tarball) (Join-Path $d $Tarball)
    if ($Tamper) {
        # Append after the sums were computed: the download succeeds, the
        # bytes differ, the published hash still describes the original.
        Add-Content -Path (Join-Path $d $Tarball) -Value "tampered" -Encoding ascii
    }
    if (-not $NoSums) { Write-Sums -Path (Join-Path $d "SHA256SUMS") -Body $Sums }
    return $d
}

$lf = "`n"
New-Case -Name "ok"        -Sums ("$GoodHash  $Tarball$lf")                            | Out-Null
New-Case -Name "tampered"  -Sums ("$GoodHash  $Tarball$lf") -Tamper                    | Out-Null
New-Case -Name "nosums"    -Sums ""                          -NoSums                   | Out-Null
New-Case -Name "omitted"   -Sums ("$GoodHash  asd-$Tag-x86_64-unknown-linux-gnu.tar.gz$lf") | Out-Null
New-Case -Name "upper"     -Sums ("$($GoodHash.ToUpper())  $Tarball$lf")               | Out-Null
New-Case -Name "crlf"      -Sums ("$GoodHash  $Tarball`r`n")                            | Out-Null
New-Case -Name "binmode"   -Sums ("$GoodHash *$Tarball$lf")                             | Out-Null

# --- Serve it --------------------------------------------------------------
# The documented install is `iwr <url> | iex`, which never touches the file
# system -- Invoke-WebRequest decodes the HTTP body by its charset, so it can
# succeed on a file that `powershell -File` cannot even parse. Serve the script
# itself so that path gets asserted too rather than assumed.
Copy-Item $InstallScript (Join-Path $Srv "install.ps1")

$py = Get-Command python -ErrorAction SilentlyContinue
if (-not $py) { $py = Get-Command python3 -ErrorAction SilentlyContinue }
if (-not $py) { throw "python is required for the fixture server but was not found on PATH" }

$server = Start-Process -FilePath $py.Source `
    -ArgumentList @("-m", "http.server", "$Port", "--bind", "127.0.0.1") `
    -WorkingDirectory $Srv -PassThru -WindowStyle Hidden

$ready = $false
foreach ($i in 1..40) {
    Start-Sleep -Milliseconds 250
    try {
        Invoke-WebRequest -Uri "http://127.0.0.1:$Port/ok/SHA256SUMS" -UseBasicParsing -TimeoutSec 2 | Out-Null
        $ready = $true; break
    } catch { }
}
if (-not $ready) {
    try { $server | Stop-Process -Force } catch { }
    throw "fixture server did not come up on port $Port"
}
Write-Host "fixture server : http://127.0.0.1:$Port"
Write-Host ""

# --- Isolate the install so the runner's real profile is untouched ---------
$RealLocalAppData = $env:LOCALAPPDATA
$SavedUserPath    = [Environment]::GetEnvironmentVariable("Path", "User")
$env:LOCALAPPDATA = Join-Path $Root "localappdata"
$BinDir = Join-Path $env:LOCALAPPDATA "asd\bin"
$env:ASD_VERSION  = $Tag

$Failures = New-Object System.Collections.Generic.List[string]

function Invoke-Case {
    param(
        [string]$Name,
        [int]$ExpectExit,
        [bool]$ExpectInstalled,
        [string[]]$MustSay = @(),
        [string[]]$MustNotSay = @(),
        [string]$Because
    )

    # install.ps1 creates the bin dir before it downloads, so a bare directory
    # is not evidence of an install - assert on the binaries themselves.
    if (Test-Path $BinDir) { Remove-Item -Recurse -Force $BinDir }

    $env:ASD_DOWNLOAD_BASE = "http://127.0.0.1:$Port/$Name"
    # The installer writes its refusals to stderr, and under
    # $ErrorActionPreference = "Stop" a native command's stderr arriving via
    # 2>&1 is raised as a terminating error by Windows PowerShell 5.1. Left
    # alone, this harness would abort on exactly the failure cases it exists to
    # assert. Relax it for the call and put it straight back.
    $savedEap = $ErrorActionPreference
    $ErrorActionPreference = "Continue"
    try {
        $out  = (& $Pwsh -NoProfile -File $InstallScript 2>&1 | Out-String)
        $code = $LASTEXITCODE
    } finally {
        $ErrorActionPreference = $savedEap
    }

    $installed = @("asd.exe", "asd-mcp.exe", "asd-serve.exe") |
        ForEach-Object { Test-Path (Join-Path $BinDir $_) }
    $allThere  = ($installed -notcontains $false) -and ($installed.Count -eq 3)

    $problems = New-Object System.Collections.Generic.List[string]
    if ($code -ne $ExpectExit)        { $problems.Add("exit $code, expected $ExpectExit") }
    if ($allThere -ne $ExpectInstalled) {
        $problems.Add("binaries installed=$allThere, expected $ExpectInstalled")
    }
    foreach ($s in $MustSay)    { if ($out -notmatch [regex]::Escape($s)) { $problems.Add("missing output: '$s'") } }
    foreach ($s in $MustNotSay) { if ($out -match  [regex]::Escape($s)) { $problems.Add("unexpected output: '$s'") } }

    if ($problems.Count -eq 0) {
        Write-Host ("  PASS  {0,-9} {1}" -f $Name, $Because) -ForegroundColor Green
    } else {
        Write-Host ("  FAIL  {0,-9} {1}" -f $Name, $Because) -ForegroundColor Red
        foreach ($p in $problems) { Write-Host "          - $p" -ForegroundColor Red }
        Write-Host "        ---- installer output ----" -ForegroundColor DarkGray
        foreach ($l in ($out -split "`r?`n")) { Write-Host "        $l" -ForegroundColor DarkGray }
        $Failures.Add($Name)
    }
}

try {
    Write-Host "The four cases install.sh was tested against:"
    Invoke-Case -Name "ok"       -ExpectExit 0 -ExpectInstalled $true `
        -MustSay @("checksum verified") -MustNotSay @("No SHA256SUMS published") `
        -Because "sums match -> verified, 3 binaries installed"

    Invoke-Case -Name "tampered" -ExpectExit 1 -ExpectInstalled $false `
        -MustSay @("Checksum mismatch") `
        -Because "tarball tampered -> refuses, nothing installed"

    Invoke-Case -Name "nosums"   -ExpectExit 0 -ExpectInstalled $true `
        -MustSay @("No SHA256SUMS published") -MustNotSay @("checksum verified") `
        -Because "no sums published -> warns, still installs (must stay soft)"

    Invoke-Case -Name "omitted"  -ExpectExit 1 -ExpectInstalled $false `
        -MustSay @("does not list") `
        -Because "sums omit this tarball -> refuses, nothing installed"

    Write-Host ""
    Write-Host "The documented one-liner (iwr | iex), which reads no file:"
    if (Test-Path $BinDir) { Remove-Item -Recurse -Force $BinDir }
    $env:ASD_DOWNLOAD_BASE = "http://127.0.0.1:$Port/ok"
    $oneLiner = "iex ((Invoke-WebRequest -Uri 'http://127.0.0.1:$Port/install.ps1' -UseBasicParsing).Content)"
    $savedEap = $ErrorActionPreference
    $ErrorActionPreference = "Continue"
    try {
        $out  = (& $Pwsh -NoProfile -Command $oneLiner 2>&1 | Out-String)
        $code = $LASTEXITCODE
    } finally { $ErrorActionPreference = $savedEap }
    $ok = @("asd.exe", "asd-mcp.exe", "asd-serve.exe") |
        ForEach-Object { Test-Path (Join-Path $BinDir $_) }
    if (($ok -notcontains $false) -and ($out -match "checksum verified")) {
        Write-Host ("  PASS  {0,-9} {1}" -f "oneliner", "iwr | iex installs and verifies") -ForegroundColor Green
    } else {
        Write-Host ("  FAIL  {0,-9} {1}" -f "oneliner", "iwr | iex installs and verifies") -ForegroundColor Red
        foreach ($l in ($out -split "`r?`n")) { Write-Host "        $l" -ForegroundColor DarkGray }
        $Failures.Add("oneliner")
    }

    Write-Host ""
    Write-Host "Regression guards for the three most-likely-wrong details:"
    Invoke-Case -Name "upper"    -ExpectExit 0 -ExpectInstalled $true `
        -MustSay @("checksum verified") `
        -Because "UPPERCASE sums still match (-ine, not -cne)"

    Invoke-Case -Name "crlf"     -ExpectExit 0 -ExpectInstalled $true `
        -MustSay @("checksum verified") `
        -Because "CRLF sums still match (no CR before the end anchor)"

    Invoke-Case -Name "binmode"  -ExpectExit 0 -ExpectInstalled $true `
        -MustSay @("checksum verified") `
        -Because "binary-mode '*name' marker still matches"
}
finally {
    try { $server | Stop-Process -Force } catch { }
    $env:LOCALAPPDATA = $RealLocalAppData
    Remove-Item Env:\ASD_DOWNLOAD_BASE -ErrorAction SilentlyContinue
    Remove-Item Env:\ASD_VERSION       -ErrorAction SilentlyContinue
    # install.ps1 appends its bin dir to the USER PATH; put it back so a
    # local run does not leave a temp path wired into the developer's profile.
    if ($null -ne $SavedUserPath) {
        [Environment]::SetEnvironmentVariable("Path", $SavedUserPath, "User")
    }
    Remove-Item -Recurse -Force $Root -ErrorAction SilentlyContinue
}

Write-Host ""
if ($Failures.Count -gt 0) {
    Write-Host "FAILED: $($Failures -join ', ')" -ForegroundColor Red
    exit 1
}
Write-Host "All 8 cases passed." -ForegroundColor Green
exit 0
