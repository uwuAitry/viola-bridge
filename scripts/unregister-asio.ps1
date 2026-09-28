# SPDX-License-Identifier: GPL-3.0-or-later
#
# Unregisters viola_asio.dll: the inverse of scripts/register-asio.ps1.
#
# Exactly the two keys register-asio.ps1 creates are removed, which between them
# are all four of the locations docs/asio-driver-notes.md section 2 records from
# the SDK's common/register.cpp:
#
#   HKLM\SOFTWARE\Classes\CLSID\{6C1E7D94-3A52-4B8F-9E27-5D0B4C8A1F63}
#     (default)                       = "viola-bridge ASIO"
#     \InprocServer32 (default)       = the installed DLL path
#     \InprocServer32\ThreadingModel  = "Apartment"
#   HKLM\SOFTWARE\ASIO\viola-bridge ASIO
#     Description                     = "viola-bridge ASIO"
#     CLSID                           = "{6C1E7D94-3A52-4B8F-9E27-5D0B4C8A1F63}"
#
# Nothing else is touched: other drivers under HKLM\SOFTWARE\ASIO are left alone.
# The installed DLL is *not* deleted - unregistering a driver should not silently
# remove files - so the path is printed for the operator to remove by hand.
#
# DRY RUN BY DEFAULT, and a backup is exported before anything is deleted: each
# key that exists is written to a timestamped .reg file under the install
# directory first, so the removal can be undone by double-clicking that file.
# Like register-asio.ps1 this script never self-elevates and stops with a message
# when -Apply is used without elevation.
#
# Usage:
#   pwsh -File scripts/unregister-asio.ps1          # preview (safe)
#   pwsh -File scripts/unregister-asio.ps1 -Apply   # from an elevated shell
[CmdletBinding()]
param(
    [switch]$Apply
)

$ErrorActionPreference = 'Stop'
# Native exit codes are checked explicitly below; do not turn every reg.exe
# message on stderr into a terminating error.
$PSNativeCommandUseErrorActionPreference = $false

# Values frozen by docs/viola-asio-contract.md ("Names").
$Clsid       = '{6C1E7D94-3A52-4B8F-9E27-5D0B4C8A1F63}'
$InstallDir  = 'C:\ProgramData\viola-asio'
$InstalledDll = Join-Path $InstallDir 'viola_asio.dll'
$AsioKeyName = 'viola-bridge ASIO'

$ClsidKey = "HKLM:\SOFTWARE\Classes\CLSID\$Clsid"
$ClsidReg = "HKLM\SOFTWARE\Classes\CLSID\$Clsid"
$AsioKey  = "HKLM:\SOFTWARE\ASIO\$AsioKeyName"
$AsioReg  = "HKLM\SOFTWARE\ASIO\$AsioKeyName"

function Test-Elevated {
    $identity  = [Security.Principal.WindowsIdentity]::GetCurrent()
    $principal = New-Object -TypeName Security.Principal.WindowsPrincipal -ArgumentList $identity
    return $principal.IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)
}

function Export-RegKeyBackup {
    param([string]$RegPath, [string]$File)
    # reg.exe takes the key name and the file as separate argv entries, so the
    # space in "viola-bridge ASIO" needs no manual quoting here (and we do not
    # use Start-Process, which does not quote array elements).
    $output = & reg.exe export $RegPath $File /y 2>&1
    if ($LASTEXITCODE -ne 0) {
        throw ('reg export failed for {0} (exit {1}): {2}' -f $RegPath, $LASTEXITCODE, ($output -join ' '))
    }
    Write-Host ('  exported (backup) {0}' -f $RegPath)
    Write-Host ('                 -> {0}' -f $File)
}

# ---------------------------------------------------------------------------
# Planning: reads only, nothing is written
# ---------------------------------------------------------------------------
$elevated    = Test-Elevated
$clsidExists = Test-Path -LiteralPath $ClsidKey
$asioExists  = Test-Path -LiteralPath $AsioKey

Write-Host 'viola-asio unregistration'
if ($Apply) {
    Write-Host '  mode        : APPLY - this run deletes the keys below from HKLM'
} else {
    Write-Host '  mode        : DRY RUN (no -Apply) - nothing is written anywhere'
}
Write-Host ('  elevated    : {0}' -f $(if ($elevated) { 'yes' } else { 'NO' }))
Write-Host ''

if ($Apply -and -not $elevated) {
    Write-Host 'STOP: not running elevated.'
    Write-Host '      Deleting from HKLM requires an Administrator PowerShell prompt. Re-run'
    Write-Host '      this script from an elevated shell - it will not self-elevate.'
    exit 1
}

if (-not $clsidExists -and -not $asioExists) {
    Write-Host 'Not registered: neither key exists, so there is nothing to remove.'
    exit 0
}

if ($Apply -and -not (Test-Path -LiteralPath $InstallDir)) {
    New-Item -ItemType Directory -Force -Path $InstallDir | Out-Null
}

# ---------------------------------------------------------------------------
# Backups first - nothing is deleted before the .reg file is on disk
# ---------------------------------------------------------------------------
$stamp = Get-Date -Format 'yyyyMMdd-HHmmss'

if ($Apply) {
    if ($clsidExists) {
        Export-RegKeyBackup -RegPath $ClsidReg -File (Join-Path $InstallDir "unregister-backup-$stamp-clsid.reg")
    }
    if ($asioExists) {
        Export-RegKeyBackup -RegPath $AsioReg -File (Join-Path $InstallDir "unregister-backup-$stamp-asio.reg")
    }
} else {
    Write-Host 'Backups (taken before anything is deleted, -Apply only):'
    if ($clsidExists) {
        Write-Host ('  would export  {0}' -f $ClsidReg)
        Write-Host ('             -> {0}\unregister-backup-<timestamp>-clsid.reg' -f $InstallDir)
    }
    if ($asioExists) {
        Write-Host ('  would export  {0}' -f $AsioReg)
        Write-Host ('             -> {0}\unregister-backup-<timestamp>-asio.reg' -f $InstallDir)
    }
    Write-Host ''
}

# ---------------------------------------------------------------------------
# Removal
# ---------------------------------------------------------------------------
Write-Host 'Registry:'

if ($clsidExists) {
    if ($Apply) {
        # -Recurse removes InprocServer32 and its ThreadingModel value with it.
        Remove-Item -LiteralPath $ClsidKey -Recurse -Force
        Write-Host ('  deleted          {0}' -f $ClsidReg)
        Write-Host ('                   (including {0}\InprocServer32 and its ThreadingModel)' -f $ClsidReg)
    } else {
        Write-Host ('  would delete     {0}' -f $ClsidReg)
        Write-Host ('                   (including {0}\InprocServer32 and its ThreadingModel)' -f $ClsidReg)
    }
} else {
    Write-Host ('  not present      {0}' -f $ClsidReg)
}

if ($asioExists) {
    if ($Apply) {
        Remove-Item -LiteralPath $AsioKey -Recurse -Force
        Write-Host ('  deleted          {0}' -f $AsioReg)
        Write-Host '                   (including its Description and CLSID values)'
    } else {
        Write-Host ('  would delete     {0}' -f $AsioReg)
        Write-Host '                   (including its Description and CLSID values)'
    }
} else {
    Write-Host ('  not present      {0}' -f $AsioReg)
}
Write-Host ''

Write-Host 'Installed DLL:'
Write-Host ('  left in place    {0}' -f $InstalledDll)
Write-Host '                   (unregistering never deletes files; remove it by hand if wanted)'
Write-Host ''

if ($Apply) {
    Write-Host 'Applied: the keys above were removed after being backed up to .reg files.'
    Write-Host ('Re-install with scripts/register-asio.ps1 -Apply.')
} else {
    Write-Host 'DRY RUN complete.'
    Write-Host 'No registry key or value was deleted or modified.'
    Write-Host 'Re-run with -Apply from an elevated shell to unregister.'
}
