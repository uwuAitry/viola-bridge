# SPDX-License-Identifier: GPL-3.0-or-later
#
# Records everything scripts/register-asio.ps1 could possibly change, so that
# "unregistering puts the machine back" is something you can *check* rather than
# believe. No elevation needed: it only reads.
#
#   pwsh -File scripts/asio-state-snapshot.ps1 -Name baseline
#     -> state-snapshots/baseline/  (the "before" picture, take this FIRST)
#
#   pwsh -File scripts/asio-state-snapshot.ps1 -Name after -CompareTo state-snapshots/baseline
#     -> re-captures, then diffs against the baseline and says IDENTICAL or lists
#        exactly what differs.
#
# What is captured, and why that is the whole surface:
#   * HKLM\SOFTWARE\ASIO                     - the driver entry (and proof the other
#                                              drivers, ASIO4ALL / Voicemeeter, were
#                                              left untouched)
#   * HKLM\SOFTWARE\Classes\CLSID\{<ours>}   - the COM registration
#   * C:\ProgramData\viola-asio              - the install directory and the DLL
# Nothing else is written by the register script: no HKCU, no WOW6432Node copy, no
# service, no kernel driver, no file anywhere else.
[CmdletBinding()]
param(
    [string]$Root = (Join-Path $PSScriptRoot '..\state-snapshots'),
    [string]$Name = 'baseline',
    [string]$CompareTo,
    [string]$Clsid = '{6C1E7D94-3A52-4B8F-9E27-5D0B4C8A1F63}',
    [string]$InstallDir = 'C:\ProgramData\viola-asio'
)

$ErrorActionPreference = 'Stop'
# reg.exe writes its diagnostics to stderr on a missing key; that is data here,
# not a terminating error.
$PSNativeCommandUseErrorActionPreference = $false

$AsioKey  = 'HKLM:\SOFTWARE\ASIO'   # PowerShell drive form, for Test-Path
$AsioReg  = 'HKLM\SOFTWARE\ASIO'   # reg.exe form, for reg export
$ClsidReg = "HKLM\SOFTWARE\Classes\CLSID\$Clsid"

function Save-Snapshot {
    param([string]$Directory)

    New-Item -ItemType Directory -Force -Path $Directory | Out-Null

    # --- registry: the ASIO driver list -------------------------------------
    $asioFile = Join-Path $Directory 'hkkm-software-asio.reg'
    if (Test-Path -LiteralPath $AsioKey) {
        & reg.exe export $AsioReg $asioFile /y | Out-Null
        if ($LASTEXITCODE -ne 0) { throw "reg export failed for $AsioReg" }
    } else {
        Set-Content -Path $asioFile -Value '; the key does not exist'
    }

    # --- registry: our COM registration (or its absence) --------------------
    $clsidFile = Join-Path $Directory 'hkcr-clsid-viola-asio.reg'
    if (Test-Path -LiteralPath "HKLM:\SOFTWARE\Classes\CLSID\$Clsid") {
        & reg.exe export $ClsidReg $clsidFile /y | Out-Null
        if ($LASTEXITCODE -ne 0) { throw "reg export failed for $ClsidReg" }
    } else {
        Set-Content -Path $clsidFile -Value '; the key does not exist'
    }

    # --- the install directory ---------------------------------------------
    $report = @()
    if (Test-Path -LiteralPath $InstallDir) {
        $report += "exists: $InstallDir"
        foreach ($item in Get-ChildItem -LiteralPath $InstallDir -Recurse -File) {
            $hash = (Get-FileHash -LiteralPath $item.FullName -Algorithm SHA256).Hash.ToLower()
            $rel = $item.FullName.Substring($InstallDir.Length).TrimStart('\')
            $report += ('{0}  {1}  {2} bytes' -f $hash, $rel, $item.Length)
        }
        $report = $report | Sort-Object
    } else {
        $report += "absent: $InstallDir"
    }
    Set-Content -Path (Join-Path $Directory 'install-dir.txt') -Value $report
}

function Compare-Snapshot {
    param([string]$Baseline, [string]$Current)

    $files = Get-ChildItem -LiteralPath $Baseline -File | Select-Object -ExpandProperty Name
    $diffs = @()
    foreach ($file in $files) {
        $a = Join-Path $Baseline $file
        $b = Join-Path $Current  $file
        if (-not (Test-Path -LiteralPath $b)) { $diffs += "$file : missing from the new snapshot"; continue }
        $ha = (Get-FileHash -LiteralPath $a -Algorithm SHA256).Hash
        $hb = (Get-FileHash -LiteralPath $b -Algorithm SHA256).Hash
        if ($ha -ne $hb) {
            $diffs += $file
            Compare-Object (Get-Content -LiteralPath $a) (Get-Content -LiteralPath $b) |
                ForEach-Object { $diffs += ('    {0} {1}' -f $_.SideIndicator, $_.InputObject) }
        }
    }
    # A snapshot may also have *gained* a file (e.g. the CLSID key now exists
    # while the baseline recorded its absence) - that is a difference too.
    foreach ($file in Get-ChildItem -LiteralPath $Current -File | Select-Object -ExpandProperty Name) {
        if ($file -notin $files) { $diffs += "$file : new since the baseline" }
    }
    return $diffs
}

$target = Join-Path $Root $Name
Save-Snapshot -Directory $target
Write-Host ("snapshot  : {0}" -f (Resolve-Path $target))
foreach ($file in Get-ChildItem -LiteralPath $target -File) {
    Write-Host ('  {0,-32} {1} bytes' -f $file.Name, $file.Length)
}

if ($CompareTo) {
    if (-not (Test-Path -LiteralPath $CompareTo)) { throw "no baseline at $CompareTo" }
    $temp = Join-Path $Root ('.compare-' + [guid]::NewGuid().ToString('N').Substring(0, 8))
    Save-Snapshot -Directory $temp
    Write-Host ''
    $diffs = Compare-Snapshot -Baseline $CompareTo -Current $temp
    if ($diffs.Count -eq 0) {
        Write-Host ("COMPARISON: IDENTICAL to {0}" -f (Resolve-Path $CompareTo))
        Write-Host '  the ASIO key, our COM registration and the install directory are all unchanged.'
    } else {
        Write-Host ("COMPARISON: DIFFERENT from {0}" -f (Resolve-Path $CompareTo))
        $diffs | ForEach-Object { Write-Host ("  {0}" -f $_) }
    }
    Remove-Item -LiteralPath $temp -Recurse -Force
}
