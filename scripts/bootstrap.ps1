# SPDX-License-Identifier: GPL-3.0-or-later
#
# Fetches the pinned upstream Omniphony revision that provides the `bridge_api`
# crate used by viola_bridge. Nothing from upstream is committed to this repo;
# the checkout lives in third_party/ and is git-ignored.
#
# Usage:  pwsh -File scripts/bootstrap.ps1
[CmdletBinding()]
param(
    [string]$Revision = 'b81f518831e89864a6391744cf9daa9eeadb3c51',
    [string]$Destination = (Join-Path $PSScriptRoot '..\third_party\Omniphony')
)

$ErrorActionPreference = 'Stop'
$env:GIT_TERMINAL_PROMPT = '0'

if (Test-Path (Join-Path $Destination '.git')) {
    Write-Host "upstream checkout already present: $Destination"
} else {
    New-Item -ItemType Directory -Force -Path (Split-Path $Destination) | Out-Null
    Write-Host "cloning mgth/Omniphony into $Destination"
    git clone --filter=blob:none --no-checkout https://github.com/mgth/Omniphony $Destination
    if ($LASTEXITCODE -ne 0) { throw "git clone failed ($LASTEXITCODE)" }
}

Push-Location $Destination
try {
    git fetch --depth 1 origin $Revision
    if ($LASTEXITCODE -ne 0) { throw "git fetch $Revision failed ($LASTEXITCODE)" }
    git checkout --force $Revision
    if ($LASTEXITCODE -ne 0) { throw "git checkout $Revision failed ($LASTEXITCODE)" }
} finally {
    Pop-Location
}

$api = Join-Path $Destination 'omniphony-renderer\bridge_api\Cargo.toml'
if (-not (Test-Path $api)) { throw "bridge_api not found at $api" }
Write-Host "bridge_api ready at $api"
