# =============================================================================
# scripts/common.ps1 - helpers shared by the repository scripts.
#
# Dot-source it from another script:
#
#     . "$PSScriptRoot/common.ps1"
#
# NOTE on error handling: these scripts deliberately do NOT set
# `$ErrorActionPreference = 'Stop'`. Cargo writes its progress to stderr, and
# under Windows PowerShell 5.1 a native command's stderr output turns into a
# terminating error when that preference is Stop. Every failure path below is
# therefore checked explicitly instead of relying on the global preference.
# =============================================================================

# Repository root (this file lives in <root>/scripts).
$RepoRoot = Split-Path -Parent $PSScriptRoot

# -----------------------------------------------------------------------------
# Cargo.toml
# -----------------------------------------------------------------------------

function Get-CrateMetadata {
    <#
    .SYNOPSIS
    Read `name` and `version` straight out of Cargo.toml.

    Only the first standalone `key = "value"` occurrence is matched, which is
    the one in `[package]`; the entries inside inline dependency tables
    (`windows = { version = "0.58" }`) start with a different key, so they cannot
    match. That is enough for the two values the release pipeline needs and
    avoids taking a TOML parser as a dependency just for packaging.
    #>
    param([Parameter(Mandatory)][string]$Manifest)

    $text = Get-Content -LiteralPath $Manifest -Raw
    $name = 'pppoe'
    $version = '0.0.0'

    if ($text -match '(?m)^\s*name\s*=\s*"([^"]+)"') { $name = $Matches[1] }
    if ($text -match '(?m)^\s*version\s*=\s*"([^"]+)"') { $version = $Matches[1] }

    return [pscustomobject]@{
        Name     = $name
        Version  = $version
        Manifest = $Manifest
    }
}

# -----------------------------------------------------------------------------
# CHANGELOG.md
# -----------------------------------------------------------------------------

function Get-ChangelogSection {
    <#
    .SYNOPSIS
    Extract the `## [<version>]` section of CHANGELOG.md.

    The release workflow uses this to publish the curated notes instead of
    GitHub's list of commit subjects. Returns $null when the file or the section
    is missing, so the caller can fall back to --generate-notes.

    A heading looks like `## [0.1.0]` or `## [0.1.0] - 2026-09-15`. The section
    ends at the next `## ` heading or at the link reference block at the bottom
    of the file.
    #>
    param(
        [Parameter(Mandatory)][string]$Path,
        [Parameter(Mandatory)][string]$Version
    )

    if (-not (Test-Path -LiteralPath $Path)) { return $null }

    # Anchored so that "1.0.0" cannot match inside "11.0.0".
    $heading = '^##\s+\[?' + [regex]::Escape($Version) + '\]?(\s|$)'
    $lines = Get-Content -LiteralPath $Path -Encoding UTF8
    $collecting = $false
    $body = New-Object System.Collections.Generic.List[string]

    foreach ($line in $lines) {
        if (-not $collecting) {
            if ($line -match $heading) { $collecting = $true }
            continue
        }
        if ($line -match '^##\s') { break }
        if ($line -match '^\[[^\]]+\]:\s*\S') { break }
        $body.Add($line)
    }

    $text = ($body -join "`n").Trim()
    if (-not $text) { return $null }
    return $text
}

# -----------------------------------------------------------------------------
# Cargo discovery
# -----------------------------------------------------------------------------

function Test-CargoUsable {
    <#
    .SYNOPSIS
    `$true` when this cargo can actually be executed.

    Exists because a broken rustup install makes a *present* `cargo.exe`
    unrunnable: `%USERPROFILE%\.cargo\bin\cargo.exe` is a 0 byte symlink to
    rustup.exe, and when that link cannot be resolved Windows PowerShell throws
    NativeCommandFailed instead of starting anything. Checking up front turns a
    cryptic exception into an actionable message.
    #>
    param([Parameter(Mandatory)][string]$Cargo)

    if (-not (Test-Path -LiteralPath $Cargo)) { return $false }

    $previous = $ErrorActionPreference
    $ErrorActionPreference = 'Continue'
    try {
        $output = & $Cargo --version 2>&1
        if ($LASTEXITCODE -ne 0) { return $false }
        return (($output | Out-String) -match '^cargo\s')
    } catch {
        return $false
    } finally {
        $ErrorActionPreference = $previous
    }
}

function Resolve-Cargo {
    <#
    .SYNOPSIS
    Pick the cargo to use: `cargo` from PATH, or an explicit toolchain bin dir.

    Normal case: cargo comes from PATH (a healthy rustup install). The override
    exists only for environments where the rustup shims are unusable; in that
    case Initialize-RustEnvironment has to fix up PATH, RUSTC and CARGO as well,
    because `cargo fmt` / `cargo clippy` are separate executables resolved
    through PATH and the rustc cargo spawns would hit the same dead shims.
    #>
    param([string]$ToolchainBin)

    $explicit = $ToolchainBin
    if (-not $explicit -and $env:PPPOE_TOOLCHAIN_BIN) { $explicit = $env:PPPOE_TOOLCHAIN_BIN }

    if ($explicit) {
        $cargo = Join-Path $explicit 'cargo.exe'
        if (-not (Test-Path -LiteralPath $cargo)) {
            throw "-ToolchainBin '$explicit' does not contain cargo.exe"
        }
        if (-not (Test-CargoUsable -Cargo $cargo)) {
            throw "'$cargo' exists but cannot be executed."
        }
        return [pscustomobject]@{
            Cargo           = $cargo
            BinDir          = $explicit
            Source          = 'explicit override'
            UseExplicitPath = $true
            Version         = ((& $cargo --version) | Out-String).Trim()
        }
    }

    $command = Get-Command cargo -ErrorAction SilentlyContinue
    if (-not $command -or -not $command.Source) {
        throw @'
cargo was not found on PATH.

Install the Rust toolchain from https://rustup.rs, then open a new shell so the
updated PATH is picked up. If you cannot change the machine, point the scripts
straight at an existing toolchain with -ToolchainBin or $env:PPPOE_TOOLCHAIN_BIN.
'@
    }

    if (-not (Test-CargoUsable -Cargo $command.Source)) {
        $hint = @'
The file exists but cannot be executed. The usual cause is a broken rustup
install: %USERPROFILE%\.cargo\bin\cargo.exe is a 0 byte symlink to rustup.exe
that no longer resolves (Windows reports "No application is associated with the
specified file for this operation").

Repair rustup:

    rustup self update
    rustup toolchain install stable

Or bypass the shims entirely by using a toolchain directory directly:

    $env:PPPOE_TOOLCHAIN_BIN = "$env:USERPROFILE\.rustup\toolchains\stable-x86_64-pc-windows-msvc\bin"
    .\scripts\check.ps1

    # or, per invocation:
    .\scripts\check.ps1 -ToolchainBin "$env:USERPROFILE\.rustup\toolchains\stable-x86_64-pc-windows-msvc\bin"
'@
        throw "'$($command.Source)' cannot be executed.`n`n$hint"
    }

    return [pscustomobject]@{
        Cargo           = $command.Source
        BinDir          = Split-Path -Parent $command.Source
        Source          = 'PATH'
        UseExplicitPath = $false
        Version         = ((& $command.Source --version) | Out-String).Trim()
    }
}

function Initialize-RustEnvironment {
    <#
    .SYNOPSIS
    Wire up PATH/RUSTC/CARGO when an explicit toolchain directory is in use.
    #>
    param([Parameter(Mandatory)][object]$Toolchain)

    if (-not $Toolchain.UseExplicitPath) { return }

    $env:PATH = "$($Toolchain.BinDir);$env:PATH"
    $env:RUSTC = Join-Path $Toolchain.BinDir 'rustc.exe'
    $env:CARGO = $Toolchain.Cargo
}

function Write-ToolchainBanner {
    param([Parameter(Mandatory)][object]$Toolchain)

    Write-Host "cargo     : $($Toolchain.Version)   [$($Toolchain.Cargo)]"
    Write-Host "            (resolved via $($Toolchain.Source))"

    $rustc = 'rustc'
    if ($Toolchain.UseExplicitPath) { $rustc = Join-Path $Toolchain.BinDir 'rustc.exe' }
    Write-Host "rustc     : $(((& $rustc --version 2>&1) | Out-String).Trim())"
    Write-Host "powershell: $($PSVersionTable.PSVersion)"
}

# -----------------------------------------------------------------------------
# Step runner
# -----------------------------------------------------------------------------

$script:CheckFailures = New-Object System.Collections.Generic.List[string]

function Invoke-CheckStep {
    <#
    .SYNOPSIS
    Run one gate, print a readable result line, and record failures.

    The action script block must return the exit code of the command it ran
    (typically `return $LASTEXITCODE`).
    #>
    param(
        [Parameter(Mandatory)][string]$Name,
        [Parameter(Mandatory)][scriptblock]$Action
    )

    Write-Host ''
    Write-Host "==> $Name" -ForegroundColor Cyan

    $stopwatch = [System.Diagnostics.Stopwatch]::StartNew()
    $code = 1
    try {
        $result = & $Action
        # The action contract is "return the exit code", but a command that also
        # writes to stdout (cargo test does) turns the script block's output into
        # an array whose last element is still that exit code. Accept both.
        if ($result -is [array]) {
            if ($result.Count -gt 0) { $code = $result[$result.Count - 1] } else { $code = $LASTEXITCODE }
        } elseif ($null -ne $result) {
            $code = $result
        } else {
            $code = $LASTEXITCODE
        }
        if ($null -eq $code -or $code -isnot [int]) { $code = 1 }
    } catch {
        Write-Host "    exception: $($_.Exception.Message)" -ForegroundColor Red
        $code = 1
    }
    $stopwatch.Stop()

    $seconds = $stopwatch.Elapsed.TotalSeconds
    if ($code -eq 0) {
        Write-Host ("    OK      ({0:N1}s)" -f $seconds) -ForegroundColor Green
    } else {
        Write-Host ("    FAILED  (exit {0}, {1:N1}s)" -f $code, $seconds) -ForegroundColor Red
        $script:CheckFailures.Add($Name) | Out-Null
    }
}

function Complete-CheckRun {
    param([Parameter(Mandatory)][string]$Title)

    Write-Host ''
    Write-Host ('-' * 72)
    if ($script:CheckFailures.Count -eq 0) {
        Write-Host "$Title`: ALL CHECKS PASSED" -ForegroundColor Green
        exit 0
    }
    Write-Host "$Title`: FAILED -> $($script:CheckFailures -join ', ')" -ForegroundColor Red
    exit 1
}
