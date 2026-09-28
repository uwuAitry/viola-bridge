# SPDX-License-Identifier: GPL-3.0-or-later
#
# M3 acceptance: live audio from Windows into orender.
#
#     default output device  ──(WASAPI loopback)──▶ viola_feeder
#        ▲                                                  │
#        │ ffplay plays a test tone                          ▼
#        └──────────── speakers ─────────────  \\.\pipe\orender.input
#                                                              │
#                                            orender + viola_bridge ──▶ render dump
#
# Both binaries come from the cloud CI; nothing is built or installed here.
# The engine's config is only read (no --config, no --save-config).
#
# Usage:
#   pwsh -File scripts/live-capture-probe.ps1
#   pwsh -File scripts/live-capture-probe.ps1 -Seconds 8 -ToneVolume 10
[CmdletBinding()]
param(
    [string]$Orender = "$env:LOCALAPPDATA\Programs\Omniphony Studio\orender.exe",
    [string]$Layout = "$env:LOCALAPPDATA\Programs\Omniphony Studio\layouts\9.1.6.yaml",
    [string]$BridgePath = (Join-Path $PSScriptRoot '..\dist-live\viola-bridge-windows-x86_64\viola_bridge.dll'),
    [string]$FeederPath = (Join-Path $PSScriptRoot '..\dist-live\viola-feeder-windows-x86_64\viola_feeder.exe'),
    [string]$ProbeDir = (Join-Path $PSScriptRoot '..\probe'),
    [int]$Seconds = 6,
    [int]$Channels = 2,
    [int]$SampleRate = 48000,
    [double]$ToneHz = 1000,
    [int]$ToneVolume = 15,
    [switch]$NoTone,
    [string]$Device,
    [int]$TimeoutSeconds = 40
)

$ErrorActionPreference = 'Stop'

foreach ($required in @($Orender, $Layout, $BridgePath, $FeederPath)) {
    if (-not (Test-Path $required)) { throw "missing: $required" }
}
New-Item -ItemType Directory -Force -Path $ProbeDir | Out-Null

$outFile = Join-Path $ProbeDir 'live_render_out.f32'
$logFile = Join-Path $ProbeDir 'live_render.log'
$stdoutFile = Join-Path $ProbeDir 'live_render_stdout.txt'
$feederLog = Join-Path $ProbeDir 'live_feeder.log'
$feederOut = Join-Path $ProbeDir 'live_feeder_stdout.txt'
$toneLog = Join-Path $ProbeDir 'live_tone.log'
$toneOut = Join-Path $ProbeDir 'live_tone_stdout.txt'
Remove-Item $outFile, $logFile, $stdoutFile, $feederLog, $feederOut, $toneLog, $toneOut -ErrorAction SilentlyContinue

# The feeder writes a RIFF header, so the bridge's `auto` mode reads the format
# from the stream itself. Nothing else has to be kept in sync.
$env:VIOLA_BRIDGE_MODE = 'auto'

$pipePath = '\\.\pipe\orender.input'
$engineLine = @(
    'render', "`"$pipePath`"",
    '--continuous',
    '--bridge-path', "`"$BridgePath`"",
    '--enable-vbap',
    '--speaker-layout', "`"$Layout`"",
    '--output-backend', 'file',
    '--output-file', "`"$outFile`"",
    '--loglevel', 'info'
) -join ' '

Write-Host "engine : $Orender"
Write-Host "feeder : $FeederPath"
Write-Host "pipe   : $pipePath"

# The engine creates the pipe and waits for a client.
$engine = Start-Process -FilePath $Orender -ArgumentList $engineLine `
    -RedirectStandardOutput $stdoutFile -RedirectStandardError $logFile -PassThru
Start-Sleep -Seconds 2

$tone = $null
if (-not $NoTone) {
    Write-Host "playing a ${ToneHz} Hz tone for ${Seconds}s at volume ${ToneVolume}% (this is audible)"
    $toneArgs = @(
        '-nodisp', '-autoexit', '-hide_banner', '-loglevel', 'error',
        '-volume', "$ToneVolume",
        # `${ToneHz}` must be braced: "$ToneHz:duration" parses as a variable
        # literally named `ToneHz:duration`, which silently yields
        # "sine=frequency==8" and the tone never plays.
        '-f', 'lavfi', '-i', "sine=frequency=${ToneHz}:duration=$($Seconds + 2)"
    )
    $tone = Start-Process -FilePath 'ffplay' -ArgumentList $toneArgs `
        -RedirectStandardOutput $toneOut -RedirectStandardError $toneLog -PassThru
}

$feederArgs = @(
    '--seconds', "$Seconds",
    '--rate', "$SampleRate",
    '--channels', "$Channels",
    '--pipe', "$pipePath",
    '--stats'
)
if ($Device) { $feederArgs += @('--device', $Device) }

Write-Host "running the feeder for ${Seconds}s ..."
# Start-Process must not be given the same path for both streams.
$feeder = Start-Process -FilePath $FeederPath -ArgumentList $feederArgs `
    -RedirectStandardOutput $feederOut -RedirectStandardError $feederLog -PassThru
# Bounded wait: -Wait has hung on us before when the engine keeps the stream open.
if (-not $feeder.WaitForExit(($Seconds + 30) * 1000)) {
    $feeder.Kill()
    Write-Warning 'the feeder did not exit in time; killed'
}
Write-Host '--- feeder output ---'
Get-Content $feederOut, $feederLog -ErrorAction SilentlyContinue | ForEach-Object { Write-Host "  $_" }

if ($tone -and -not $tone.HasExited) { $tone.Kill() }

$deadline = (Get-Date).AddSeconds($TimeoutSeconds)
while ((Get-Date) -lt $deadline -and -not $engine.HasExited) { Start-Sleep -Milliseconds 500 }
if (-not $engine.HasExited) {
    $engine.Kill()
    Write-Host '(engine kept running - upstream behaviour; killed)'
}

Write-Host '--- engine log (bridge + writer lines) ---'
Select-String -Path $logFile -ErrorAction SilentlyContinue `
    -Pattern 'Loading format bridge|Processing complete|Writing rendered audio|Output width|Live input' |
    Select-Object -Last 12 | ForEach-Object { Write-Host "  $($_.Line)" }

if (-not (Test-Path $outFile) -or (Get-Item $outFile).Length -eq 0) {
    throw "no render output produced: $outFile (see $logFile)"
}

$renderChannels = $Channels
$logText = Get-Content $logFile -Raw -ErrorAction SilentlyContinue
if ($logText) {
    $ch = [regex]::Matches($logText, 'Writing rendered audio to .*?\((\d+) Hz, (\d+) channels')
    if ($ch.Count -gt 0) { $renderChannels = [int]$ch[$ch.Count - 1].Groups[2].Value }
}

Write-Host "--- live render dump: $((Get-Item $outFile).Length) bytes, $renderChannels channel(s) ---"
python (Join-Path $PSScriptRoot '..\tools\analyze_raw_f32.py') $outFile $renderChannels $SampleRate
