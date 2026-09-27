# SPDX-License-Identifier: GPL-3.0-or-later
#
# M2 probe: proves the full pipe chain
#
#     WAV bytes ──▶ \\.\pipe\orender.input ──▶ orender ──▶ viola_bridge ──▶ render dump
#
# `orender` reads a named pipe natively (`render \\.\pipe\orender.input
# --continuous`), which is exactly how Omniphony Studio drives its own engine.
# This script is the other end: it creates the pipe, waits for the engine to
# attach, streams a multichannel WAV into it, then reports what came out.
#
# Only PowerShell/.NET is used, so it needs no local toolchain.
#
# Usage:
#   pwsh -File scripts/pipe-feed-probe.ps1
#   pwsh -File scripts/pipe-feed-probe.ps1 -BridgePath dist\viola_bridge.dll -ProbeWav probe\tone916_all.wav
[CmdletBinding()]
param(
    [string]$Orender = "$env:LOCALAPPDATA\Programs\Omniphony Studio\orender.exe",
    [string]$Layout = "$env:LOCALAPPDATA\Programs\Omniphony Studio\layouts\9.1.6.yaml",
    [string]$BridgePath = (Join-Path $PSScriptRoot '..\dist\viola-bridge-windows-x86_64\viola_bridge.dll'),
    [string]$ProbeDir = (Join-Path $PSScriptRoot '..\probe'),
    [string]$ProbeWav,
    [int]$Channels = 16,
    [int]$SampleRate = 48000,
    [string]$PipeName = 'orender.input',
    [int]$TimeoutSeconds = 60,
    [int]$ChunkBytes = 32768,
    # Pace the feed to the file's byte rate (a live feed) instead of dumping it.
    [switch]$Realtime
)

$ErrorActionPreference = 'Stop'
# System.IO.Pipes is part of the shared framework in PowerShell 7; no Add-Type.

foreach ($required in @($Orender, $Layout, $BridgePath)) {
    if (-not (Test-Path $required)) { throw "missing: $required" }
}
New-Item -ItemType Directory -Force -Path $ProbeDir | Out-Null

if (-not $ProbeWav) { $ProbeWav = Join-Path $ProbeDir "tone${Channels}_all.wav" }
if (-not (Test-Path $ProbeWav)) {
    $layoutName = if ($Channels -eq 16) { '9.1.6' } else { "$($Channels)c" }
    & ffmpeg -y -hide_banner -loglevel error -f lavfi `
        -i "sine=frequency=1000:sample_rate=$SampleRate:duration=1" `
        -af "aformat=channel_layouts=$layoutName" -c:a pcm_f32le $ProbeWav
    if ($LASTEXITCODE -ne 0) { throw "ffmpeg could not generate the probe WAV" }
}

$outFile = Join-Path $ProbeDir 'pipe_render_out.f32'
$logFile = Join-Path $ProbeDir 'pipe_render.log'
$stdoutFile = Join-Path $ProbeDir 'pipe_render_stdout.txt'
Remove-Item $outFile, $logFile, $stdoutFile -ErrorAction SilentlyContinue

# ---- engine side ---------------------------------------------------------
# `auto` mode: the stream starts with RIFF, so the bridge takes the WAV path.
$env:VIOLA_BRIDGE_MODE = 'auto'
$env:VIOLA_BRIDGE_CHANNELS = "$Channels"
$env:VIOLA_BRIDGE_RATE = "$SampleRate"

$pipePath = "\\.\pipe\$PipeName"
$argLine = @(
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
Write-Host "pipe   : $pipePath"
Write-Host "bridge : $BridgePath"
Write-Host "source : $ProbeWav"

$engine = Start-Process -FilePath $Orender -ArgumentList $argLine `
    -RedirectStandardOutput $stdoutFile -RedirectStandardError $logFile -PassThru

# ---- feeder side ---------------------------------------------------------
$bytes = [System.IO.File]::ReadAllBytes($ProbeWav)
$server = New-Object System.IO.Pipes.NamedPipeServerStream(
    $PipeName,
    [System.IO.Pipes.PipeDirection]::Out,
    1,
    [System.IO.Pipes.PipeTransmissionMode]::Byte,
    [System.IO.Pipes.PipeOptions]::None,
    65536,
    65536)

try {
    Write-Host "waiting for the engine to attach to the pipe (timeout ${TimeoutSeconds}s)..."
    $wait = $server.BeginWaitForConnection($null, $null)
    if (-not $wait.AsyncWaitHandle.WaitOne($TimeoutSeconds * 1000)) {
        throw "the engine never attached to $pipePath"
    }
    $server.EndWaitForConnection($wait)
    Write-Host "engine attached; streaming $($bytes.Length) bytes"

    # Byte rate from the fmt chunk, used only to pace -Realtime feeds.
    $byteRate = 0
    $fmt = -1
    for ($i = 12; $i -lt $bytes.Length - 8; $i++) {
        if ($bytes[$i] -eq 0x66 -and $bytes[$i + 1] -eq 0x6D -and $bytes[$i + 2] -eq 0x74 -and $bytes[$i + 3] -eq 0x20) { $fmt = $i; break }
    }
    if ($fmt -ge 0) {
        $byteRate = [BitConverter]::ToUInt32($bytes, $fmt + 8 + 8)
        Write-Host "source byte rate: $byteRate B/s"
    }

    $sw = [System.Diagnostics.Stopwatch]::StartNew()
    $sent = 0
    while ($sent -lt $bytes.Length) {
        $n = [Math]::Min($ChunkBytes, $bytes.Length - $sent)
        $server.Write($bytes, $sent, $n)
        $server.Flush()
        $sent += $n
        if ($Realtime -and $byteRate -gt 0) {
            $targetSeconds = $sent / [double]$byteRate
            $behind = $targetSeconds - $sw.Elapsed.TotalSeconds
            if ($behind -gt 0) { Start-Sleep -Milliseconds ([int]($behind * 1000)) }
        }
    }
    Write-Host "all $sent bytes written in $([int]$sw.ElapsedMilliseconds) ms"
    $server.WaitForPipeDrain()
}
finally {
    $server.Dispose()
}

# ---- collect -------------------------------------------------------------
$deadline = (Get-Date).AddSeconds($TimeoutSeconds)
while ((Get-Date) -lt $deadline -and -not $engine.HasExited) { Start-Sleep -Milliseconds 500 }
if (-not $engine.HasExited) {
    $engine.Kill()
    Write-Host "(engine kept running after the feed - upstream behaviour; killed)"
}

Write-Host '--- engine log (bridge + writer lines) ---'
Select-String -Path $logFile -ErrorAction SilentlyContinue `
    -Pattern 'bridge|Bridge|Processing complete|Writing rendered audio|Output width|decoding|pipe' |
    Select-Object -Last 20 | ForEach-Object { Write-Host "  $($_.Line)" }

if (-not (Test-Path $outFile) -or (Get-Item $outFile).Length -eq 0) {
    throw "no render output produced: $outFile (see $logFile)"
}

$renderChannels = $Channels
$logText = Get-Content $logFile -Raw -ErrorAction SilentlyContinue
if ($logText) {
    $ch = [regex]::Matches($logText, 'Writing rendered audio to .*?\((\d+) Hz, (\d+) channels')
    if ($ch.Count -gt 0) { $renderChannels = [int]$ch[$ch.Count - 1].Groups[2].Value }
}

Write-Host "--- render dump: $((Get-Item $outFile).Length) bytes, $renderChannels channel(s) ---"
python (Join-Path $PSScriptRoot '..\tools\analyze_raw_f32.py') $outFile $renderChannels $SampleRate
