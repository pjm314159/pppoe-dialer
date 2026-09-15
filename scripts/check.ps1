#Requires -Version 5.1
<#
.SYNOPSIS
    The single validation gate for this repository.

.DESCRIPTION
    Runs the same gates in the same order locally, in the pre-commit hook and in
    CI, so "it passed on my machine" and "it passed in CI" cannot drift apart:

      1. cargo fmt --all -- --check
      2. cargo clippy --all-targets -- -D warnings
      3. cargo test --all-targets
      4. cargo build --release, then a smoke test of the produced binary

    Step 2 is stronger than it looks: `-D warnings` is handed to rustc as well,
    so ordinary compiler warnings fail the run too, and the `[lints.clippy]`
    table in Cargo.toml denies `unwrap_used`, `expect_used`, `panic`, `todo`,
    `unimplemented` and `dbg_macro` for every target including tests. That is how
    the "no unwrap anywhere" guarantee is enforced rather than merely reviewed.

.PARAMETER Fast
    Skip step 4 (the release build and its smoke test). Used by the pre-commit
    hook so a commit stays quick; CI always runs the full set.

.PARAMETER Locked
    Pass --locked to cargo, which fails when Cargo.lock is out of date. CI uses
    this so a forgotten lock file update cannot slip through.

.PARAMETER ToolchainBin
    Directory containing cargo.exe/rustc.exe, for environments where the rustup
    shims on PATH cannot be executed. Also settable via $env:PPPOE_TOOLCHAIN_BIN.
    On a healthy install this is not needed and should be left alone.

.EXAMPLE
    .\scripts\check.ps1

.EXAMPLE
    .\scripts\check.ps1 -Fast          # what the pre-commit hook runs
#>
[CmdletBinding()]
param(
    [switch]$Fast,
    [switch]$Locked,
    [string]$ToolchainBin
)

$ErrorActionPreference = 'Continue'
. "$PSScriptRoot/common.ps1"

Set-Location -LiteralPath $RepoRoot

$crate = Get-CrateMetadata -Manifest (Join-Path $RepoRoot 'Cargo.toml')
$lockArgs = @()
if ($Locked) { $lockArgs = @('--locked') }

Write-Host "pppoe $($crate.Version) - validating" -ForegroundColor White
if ($Fast) { Write-Host "mode      : fast (release build skipped)" }

try {
    $toolchain = Resolve-Cargo -ToolchainBin $ToolchainBin
} catch {
    Write-Host ''
    Write-Host $_.Exception.Message -ForegroundColor Red
    exit 1
}
Initialize-RustEnvironment -Toolchain $toolchain
Write-ToolchainBanner -Toolchain $toolchain

# Every cargo call is piped through Out-Host: that keeps its output on the
# console (live, instead of captured into the step result) so the only thing the
# script block returns is the exit code.

# 1. formatting ---------------------------------------------------------------
Invoke-CheckStep 'cargo fmt --all -- --check' {
    & $toolchain.Cargo fmt --all -- --check | Out-Host
    return $LASTEXITCODE
}

# 2. lints (rustc warnings + the Cargo.toml clippy denies) --------------------
Invoke-CheckStep 'cargo clippy --all-targets -- -D warnings' {
    & $toolchain.Cargo clippy @lockArgs --all-targets -- -D warnings | Out-Host
    return $LASTEXITCODE
}

# 3. unit tests ---------------------------------------------------------------
Invoke-CheckStep 'cargo test --all-targets' {
    & $toolchain.Cargo test @lockArgs --all-targets | Out-Host
    return $LASTEXITCODE
}

# 4. the shipping artifact builds and runs ------------------------------------
if (-not $Fast) {
    Invoke-CheckStep 'cargo build --release' {
        & $toolchain.Cargo build @lockArgs --release | Out-Host
        return $LASTEXITCODE
    }

    $exe = Join-Path $RepoRoot 'target\release\pppoe.exe'
    Invoke-CheckStep "smoke test: $($crate.Name).exe --version" {
        if (-not (Test-Path -LiteralPath $exe)) {
            Write-Host "    $exe was not produced" -ForegroundColor Red
            return 1
        }
        $output = (& $exe --version 2>&1 | Out-String).Trim()
        Write-Host "    $output"
        if ($output -notmatch [regex]::Escape($crate.Version)) {
            Write-Host "    expected version $($crate.Version) in the output" -ForegroundColor Red
            return 1
        }
        return 0
    }
}

Complete-CheckRun -Title 'check'
