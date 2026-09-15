#Requires -Version 5.1
<#
.SYNOPSIS
    Build the release archive that release.yml publishes.

.DESCRIPTION
    Produces, under dist/:

        pppoe-<version>-<host triple>.zip
            pppoe-<version>-<host triple>/
                pppoe.exe
                pppoe.toml          (copy of pppoe.toml.example, ready to edit)
                README.md
        SHA256SUMS.txt          (sha256sum compatible)

    The archive carries only what a user needs to run the service: the binary,
    its configuration file and the user documentation.

    The archive wraps everything in a folder, so extracting it does not scatter
    files into the download directory. Packaging always builds with --locked: a
    release must come from exactly the dependency set recorded in Cargo.lock.

.PARAMETER Version
    Override the version taken from Cargo.toml (used for the archive file name
    only; the binary always reports the version it was compiled with).

.PARAMETER SkipBuild
    Package the binary already present in target/release. CI uses this after the
    validation step has built it.

.PARAMETER ToolchainBin
    Directory containing cargo.exe/rustc.exe, for environments where the rustup
    shims on PATH cannot be executed. Also settable via $env:PPPOE_TOOLCHAIN_BIN.

.EXAMPLE
    pwsh -File scripts/package.ps1
#>
[CmdletBinding()]
param(
    [string]$Version,
    [switch]$SkipBuild,
    [string]$ToolchainBin
)

$ErrorActionPreference = 'Continue'
. "$PSScriptRoot/common.ps1"

Set-Location -LiteralPath $RepoRoot

$crate = Get-CrateMetadata -Manifest (Join-Path $RepoRoot 'Cargo.toml')
if (-not $Version) { $Version = $crate.Version }

try {
    $toolchain = Resolve-Cargo -ToolchainBin $ToolchainBin
} catch {
    Write-Host ''
    Write-Host $_.Exception.Message -ForegroundColor Red
    exit 1
}
Initialize-RustEnvironment -Toolchain $toolchain

# Take the target triple from the compiler rather than hard coding it, so the
# archive name always describes what is inside.
$rustc = 'rustc'
if ($toolchain.UseExplicitPath) { $rustc = Join-Path $toolchain.BinDir 'rustc.exe' }
$hostTriple = 'x86_64-pc-windows-msvc'
if (((& $rustc -vV 2>&1) | Out-String) -match '(?m)^host:\s*(\S+)') {
    $hostTriple = $Matches[1]
}

$packageName = "$($crate.Name)-$Version-$hostTriple"
$dist = Join-Path $RepoRoot 'dist'
$stageRoot = Join-Path $dist '.stage'
$stage = Join-Path $stageRoot $packageName
$zipPath = Join-Path $dist "$packageName.zip"

Write-Host "packaging $packageName" -ForegroundColor White
Write-ToolchainBanner -Toolchain $toolchain

# --- 1. build ----------------------------------------------------------------
if ($SkipBuild) {
    Write-Host ''
    Write-Host '==> build skipped (-SkipBuild)' -ForegroundColor DarkGray
} else {
    Write-Host ''
    Write-Host '==> cargo build --release --locked' -ForegroundColor Cyan
    & $toolchain.Cargo build --locked --release | Out-Host
    if ($LASTEXITCODE -ne 0) {
        Write-Host "    FAILED (exit $LASTEXITCODE)" -ForegroundColor Red
        exit 1
    }
    Write-Host '    OK' -ForegroundColor Green
}

$exe = Join-Path $RepoRoot 'target\release\pppoe.exe'
if (-not (Test-Path -LiteralPath $exe)) {
    Write-Host ''
    Write-Host 'target\release\pppoe.exe is missing - run without -SkipBuild.' -ForegroundColor Red
    exit 1
}

# --- 2. stage ----------------------------------------------------------------
if (Test-Path -LiteralPath $stageRoot) {
    Remove-Item -LiteralPath $stageRoot -Recurse -Force
}
New-Item -ItemType Directory -Path $stage -Force | Out-Null

$contents = @(
    [pscustomobject]@{ Source = $exe; Name = 'pppoe.exe' }
    # Shipped under its real name, so there is no copy step before first use. The
    # repository keeps the template as pppoe.toml.example because pppoe.toml
    # itself is gitignored (it would hold a real password).
    [pscustomobject]@{ Source = (Join-Path $RepoRoot 'pppoe.toml.example'); Name = 'pppoe.toml' }
    [pscustomobject]@{ Source = (Join-Path $RepoRoot 'README.md'); Name = 'README.md' }
    # NOTE: the GPL asks for the licence text to travel with the binary. It is
    # left out of the archive on purpose - the README, the release page and the
    # repository all point at it. Add a LICENSE entry here to ship it anyway.
)

Write-Host ''
Write-Host 'staging:' -ForegroundColor Cyan
foreach ($item in $contents) {
    if (-not (Test-Path -LiteralPath $item.Source)) {
        Write-Host "    missing $($item.Source)" -ForegroundColor Red
        exit 1
    }
    $target = Join-Path $stage $item.Name
    Copy-Item -LiteralPath $item.Source -Destination $target -Force
    $size = (Get-Item -LiteralPath $target).Length
    Write-Host ("    {0,-24} {1,10:N0} B" -f $item.Name, $size)
}

# The packaged binary must at least start and report its version; a broken
# artifact should never reach the release page.
$stagedExe = Join-Path $stage 'pppoe.exe'
$reported = (& $stagedExe --version 2>&1 | Out-String).Trim()
Write-Host ''
Write-Host "smoke test: $reported"
if ($reported -notmatch [regex]::Escape($Version)) {
    Write-Host "the packaged binary does not report version $Version" -ForegroundColor Red
    exit 1
}

# --- 3. zip ------------------------------------------------------------------
Add-Type -AssemblyName System.IO.Compression.FileSystem
if (Test-Path -LiteralPath $zipPath) { Remove-Item -LiteralPath $zipPath -Force }
# Zipping the stage ROOT (which contains the package folder) is what gives the
# archive its wrapping directory.
[System.IO.Compression.ZipFile]::CreateFromDirectory(
    $stageRoot,
    $zipPath,
    [System.IO.Compression.CompressionLevel]::Optimal,
    $false
)
Remove-Item -LiteralPath $stageRoot -Recurse -Force

# --- 4. checksums ------------------------------------------------------------
$hash = (Get-FileHash -LiteralPath $zipPath -Algorithm SHA256).Hash.ToLowerInvariant()
$sumsPath = Join-Path $dist 'SHA256SUMS.txt'
"$hash  $([System.IO.Path]::GetFileName($zipPath))" |
    Set-Content -LiteralPath $sumsPath -Encoding ASCII

# --- 5. summary --------------------------------------------------------------
$zipSize = (Get-Item -LiteralPath $zipPath).Length
Write-Host ''
Write-Host ('-' * 72)
Write-Host "archive : $zipPath" -ForegroundColor Green
Write-Host ("size    : {0:N0} bytes" -f $zipSize)
Write-Host "sha256  : $hash"
Write-Host "sums    : $sumsPath"
exit 0
