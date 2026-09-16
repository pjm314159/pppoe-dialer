#Requires -Version 5.1
<#
.SYNOPSIS
    Install and start the PPPoE auto dialer service.

.DESCRIPTION
    Lives next to pppoe.exe, in the folder extracted from the release archive.

    The script:
      1. re-launches itself with administrator rights if needed (one UAC prompt);
      2. checks that pppoe.exe and pppoe.toml are there;
      3. checks that the broadband account name and the password are present, and
         prints exactly what to write when they are not;
      4. registers the service and starts it.

    Nothing on the system is touched when -DryRun is used.

.PARAMETER ExePath
    pppoe.exe to install. Default: the copy next to this script.

.PARAMETER Config
    pppoe.toml to use. Default: the copy next to this script.

.PARAMETER DryRun
    Run the checks and print the commands that would be executed, then exit.
    No UAC prompt, nothing is installed.

.PARAMETER NoStart
    Register the service but leave it stopped.

.PARAMETER SkipConfigCheck
    Install even though the account name or the password is missing.

.PARAMETER LockConfig
    Restrict pppoe.toml to Administrators and SYSTEM with icacls. Off by default,
    because editing the file afterwards then needs an elevated editor.

.EXAMPLE
    .\install.ps1

    Install and start the service.

.EXAMPLE
    .\install.ps1 -DryRun

    Show what would happen without changing anything.

.EXAMPLE
    .\install.ps1 -LockConfig

    Install and restrict the permissions of pppoe.toml.
#>
[CmdletBinding()]
param(
    [string]$ExePath,
    [string]$Config,
    [switch]$DryRun,
    [switch]$NoStart,
    [switch]$SkipConfigCheck,
    [switch]$LockConfig
)

$ErrorActionPreference = 'Stop'

# Values that mean "the account data has not been written yet".
# `12345678` is deliberately NOT in this list: it is the sample value in the
# shipped pppoe.toml and it is a perfectly plausible account name, so treating it
# as "not filled in" would block a legitimate setup.
$Placeholders = @('', 'changeme', 'change_me', 'your-account', 'your-password')

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

function Read-ConfigValue {
    # Returns the value of the first `key = "..."` line, or $null.
    param([string]$Text, [string]$Key)

    $pattern = '(?m)^\s*' + $Key + '\s*=\s*["'']([^"'']*)["'']'
    $match = [regex]::Match($Text, $pattern)
    if ($match.Success) { return $match.Groups[1].Value.Trim() }
    return $null
}

# -----------------------------------------------------------------------------
# paths
# -----------------------------------------------------------------------------
if (-not $ExePath) { $ExePath = Join-Path $PSScriptRoot 'pppoe.exe' }
if (-not $Config) { $Config = Join-Path $PSScriptRoot 'pppoe.toml' }
$ExePath = [System.IO.Path]::GetFullPath($ExePath)
$Config = [System.IO.Path]::GetFullPath($Config)

Write-Host ''
Write-Host 'PPPoE auto dialer - install' -ForegroundColor White
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
    Write-Host '    Extract the release archive first, or pass -ExePath <path>.' -ForegroundColor DarkGray
    exit 1
}
Write-Ok 'pppoe.exe'

if (-not (Test-Path -LiteralPath $Config)) {
    Write-Fail "the configuration was not found at $Config"
    Write-Host '    The release archive ships one, or pass -Config <path>.' -ForegroundColor DarkGray
    exit 1
}
Write-Ok 'pppoe.toml'

# -----------------------------------------------------------------------------
# 3. the broadband account name and password
# -----------------------------------------------------------------------------
Write-Step 'checking the broadband account name and password'
$text = Get-Content -LiteralPath $Config -Raw -Encoding UTF8
# A file saved as UTF-16 (Notepad offers that) would decode to NUL separated text.
if ($text -match "`0") { $text = Get-Content -LiteralPath $Config -Raw -Encoding Unicode }

$account = Read-ConfigValue -Text $text -Key 'username'
$secret = Read-ConfigValue -Text $text -Key 'password'

$problems = New-Object System.Collections.Generic.List[string]
if ($null -eq $account -or $Placeholders -contains $account) {
    $problems.Add('username is not set')
}
if ($null -eq $secret -or $Placeholders -contains $secret) {
    $problems.Add('password is not set')
}

if ($problems.Count -gt 0) {
    Write-Host ''
    Write-Host '    The service cannot dial without your broadband account.' -ForegroundColor Yellow
    Write-Host '    Open the configuration file and fill in these two values:' -ForegroundColor Yellow
    Write-Host ''
    Write-Host '        [dial]'
    Write-Host '        entry_name = "Dr.COM"                    # any name you recognise'
    Write-Host '        username   = "<your broadband account>"'
    Write-Host '        password   = "<your broadband password>"'
    Write-Host ''
    Write-Host "        notepad `"$Config`"" -ForegroundColor DarkGray
    Write-Host ''
    foreach ($problem in $problems) { Write-Host "    - $problem" -ForegroundColor Yellow }
    Write-Host ''
    if ($SkipConfigCheck) {
        Write-Warn 'continuing anyway because -SkipConfigCheck was given'
    } else {
        Write-Fail 'the account name and the password have to be filled in first'
        Write-Host '    (re-run with -SkipConfigCheck to install regardless)' -ForegroundColor DarkGray
        exit 1
    }
} else {
    Write-Ok 'both are set (their values are never printed)'
}

# -----------------------------------------------------------------------------
# 4. the service
# -----------------------------------------------------------------------------
Write-Step 'registering the service'
if ($DryRun) {
    Write-Host "    would run: `"$ExePath`" install" -ForegroundColor DarkGray
} else {
    & $ExePath install
    if ($LASTEXITCODE -ne 0) {
        Write-Fail "pppoe.exe install exited with $LASTEXITCODE"
        exit $LASTEXITCODE
    }
    Write-Ok 'registered for automatic start, with restart after a crash'
}

Write-Step 'starting the service'
if ($NoStart) {
    Write-Warn 'skipped because of -NoStart'
} elseif ($DryRun) {
    Write-Host "    would run: `"$ExePath`" start" -ForegroundColor DarkGray
} else {
    & $ExePath start
    if ($LASTEXITCODE -ne 0) {
        Write-Fail "pppoe.exe start exited with $LASTEXITCODE"
        exit $LASTEXITCODE
    }
    Write-Ok 'started'
}

# -----------------------------------------------------------------------------
# 5. optional: restrict the configuration file
# -----------------------------------------------------------------------------
if ($LockConfig) {
    Write-Step 'restricting the configuration file'
    if ($DryRun) {
        Write-Host "    would run: icacls `"$Config`" /inheritance:r /grant:r ..." -ForegroundColor DarkGray
    } else {
        & icacls.exe $Config /inheritance:r /grant:r 'BUILTIN\Administrators:F' 'NT AUTHORITY\SYSTEM:F' | Out-Null
        if ($LASTEXITCODE -ne 0) {
            Write-Fail "icacls exited with $LASTEXITCODE"
            exit $LASTEXITCODE
        }
        Write-Ok 'only Administrators and SYSTEM can read it'
    }
} else {
    Write-Host ''
    Write-Host '    tip: pppoe.toml holds the password in clear text. To restrict it:' -ForegroundColor DarkGray
    Write-Host "         icacls `"$Config`" /inheritance:r /grant:r `"BUILTIN\Administrators:F`" `"NT AUTHORITY\SYSTEM:F`"" -ForegroundColor DarkGray
    Write-Host '         (or re-run with -LockConfig; undo with: icacls "<file>" /reset)' -ForegroundColor DarkGray
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
Write-Host 'remember: [dial] username and password in'
Write-Host "  $Config"
Write-Host '          have to be your real broadband account. The log never prints them.'
Write-Host 'useful commands:'
Write-Host '  status    : sc.exe query PppoeDialer'
Write-Host "  logs      : Get-Content `"$(Join-Path (Split-Path -Parent $ExePath) 'logs')\pppoe.log.*`" -Encoding UTF8 -Tail 50"
Write-Host '  stop      : sc.exe stop PppoeDialer        (keeps the connection online)'
Write-Host '  uninstall : .\uninstall.ps1'
exit 0
