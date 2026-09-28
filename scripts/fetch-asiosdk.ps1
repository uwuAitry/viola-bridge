# SPDX-License-Identifier: GPL-3.0-or-later
#
# Fetches the Steinberg ASIO SDK that `viola_asio` needs in order to declare the
# ASIO driver interface. Nothing from the SDK is committed to this repository:
# the download lands in `third_party/` (git-ignored) and is only ever read by the
# build, exactly like FlexASIO's own CMake FetchContent does.
#
#   https://github.com/dechamps/ASIOUtil/blob/master/CMakeLists.txt
#
# The URL and SHA1 below are the ones FlexASIO pins, so the SDK revision is the
# one a well-tested ASIO driver is built against.
#
# Usage:  pwsh -File scripts/fetch-asiosdk.ps1
[CmdletBinding()]
param(
    [string]$Url = 'https://download.steinberg.net/sdk_downloads/asiosdk_2.3.3_2019-06-14.zip',
    [string]$Sha1 = '4d0097725bcf1015c91fb84f89e1a888141bd131',
    [string]$Destination = (Join-Path $PSScriptRoot '..\third_party\asiosdk')
)

$ErrorActionPreference = 'Stop'

$common = Join-Path $Destination 'common\iasiodrv.h'
if (Test-Path $common) {
    Write-Host "ASIO SDK already present: $Destination"
    exit 0
}

$zip = Join-Path $env:TEMP 'asiosdk.zip'
Write-Host "downloading $Url"
Invoke-WebRequest -Uri $Url -OutFile $zip -UseBasicParsing

$actual = (Get-FileHash -Path $zip -Algorithm SHA1).Hash.ToLower()
if ($actual -ne $Sha1.ToLower()) {
    Remove-Item $zip -Force -ErrorAction SilentlyContinue
    throw "ASIO SDK checksum mismatch: expected $Sha1, got $actual"
}
Write-Host "checksum ok ($actual)"

New-Item -ItemType Directory -Force -Path $Destination | Out-Null
if (Test-Path (Join-Path $Destination 'common')) {
    Remove-Item (Join-Path $Destination 'common') -Recurse -Force
    Remove-Item (Join-Path $Destination 'host') -Recurse -Force -ErrorAction SilentlyContinue
}
Expand-Archive -Path $zip -DestinationPath $Destination -Force

# The archive is not rooted at a single directory on every release; if the
# expected layout is one level down, flatten it.
if (-not (Test-Path $common)) {
    $inner = Get-ChildItem $Destination -Directory | Where-Object { Test-Path (Join-Path $_.FullName 'common\iasiodrv.h') } | Select-Object -First 1
    if ($inner) {
        Get-ChildItem $inner.FullName | Move-Item -Destination $Destination -Force
        Remove-Item $inner.FullName -Recurse -Force
    }
}
if (-not (Test-Path $common)) { throw "ASIO SDK layout unexpected: $common missing" }

Remove-Item $zip -Force -ErrorAction SilentlyContinue
Write-Host "ASIO SDK ready: $common"
