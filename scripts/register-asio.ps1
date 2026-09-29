# SPDX-License-Identifier: GPL-3.0-or-later
#
# Registers viola_asio.dll as an ASIO driver.
#
# These are the only two registry locations involved, and they are exactly the
# ones docs/asio-driver-notes.md section 2 records from the SDK's
# common/register.cpp, confirmed against the working drivers installed on this
# machine (HKLM\SOFTWARE\ASIO\ASIO4ALL v2 and \Voicemeeter Virtual ASIO both
# carry "Description" and "CLSID", the GUID in braces):
#
#   HKLM\SOFTWARE\Classes\CLSID\{6C1E7D94-3A52-4B8F-9E27-5D0B4C8A1F63}
#     (default)                       = "viola-bridge ASIO"
#     \InprocServer32 (default)       = the full installed DLL path
#     \InprocServer32\ThreadingModel  = "Apartment"          (REG_SZ)
#   HKLM\SOFTWARE\ASIO\viola-bridge ASIO
#     Description                     = "viola-bridge ASIO"  (REG_SZ)
#     CLSID                           = "{6C1E7D94-3A52-4B8F-9E27-5D0B4C8A1F63}"  (REG_SZ)
#
# No other key is invented: a 64-bit-only driver needs no WOW6432Node copy.
# HKLM\SOFTWARE\Classes is the machine hive that HKCR merges, so writing it
# directly is the elevated equivalent of what the SDK sample's DllRegisterServer
# does; the DLL itself stays passive.
#
# DRY RUN BY DEFAULT. Without -Apply this script writes nothing anywhere, creates
# no directory, and only *reads* the registry while printing every action it
# would take. Installing a driver is the operator's decision
# (docs/viola-asio-contract.md, "House rules"); the script never self-elevates
# and stops with a message when -Apply is used without elevation.
#
# Idempotent: values that already match are reported as "already correct" and
# left alone, and a run with nothing to change does not touch the disk at all.
# Before overwriting an existing key, -Apply exports that key to a timestamped
# .reg file under the install directory.
#
# Usage:
#   pwsh -File scripts/register-asio.ps1                    # preview (safe)
#   pwsh -File scripts/register-asio.ps1 -Apply             # from an elevated shell
#   pwsh -File scripts/register-asio.ps1 -DllPath <path> -Apply
[CmdletBinding()]
param(
    # Where the CI artifact lands by default:
    #   gh run download --name viola-asio-windows-x86_64 --dir dist
    [string]$DllPath = (Join-Path $PSScriptRoot '..\dist\viola-asio-windows-x86_64\viola_asio.dll'),
    [switch]$Apply
)

$ErrorActionPreference = 'Stop'
# Native exit codes are checked explicitly below; do not turn every reg.exe
# message on stderr into a terminating error.
$PSNativeCommandUseErrorActionPreference = $false

# Values frozen by docs/viola-asio-contract.md ("Names"). The GUID is written
# here and in crates/viola_asio/src/guid.rs, and is never generated at install
# time: the host passes it back to us as riid, so CLSID and IID must stay
# identical forever.
$Clsid        = '{6C1E7D94-3A52-4B8F-9E27-5D0B4C8A1F63}'
$Description  = 'viola-bridge ASIO'
$InstallDir   = 'C:\ProgramData\viola-asio'
$InstalledDll = Join-Path $InstallDir 'viola_asio.dll'
$AsioKeyName  = 'viola-bridge ASIO'

$ClsidKey  = "HKLM:\SOFTWARE\Classes\CLSID\$Clsid"
$ClsidReg  = "HKLM\SOFTWARE\Classes\CLSID\$Clsid"
$InprocKey = Join-Path $ClsidKey 'InprocServer32'
$InprocReg = "$ClsidReg\InprocServer32"
$AsioKey   = "HKLM:\SOFTWARE\ASIO\$AsioKeyName"
$AsioReg   = "HKLM\SOFTWARE\ASIO\$AsioKeyName"

function Get-RegValue {
    param([string]$KeyPath, [string]$Name)
    if (-not (Test-Path -LiteralPath $KeyPath)) { return $null }
    $item = Get-ItemProperty -LiteralPath $KeyPath -Name $Name -ErrorAction SilentlyContinue
    if ($null -eq $item) { return $null }
    $property = $item.PSObject.Properties[$Name]
    if ($null -eq $property) { return $null }
    return $property.Value
}

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
$sourceDll   = [System.IO.Path]::GetFullPath($DllPath)
$sourceFound = Test-Path -LiteralPath $sourceDll -PathType Leaf
$elevated    = Test-Elevated

$plan = @(
    [pscustomobject]@{ Key = $ClsidKey;  RegPath = $ClsidReg;  Name = '(default)';      Value = $Description }
    [pscustomobject]@{ Key = $InprocKey; RegPath = $InprocReg; Name = '(default)';      Value = $InstalledDll }
    [pscustomobject]@{ Key = $InprocKey; RegPath = $InprocReg; Name = 'ThreadingModel'; Value = 'Apartment' }
    [pscustomobject]@{ Key = $AsioKey;   RegPath = $AsioReg;   Name = 'Description';    Value = $Description }
    [pscustomobject]@{ Key = $AsioKey;   RegPath = $AsioReg;   Name = 'CLSID';          Value = $Clsid }
)
foreach ($entry in $plan) {
    $entry | Add-Member -NotePropertyName Current   -NotePropertyValue (Get-RegValue -KeyPath $entry.Key -Name $entry.Name)
    $entry | Add-Member -NotePropertyName KeyExists -NotePropertyValue (Test-Path -LiteralPath $entry.Key)
    $entry | Add-Member -NotePropertyName Changes   -NotePropertyValue ($entry.Current -ne $entry.Value)
}

$valueChanges = @($plan | Where-Object { $_.Changes }).Count
$clsidTouched = @($plan | Where-Object { $_.Changes -and ($_.Key -eq $ClsidKey -or $_.Key -eq $InprocKey) }).Count -gt 0
$asioTouched  = @($plan | Where-Object { $_.Changes -and $_.Key -eq $AsioKey }).Count -gt 0

$installedFound = Test-Path -LiteralPath $InstalledDll -PathType Leaf
$copyNeeded     = $false
$sourceHash     = $null
if ($sourceFound) {
    $sourceHash = (Get-FileHash -LiteralPath $sourceDll -Algorithm SHA256).Hash
    if ($installedFound) {
        $copyNeeded = $sourceHash -ne (Get-FileHash -LiteralPath $InstalledDll -Algorithm SHA256).Hash
    } else {
        $copyNeeded = $true
    }
}

# ---------------------------------------------------------------------------
# Report
# ---------------------------------------------------------------------------
Write-Host 'viola-asio registration'
if ($Apply) {
    Write-Host '  mode        : APPLY - this run writes to HKLM and to disk'
} else {
    Write-Host '  mode        : DRY RUN (no -Apply) - nothing is written anywhere'
}
Write-Host ('  source DLL  : {0}' -f $sourceDll)
Write-Host ('  installs to : {0}' -f $InstalledDll)
Write-Host ('  elevated    : {0}' -f $(if ($elevated) { 'yes' } else { 'NO' }))
Write-Host ''

if ($Apply -and -not $elevated) {
    Write-Host 'STOP: not running elevated.'
    Write-Host '      Writing HKLM requires an Administrator PowerShell prompt. Re-run this'
    Write-Host '      script from an elevated shell - it will not self-elevate.'
    exit 1
}
if ($Apply -and -not $sourceFound) {
    Write-Host 'STOP: source DLL not found.'
    Write-Host ('      {0}' -f $sourceDll)
    Write-Host '      Download the CI artifact first, then re-run:'
    Write-Host '        gh run download --name viola-asio-windows-x86_64 --dir dist'
    exit 1
}
if ($Apply -and $valueChanges -eq 0 -and -not $copyNeeded) {
    Write-Host 'Nothing to do: every value already matches and the installed DLL is identical.'
    exit 0
}

if ($Apply -and -not (Test-Path -LiteralPath $InstallDir)) {
    New-Item -ItemType Directory -Force -Path $InstallDir | Out-Null
}

# ---------------------------------------------------------------------------
# Backups: -Apply only, and only for keys that already exist and will change
# ---------------------------------------------------------------------------
$stamp        = Get-Date -Format 'yyyyMMdd-HHmmss'
$backupNeeded = ($clsidTouched -and (Test-Path -LiteralPath $ClsidKey)) -or ($asioTouched -and (Test-Path -LiteralPath $AsioKey))

if ($Apply) {
    if ($clsidTouched -and (Test-Path -LiteralPath $ClsidKey)) {
        Export-RegKeyBackup -RegPath $ClsidReg -File (Join-Path $InstallDir "backup-$stamp-clsid.reg")
    }
    if ($asioTouched -and (Test-Path -LiteralPath $AsioKey)) {
        Export-RegKeyBackup -RegPath $AsioReg -File (Join-Path $InstallDir "backup-$stamp-asio.reg")
    }
} else {
    Write-Host 'Backups (-Apply only, and only for keys that already exist):'
    if ($clsidTouched -and (Test-Path -LiteralPath $ClsidKey)) {
        Write-Host ('  would export  {0}' -f $ClsidReg)
        Write-Host ('             -> {0}\backup-<timestamp>-clsid.reg' -f $InstallDir)
    }
    if ($asioTouched -and (Test-Path -LiteralPath $AsioKey)) {
        Write-Host ('  would export  {0}' -f $AsioReg)
        Write-Host ('             -> {0}\backup-<timestamp>-asio.reg' -f $InstallDir)
    }
    if (-not $backupNeeded) {
        Write-Host '  (neither key exists yet, so there is nothing to back up)'
    }
    Write-Host ''
}

# ---------------------------------------------------------------------------
# The DLL file
# ---------------------------------------------------------------------------
Write-Host 'Installed DLL:'
if (-not $sourceFound) {
    Write-Host ('  source missing   {0}' -f $sourceDll)
    if (-not $Apply) {
        Write-Host '                   (the CI artifact has not been downloaded; -Apply would stop here)'
    }
} elseif (-not $copyNeeded) {
    Write-Host ('  already current  {0}' -f $InstalledDll)
} elseif (-not $Apply) {
    Write-Host ('  would copy       {0}' -f $sourceDll)
    Write-Host ('                -> {0}' -f $InstalledDll)
    Write-Host ('  sha256           {0}' -f $sourceHash)
} else {
    Copy-Item -LiteralPath $sourceDll -Destination $InstalledDll -Force
    Write-Host ('  copied           {0}' -f $sourceDll)
    Write-Host ('                -> {0}' -f $InstalledDll)
}
Write-Host ''

# ---------------------------------------------------------------------------
# The registry values
# ---------------------------------------------------------------------------
Write-Host 'Registry:'
$written = 0
$announced = @{}
foreach ($entry in $plan) {
    if (-not $entry.Changes) {
        Write-Host ('  already correct  {0}  [{1}]' -f $entry.RegPath, $entry.Name)
        continue
    }
    if (-not $Apply) {
        if (-not $entry.KeyExists -and -not $announced.ContainsKey($entry.Key)) {
            Write-Host ('  would create     {0}' -f $entry.RegPath)
            $announced[$entry.Key] = $true
        }
        Write-Host ('  would set        {0}  [{1}] = "{2}"' -f $entry.RegPath, $entry.Name, $entry.Value)
        continue
    }
    # `reg.exe add` rather than New-Item + Set-ItemProperty. New-Item -Force on a
    # registry key that already exists *recreates* it and drops the values already
    # written to it, and KeyExists comes from the planning pass, so by the time the
    # second entry for a key ran it was stale: the second New-Item wiped the first
    # entry's value. Net effect was that only the last write per key survived -
    # InprocServer32 lost its default (the DLL path!) and the ASIO key lost its
    # Description. reg.exe add creates the path as needed, touches exactly one
    # value, and reports an exit code we can check.
    $regArgs = @('add', $entry.RegPath, '/f', '/t', 'REG_SZ', '/d', $entry.Value)
    if ($entry.Name -eq '(default)') { $regArgs += '/ve' } else { $regArgs += @('/v', $entry.Name) }
    $output = & reg.exe @regArgs 2>&1
    if ($LASTEXITCODE -ne 0) {
        throw ('reg add failed for {0} [{1}] (exit {2}): {3}' -f $entry.RegPath, $entry.Name, $LASTEXITCODE, ($output -join ' '))
    }
    # Read it back: "the command exited 0" is not evidence that the value is there.
    $back = Get-RegValue -KeyPath $entry.Key -Name $entry.Name
    if ($back -ne $entry.Value) {
        throw ('read-back mismatch for {0} [{1}]: expected "{2}", found "{3}"' -f $entry.RegPath, $entry.Name, $entry.Value, $back)
    }
    $written++
    Write-Host ('  set              {0}  [{1}] = "{2}"' -f $entry.RegPath, $entry.Name, $entry.Value)
}
Write-Host ''

if ($Apply) {
    Write-Host ('Applied: {0} registry value(s) written; DLL {1}.' -f $written, $(if ($copyNeeded) { 'copied' } else { 'already current' }))
    Write-Host 'Uninstall with scripts/unregister-asio.ps1.'
} else {
    Write-Host ('DRY RUN complete: {0} registry value(s) would change.' -f $valueChanges)
    Write-Host 'No registry key, value, file or directory was created or modified.'
    Write-Host 'Re-run with -Apply from an elevated shell to install.'
}
