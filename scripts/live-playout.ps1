# SPDX-License-Identifier: GPL-3.0-or-later
#
# Live playout: hear the rendered stream on the default audio device.
#
#     default output device ──(WASAPI loopback)──▶ viola_feeder
#                                                        │
#                                                        ▼
#                                              \\.\pipe\orender.input
#                                                        │
#                                         orender (spatial / binaural)
#                                                        │ stdout, raw f32
#                                                        ▼
#                                                   ffplay ──▶ speakers
#
# The engine's stdout goes straight into ffplay's stdin, so no growing temp
# file is left behind. (Writing the engine's --output-file to a named pipe
# does NOT work: file_sink opens the path with create+truncate, i.e. Win32
# CREATE_ALWAYS, which is rejected on a pipe with os error 87.)
#
# The engine's config is only READ: no --config, no --save-config.
#
# Usage:
#   pwsh -File scripts/live-playout.ps1
#   pwsh -File scripts/live-playout.ps1 -Seconds 20
#   pwsh -File scripts/live-playout.ps1 -Device "Voicemeeter Input"
#   pwsh -File scripts/live-playout.ps1 -ToneHz 0        # no test tone
[CmdletBinding()]
param(
    [string]$Orender = "$env:LOCALAPPDATA\Programs\Omniphony Studio\orender.exe",
    # Empty means "whatever the engine's own config says" (that is what makes
    # the binaural stage collapse the output to two channels).
    [string]$Layout = '',
    [string]$BridgePath = (Join-Path $PSScriptRoot '..\dist-live\viola-bridge-windows-x86_64\viola_bridge.dll'),
    [string]$FeederPath = (Join-Path $PSScriptRoot '..\dist-live\viola-feeder-windows-x86_64\viola_feeder.exe'),
    # With a DAW open on the viola-bridge ASIO driver the driver is already the
    # pipe client, so the feeder must NOT also run: it collides with it
    # (\\.\pipe\orender.input: os error 231, ERROR_PIPE_BUSY).
    [switch]$NoFeeder,
    [string]$Ffplay = 'ffplay',
    [string]$Device = '',
    # 0 (the default) keeps playing until Ctrl-C.
    [int]$Seconds = 0,
    [int]$SampleRate = 48000,
    [int]$Channels = 2,
    # 0 disables the test tone and simply plays whatever the endpoint carries.
    [double]$ToneHz = 1000,
    [double]$ToneVolume = 25,
    [string]$ProbeDir = (Join-Path $PSScriptRoot '..\probe')
)

$ErrorActionPreference = 'Stop'

foreach ($required in @($Orender, $BridgePath)) {
    if (-not (Test-Path $required)) { throw "missing: $required" }
}
if (-not $NoFeeder -and -not (Test-Path $FeederPath)) { throw "missing: $FeederPath (or pass -NoFeeder)" }
New-Item -ItemType Directory -Force -Path $ProbeDir | Out-Null

$pipePath = '\\.\pipe\orender.input'
$engineLog = Join-Path $ProbeDir 'playout_engine.log'
$playLog = Join-Path $ProbeDir 'playout_ffplay.log'
$feederLog = Join-Path $ProbeDir 'playout_feeder.log'
$feederOut = Join-Path $ProbeDir 'playout_feeder_stdout.txt'
$toneLog = Join-Path $ProbeDir 'playout_tone.log'
$cmdFile = Join-Path $ProbeDir 'playout_pipeline.cmd'
Remove-Item $engineLog, $playLog, $feederLog, $feederOut, $toneLog, $cmdFile -ErrorAction SilentlyContinue

$engineLine = @(
    'render', "`"$pipePath`"",
    '--continuous',
    '--bridge-path', "`"$BridgePath`"",
    '--enable-vbap'
)
if ($Layout) { $engineLine += @('--speaker-layout', "`"$Layout`"") }
$engineLine += @(
    '--output-backend', 'file',
    '--output-file', '-',
    '--output-file-format', 'raw-f32',
    '--loglevel', 'info'
)
$engineLine = $engineLine -join ' '

# cmd.exe is what wires one process's stdout into the next one's stdin; both
# streams have to be redirected to files because PowerShell cannot hand a
# pipe to Start-Process. The engine and ffplay read 48 kHz stereo raw f32,
# which is exactly what the binaural stage emits.
# The two commands are built separately and joined with a literal pipe: an
# @(a, b) list whose second element spans lines collapses into one element and
# silently drops the '|', which makes the engine eat ffplay as an argument.
$playCmd = '"' + $Orender + '" ' + $engineLine + ' 2>"' + $engineLog + '"'
$playerCmd = '"' + $Ffplay + '" -nodisp -hide_banner -loglevel info -f f32le -ar ' + $SampleRate +
    ' -ch_layout stereo -i - 2>"' + $playLog + '"'
$playLine = $playCmd + ' | ' + $playerCmd
Set-Content -Path $cmdFile -Value "@echo off`r`n$playLine" -Encoding ASCII

Get-Process orender, viola_feeder, ffplay, ffmpeg -ErrorAction SilentlyContinue |
    Stop-Process -Force -ErrorAction SilentlyContinue
Start-Sleep -Seconds 1

$pipeline = $null
$tone = $null
$feeder = $null
try {
    Write-Host "engine : $Orender"
    Write-Host "player : $Ffplay (default audio device)"
    Write-Host "pipe   : $pipePath"
    Write-Host "source : $(if ($Device) { $Device } else { 'default render endpoint (loopback)' })"

    $pipeline = Start-Process cmd.exe -ArgumentList ('/c "' + $cmdFile + '"') -PassThru -WindowStyle Hidden
    # The engine creates the pipe at startup and then waits; give it that
    # moment so the feeder's connect does not race the server into existence.
    Start-Sleep -Seconds 3

    # The tone is what the loopback capture picks up, so it only makes sense when
    # this script is the one feeding the engine; with a DAW it would just be noise.
    if (-not $NoFeeder -and $ToneHz -gt 0) {
        Write-Host "playing a ${ToneHz} Hz tone for the whole run at volume ${ToneVolume}% (this is audible)"
        # ffplay has no -ac in this build; the tone is already stereo.
        $tone = Start-Process -FilePath $Ffplay -ArgumentList @(
            '-nodisp', '-autoexit', '-hide_banner', '-loglevel', 'error',
            '-volume', "$ToneVolume",
            '-f', 'lavfi', '-i', "sine=frequency=${ToneHz}"
        ) -RedirectStandardError $toneLog -PassThru -WindowStyle Hidden
    }

    if ($NoFeeder) {
        Write-Host 'no feeder: the DAW driver is the pipe client; playing for the whole run'
        if ($Seconds -gt 0) { Start-Sleep -Seconds $Seconds } else { Write-Host 'playing until Ctrl-C ...'; while ($true) { Start-Sleep -Seconds 1 } }
    } else {
        $feederArgs = @(
            '--seconds', "$Seconds",
            '--rate', "$SampleRate",
            '--channels', "$Channels",
            '--pipe', $pipePath,
            '--stats'
        )
        if ($Device) { $feederArgs += @('--device', $Device) }
        $feeder = Start-Process -FilePath $FeederPath -ArgumentList $feederArgs `
            -RedirectStandardOutput $feederOut -RedirectStandardError $feederLog -PassThru -WindowStyle Hidden

        if ($Seconds -gt 0) {
            if (-not $feeder.WaitForExit(($Seconds + 30) * 1000)) {
                $feeder.Kill()
                Write-Warning 'the feeder did not exit in time; killed'
            }
        } else {
            Write-Host 'playing until Ctrl-C ...'
            $feeder.WaitForExit()
        }
    }
}
finally {
    foreach ($p in @($feeder, $tone, $pipeline)) {
        if ($p) {
            $p.Refresh()
            if (-not $p.HasExited) { $p.Kill() }
        }
    }
    Get-Process orender, viola_feeder, ffplay -ErrorAction SilentlyContinue |
        Stop-Process -Force -ErrorAction SilentlyContinue

    if (-not $NoFeeder) {
        Write-Host '--- feeder output ---'
        Get-Content $feederOut, $feederLog -ErrorAction SilentlyContinue |
            ForEach-Object { Write-Host "  $_" }
        $busy = Select-String -Path $feederOut, $feederLog -Pattern 'os error 231' -Quiet -ErrorAction SilentlyContinue
        if ($busy) {
            Write-Warning 'the pipe was already taken: a DAW is driving the ASIO driver. Re-run with -NoFeeder.'
        }
    }
    Write-Host '--- engine log ---'
    if (Test-Path $engineLog) {
        Select-String -Path $engineLog `
            -Pattern 'Client connected|Detected raw stream|Writing rendered audio|Output width|failed' |
            Select-Object -Last 8 | ForEach-Object { Write-Host "  $($_.Line)" }
    }
    Write-Host '--- player log ---'
    if (Test-Path $playLog) {
        Select-String -Path $playLog `
            -Pattern 'Input #0|Stream #0|error' |
            Select-Object -Last 6 | ForEach-Object { Write-Host "  $($_.Line)" }
    }
    # ffplay at -loglevel warning is silent on success, which would leave no
    # evidence that the stream was ever opened: require the decoder line.
    $opened = Select-String -Path $playLog -Pattern 'Stream #0' -Quiet -ErrorAction SilentlyContinue
    if ($opened) {
        Write-Host 'player : opened the rendered stream (no error reported)'
    } else {
        Write-Warning "the player never opened a stream; see $playLog"
    }
}
