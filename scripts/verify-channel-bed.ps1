# SPDX-License-Identifier: GPL-3.0-or-later
#
# Local acceptance probe for viola_bridge: renders a known multichannel WAV
# through the plugin with the installed `orender`, then reports per-channel
# levels of the render dump.
#
# The engine's config is only ever READ: this script passes no --config and
# never uses --save-config, so %ProgramData%\omniphony\config.yaml is untouched.
#
# Known upstream behaviour: with `--output-backend file` the engine decodes the
# whole stream ("Processing complete") but does not terminate, because a loaded
# bridge makes it run as a live input manager. This script therefore waits for
# the dump to stop growing and then kills the process; `-TimeoutSeconds` bounds
# that wait.
#
# Usage:
#   pwsh -File scripts/verify-channel-bed.ps1
#   pwsh -File scripts/verify-channel-bed.ps1 -BridgePath path\to\viola_bridge.dll -Channels 16
[CmdletBinding()]
param(
    [string]$Orender = "$env:LOCALAPPDATA\Programs\Omniphony Studio\orender.exe",
    [string]$Layout = "$env:LOCALAPPDATA\Programs\Omniphony Studio\layouts\9.1.6.yaml",
    [string]$BridgePath = (Join-Path $PSScriptRoot '..\dist\viola_bridge.dll'),
    [string]$ProbeDir = (Join-Path $PSScriptRoot '..\probe'),
    [string]$ProbeWav,
    # Channel count of the *probe source* (what the bridge decodes).
    [int]$Channels = 16,
    [int]$SampleRate = 48000,
    [int]$TimeoutSeconds = 45,
    [int]$QuietSeconds = 5
)

$ErrorActionPreference = 'Stop'

foreach ($required in @($Orender, $Layout)) {
    if (-not (Test-Path $required)) { throw "missing: $required" }
}
if (-not (Test-Path $BridgePath)) {
    throw "missing bridge plugin: $BridgePath (download the CI artifact first)"
}
New-Item -ItemType Directory -Force -Path $ProbeDir | Out-Null

if (-not $ProbeWav) { $ProbeWav = Join-Path $ProbeDir "tone${Channels}_all.wav" }
if (-not (Test-Path $ProbeWav)) {
    $layoutName = if ($Channels -eq 16) { '9.1.6' } else { "$($Channels)c" }
    Write-Host "generating probe source: $ProbeWav"
    & ffmpeg -y -hide_banner -loglevel error -f lavfi `
        -i "sine=frequency=1000:sample_rate=$SampleRate:duration=1" `
        -af "aformat=channel_layouts=$layoutName" -c:a pcm_f32le $ProbeWav
    if ($LASTEXITCODE -ne 0) { throw "ffmpeg could not generate the probe WAV" }
}

$outFile = Join-Path $ProbeDir 'render_out.f32'
$logFile = Join-Path $ProbeDir 'render_out.log'
$stdoutFile = Join-Path $ProbeDir 'render_stdout.txt'
Remove-Item $outFile, $logFile, $stdoutFile -ErrorAction SilentlyContinue

# WAV mode: the probe is RIFF, and we want a non-RIFF stream to fail loudly.
$env:VIOLA_BRIDGE_MODE = 'wav'
$env:VIOLA_BRIDGE_CHANNELS = "$Channels"
$env:VIOLA_BRIDGE_RATE = "$SampleRate"

# Start-Process does NOT quote array elements, and the layout path lives under
# "…\Omniphony Studio\". Build one quoted command line instead.
$argLine = @(
    'render', "`"$ProbeWav`"",
    '--bridge-path', "`"$BridgePath`"",
    '--enable-vbap',
    '--speaker-layout', "`"$Layout`"",
    '--output-backend', 'file',
    '--output-file', "`"$outFile`"",
    '--loglevel', 'info'
) -join ' '

Write-Host "engine : $Orender"
Write-Host "bridge : $BridgePath"
Write-Host "probe  : $ProbeWav ($Channels ch)"
Write-Host $argLine

$process = Start-Process -FilePath $Orender -ArgumentList $argLine `
    -RedirectStandardOutput $stdoutFile -RedirectStandardError $logFile -PassThru

# Wait for the dump to go quiet (or for the process to exit), then stop it.
$deadline = (Get-Date).AddSeconds($TimeoutSeconds)
$lastSize = -1
$quietSince = Get-Date
while ((Get-Date) -lt $deadline) {
    if ($process.HasExited) { break }
    $size = if (Test-Path $outFile) { (Get-Item $outFile).Length } else { 0 }
    if ($size -ne $lastSize) {
        $lastSize = $size
        $quietSince = Get-Date
    } elseif (((Get-Date) - $quietSince).TotalSeconds -ge $QuietSeconds -and $size -gt 0) {
        break
    }
    Start-Sleep -Milliseconds 250
}
if (-not $process.HasExited) {
    $process.Kill()
    Write-Host "(engine kept running after the dump settled - upstream behaviour; killed)"
}

Write-Host '--- engine log (bridge + writer lines) ---'
Select-String -Path $logFile -ErrorAction SilentlyContinue `
    -Pattern 'bridge|Bridge|Processing complete|Writing rendered audio|Output width|spatial renderer' |
    Select-Object -Last 20 | ForEach-Object { Write-Host "  $($_.Line)" }

if (-not (Test-Path $outFile) -or (Get-Item $outFile).Length -eq 0) {
    throw "no render output produced: $outFile (see $logFile)"
}

# The engine may narrow the dump (binaural output) after the first write; the
# last "Writing rendered audio … N channels" line is authoritative.
$renderChannels = $Channels
$logText = Get-Content $logFile -Raw -ErrorAction SilentlyContinue
if ($logText) {
    $hits = [regex]::Matches($logText, 'Writing rendered audio to .*?\((\d+) Hz, (\d+) channels')
    if ($hits.Count -gt 0) { $renderChannels = [int]$hits[$hits.Count - 1].Groups[2].Value }
}

Write-Host "--- render dump: $((Get-Item $outFile).Length) bytes, $renderChannels channel(s) ---"
python (Join-Path $PSScriptRoot '..\tools\analyze_raw_f32.py') $outFile $renderChannels $SampleRate
