#Requires -Version 5.1
<#
.SYNOPSIS
    Activate (or deactivate) this repository's git hooks.

.DESCRIPTION
    Points `core.hooksPath` at `.githooks` **for this repository only**, so git
    runs `.githooks/pre-commit` - the same gate as CI, in fast mode - before
    every commit.

    Nothing global is touched: your user-level git configuration, identity and
    other repositories are left alone. A commit can still be forced through with
    `git commit --no-verify`.

.PARAMETER Uninstall
    Remove the setting again, restoring git's default `.git/hooks` lookup.

.EXAMPLE
    pwsh -File scripts/install-hooks.ps1
    pwsh -File scripts/install-hooks.ps1 -Uninstall
#>
[CmdletBinding()]
param([switch]$Uninstall)

$ErrorActionPreference = 'Continue'
. "$PSScriptRoot/common.ps1"

Set-Location -LiteralPath $RepoRoot

if (-not (Get-Command git -ErrorAction SilentlyContinue)) {
    Write-Host 'git was not found on PATH.' -ForegroundColor Red
    exit 1
}

if ($Uninstall) {
    git config --local --unset core.hooksPath 2>$null
    $code = $LASTEXITCODE
    if ($code -ne 0) {
        Write-Host 'core.hooksPath was not set for this repository; nothing to do.'
        exit 0
    }
    Write-Host 'hooks disabled (core.hooksPath removed from .git/config)' -ForegroundColor Green
    exit 0
}

$hook = Join-Path $RepoRoot '.githooks/pre-commit'
if (-not (Test-Path -LiteralPath $hook)) {
    Write-Host "missing hook script: $hook" -ForegroundColor Red
    exit 1
}

git config --local core.hooksPath .githooks
if ($LASTEXITCODE -ne 0) {
    Write-Host 'could not set core.hooksPath' -ForegroundColor Red
    exit 1
}

$configured = (git config --local --get core.hooksPath | Out-String).Trim()
Write-Host "hooks enabled: core.hooksPath = $configured" -ForegroundColor Green
Write-Host ''
Write-Host 'What happens now: every commit runs'
Write-Host '    scripts/check.ps1 -Fast'
Write-Host 'which checks formatting, clippy (-D warnings, so unwrap/expect/panic are'
Write-Host 'rejected) and the unit tests. Expect a few seconds when nothing changed.'
Write-Host ''
Write-Host 'Run the full gate - including the release build - with:'
Write-Host '    pwsh -File scripts/check.ps1'
Write-Host 'Force a commit through with:  git commit --no-verify'
