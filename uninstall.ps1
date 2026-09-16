#Requires -Version 5.1
<#
.SYNOPSIS
    Stop and remove the PPPoE auto dialer service.

.DESCRIPTION
    Lives next to pppoe.exe, in the folder the service was installed from.

    The script:
      1. re-launches itself with administrator rights if needed (one UAC prompt);
      2. checks that pppoe.exe and pppoe.toml are still there;
      3. stops the service if it is running and removes its registration.

    The broadband connection is left online on purpose: it belongs to the Windows
    dial-up manager, not to this service. The script prints how to drop it and
    what remains in the folder.

.PARAMETER ExePath
    pppoe.exe that was installed. Default: the copy next to this script.

.PARAMETER Config
    pppoe.toml that the service uses. Default: the copy next to this script.

.PARAMETER DryRun
    Print what would happen without changing anything. No UAC prompt.

.EXAMPLE
    .\uninstall.ps1

    Remove the service. The connection stays online.
#>
[CmdletBinding()]
param(
    [string]$ExePath,
    [string]$Config,
    [switch]$DryRun
)

$ErrorActionPreference = 'Stop'

# The name compiled into pppoe.exe; `[service] name` in pppoe.toml must match it.
$ServiceName = 'PppoeDialer'

function Write-Step { param([string]$Text) Write-Host ''; Write-Host "==> $Text" -ForegroundColor Cyan }
function Write-Ok { param([string]$Text) Write-Host "    OK      $Text" -ForegroundColor Green }
function Write-Warn { param([string]$Text) Write-Host "    WARN    $Text" -ForegroundColor Yellow }
function Write-Fail { param([string]$Text) Write-Host "    FAILED  $Text" -ForegroundColor Red }

function Test-Administrator {
    $identity = [Security.Principal.WindowsIdentity]::GetCurrent()
    $principal = New-Object Security.Principal.WindowsPrincipal($identity)
    return $principal.IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)
}

function Invoke-Elevated {
    # Re-run this script through UAC, forwarding the parameters that were used.
    $hostExe = (Get-Process -Id $PID).Path
    $arguments = @('-NoProfile', '-ExecutionPolicy', 'Bypass', '-File', "`"$PSCommandPath`"")
    foreach ($entry in $PSBoundParameters.GetEnumerator()) {
        if ($entry.Value -is [switch]) {
            if ($entry.Value.IsPresent) { $arguments += "-$($entry.Key)" }
        } else {
            $arguments += "-$($entry.Key)"
            $arguments += "`"$($entry.Value)`""
        }
    }

    Write-Host 'Administrator rights are required - please confirm the UAC prompt.' -ForegroundColor Yellow
    $process = Start-Process -FilePath $hostExe -ArgumentList $arguments -Verb RunAs -PassThru -Wait
    if ($null -eq $process) { exit 1 }
    exit $process.ExitCode
}

# -----------------------------------------------------------------------------
# paths
# -----------------------------------------------------------------------------
if (-not $ExePath) { $ExePath = Join-Path $PSScriptRoot 'pppoe.exe' }
if (-not $Config) { $Config = Join-Path $PSScriptRoot 'pppoe.toml' }
$ExePath = [System.IO.Path]::GetFullPath($ExePath)
$Config = [System.IO.Path]::GetFullPath($Config)

Write-Host ''
Write-Host 'PPPoE auto dialer - uninstall' -ForegroundColor White
Write-Host "  exe    : $ExePath"
Write-Host "  config : $Config"
if ($DryRun) { Write-Host '  mode   : dry run - nothing will be changed' -ForegroundColor DarkGray }

# -----------------------------------------------------------------------------
# 1. administrator rights
# -----------------------------------------------------------------------------
if (-not (Test-Administrator)) {
    if ($DryRun) {
        Write-Warn 'not elevated, which is fine for -DryRun'
    } else {
        Invoke-Elevated
    }
}

# -----------------------------------------------------------------------------
# 2. the files
# -----------------------------------------------------------------------------
Write-Step 'checking the files'
if (-not (Test-Path -LiteralPath $ExePath)) {
    Write-Fail "pppoe.exe was not found at $ExePath"
    Write-Host '    Removing the service needs the same binary; pass -ExePath <path>.' -ForegroundColor DarkGray
    Write-Host "    Or remove it by hand: sc.exe delete $ServiceName" -ForegroundColor DarkGray
    exit 1
}
Write-Ok 'pppoe.exe'

if (-not (Test-Path -LiteralPath $Config)) {
    Write-Fail "the configuration was not found at $Config"
    Write-Host '    Passing pppoe.toml is required by pppoe.exe uninstall; use -Config <path>.' -ForegroundColor DarkGray
    exit 1
}
Write-Ok 'pppoe.toml'

# -----------------------------------------------------------------------------
# 3. is the service registered?
# -----------------------------------------------------------------------------
Write-Step 'checking whether the service is registered'
& sc.exe query $ServiceName > $null 2>&1
if ($LASTEXITCODE -ne 0) {
    Write-Warn "the service $ServiceName is not registered - nothing to remove"
    Write-Host '    The files in this folder are untouched.' -ForegroundColor DarkGray
    exit 0
}
Write-Ok 'registered'

# -----------------------------------------------------------------------------
# 4. remove it
# -----------------------------------------------------------------------------
Write-Step 'removing the service'
if ($DryRun) {
    Write-Host "    would run: `"$ExePath`" uninstall" -ForegroundColor DarkGray
    Write-Host '    (it asks the service to stop first, then deletes the registration)' -ForegroundColor DarkGray
    Write-Host '    the broadband connection would stay online' -ForegroundColor DarkGray
} else {
    & $ExePath uninstall
    if ($LASTEXITCODE -ne 0) {
        Write-Fail "pppoe.exe uninstall exited with $LASTEXITCODE"
        exit $LASTEXITCODE
    }
    Write-Ok 'removed'
}

# -----------------------------------------------------------------------------
# summary
# -----------------------------------------------------------------------------
Write-Host ''
Write-Host ('-' * 72)
if ($DryRun) {
    Write-Host 'dry run finished - nothing was changed' -ForegroundColor Green
} else {
    Write-Host 'done' -ForegroundColor Green
}
Write-Host ''
Write-Host 'the broadband connection is still online, on purpose:'
Write-Host '  it is kept by the Windows dial-up manager, so it survives the service.'
Write-Host '  to drop it right now (use your [dial] entry_name):'
Write-Host '      rasdial "Dr.COM" /disconnect'
Write-Host ''
Write-Host 'still in this folder - delete it when you no longer need it:'
Write-Host '  pppoe.exe, pppoe.toml, README.md, logs\'
Write-Host ''
Write-Host 'to install it again: .\install.ps1'
exit 0
