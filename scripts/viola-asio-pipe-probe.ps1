# SPDX-License-Identifier: GPL-3.0-or-later
#
# M5.3 acceptance: the ASIO driver's sixteen output channels reach the pipe.
#
#     a host (probe-asio-com.ps1) ──▶ viola_asio ──▶ \\.\pipe\orender.input ──▶ .f32
#
# Two ways to read that path:
#
#   default          this script IS the pipe server. It spawns the COM probe,
#                    which loads viola_asio and plays a DAW, and receives the
#                    driver's streaming WAV straight off the pipe. Nothing but
#                    the driver is needed, which is what makes it useful in CI.
#   -Orender <exe>   the engine is the server instead: orender creates the pipe
#                    and renders into its own --output-file dump, exactly as
#                    scripts/pipe-feed-probe.ps1 drives it.
#
# The engine's config is only ever READ: this script passes no --config and
# never uses --save-config, so %ProgramData%\omniphony\config.yaml is untouched.
#
# Usage:
#   pwsh -File scripts/viola-asio-pipe-probe.ps1
#   pwsh -File scripts/viola-asio-pipe-probe.ps1 -DllPath path\to\viola_asio.dll -ToneHz 2000 -Seconds 4
#   pwsh -File scripts/viola-asio-pipe-probe.ps1 -Orender "$env:LOCALAPPDATA\Programs\Omniphony Studio\orender.exe"
#
# Only PowerShell/.NET is used, so it needs no local toolchain.
[CmdletBinding()]
param(
    # Where the COM probe loads the driver from; the CI artifact lands here.
    [string]$DllPath = (Join-Path $PSScriptRoot '..\dist\viola-asio-windows-x86_64\viola_asio.dll'),
    # Empty (the default) means this script is the pipe server. A path makes the
    # engine the server and turns this script into the thing that starts it.
    [string]$Orender = '',
    [string]$Layout = "$env:LOCALAPPDATA\Programs\Omniphony Studio\layouts\9.1.6.yaml",
    [string]$ProbeDir = (Join-Path $PSScriptRoot '..\probe'),
    # 16 in + 16 out: every driver output carries a tone, so 16 channels arrive.
    [int]$Channels = 16,
    [int]$SampleRate = 48000,
    [int]$BufferSize = 512,
    # Channel N gets base*(N+1) Hz, so the table below shows sixteen distinct
    # lines rather than one tone smeared across them.
    [int]$ToneHz = 1000,
    # How long the probe is left streaming.
    [int]$Seconds = 3,
    [string]$PipeName = 'orender.input',
    [int]$TimeoutSeconds = 60
)

$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest
# System.IO.Pipes is part of the shared framework in PowerShell 7; no Add-Type.

$probeScript = Join-Path $PSScriptRoot 'probe-asio-com.ps1'
$analyzerScript = Join-Path $PSScriptRoot '..\tools\analyze_raw_f32.py'
New-Item -ItemType Directory -Force -Path $ProbeDir | Out-Null

$pipePath = "\\.\pipe\$PipeName"
$f32File = Join-Path $ProbeDir 'asio_pipe_out.f32'
$probeLog = Join-Path $ProbeDir 'asio_pipe_probe.log'
$probeOut = Join-Path $ProbeDir 'asio_pipe_probe_stdout.txt'
Remove-Item $f32File, $probeLog, $probeOut -ErrorAction SilentlyContinue

# The driver writes WAVE_FORMAT_IEEE_FLOAT. `pipe.rs` sends both the fmt size
# and the data size as u32::MAX ("until the input ends"), so the sizes cannot be
# trusted; the tags and the frame format are what matter.
function Assert-RiffHeader([byte[]]$bytes) {
    if ($bytes.Length -lt 44) { throw "the driver sent $($bytes.Length) bytes; a 44-byte header alone is missing" }
    $riff = [System.Text.Encoding]::ASCII.GetString($bytes, 0, 4)
    $wave = [System.Text.Encoding]::ASCII.GetString($bytes, 8, 4)
    $fmt  = [System.Text.Encoding]::ASCII.GetString($bytes, 12, 4)
    $data = [System.Text.Encoding]::ASCII.GetString($bytes, 36, 4)
    $formatTag = [BitConverter]::ToUInt16($bytes, 20)
    $nChannels = [BitConverter]::ToUInt16($bytes, 22)
    $rate      = [BitConverter]::ToUInt32($bytes, 24)
    $blockAlign = [BitConverter]::ToUInt16($bytes, 32)
    $bits      = [BitConverter]::ToUInt16($bytes, 34)
    Write-Host ("header     : {0}/{1} {2} tag={3} channels={4} rate={5} bits={6} blockAlign={7} data={8}" -f `
        $riff, $wave, $fmt.Trim(), $formatTag, $nChannels, $rate, $bits, $blockAlign, $data.Trim())
    if ($riff -ne 'RIFF' -or $wave -ne 'WAVE' -or $fmt -ne 'fmt ') { throw 'the stream does not start with a RIFF/WAVE format chunk' }
    # 3 is WAVE_FORMAT_IEEE_FLOAT; 32 bits makes blockAlign 4 * channels.
    if ($formatTag -ne 3) { throw "expected WAVE_FORMAT_IEEE_FLOAT (3), got $formatTag" }
    if ($bits -ne 32) { throw "expected 32-bit samples, got $bits" }
    if ($nChannels -ne $Channels) { throw "expected $Channels channels, header says $nChannels" }
    if ($rate -ne $SampleRate) { throw "expected $SampleRate Hz, header says $rate" }
    if ($blockAlign -ne (4 * $nChannels)) { throw "expected blockAlign $((4 * $nChannels)), got $blockAlign" }
}

# Spawns the COM probe so the driver starts talking, and returns the process.
function Start-AsioProbe {
    if (-not (Test-Path $DllPath)) {
        throw "missing ASIO driver: $DllPath (download the CI artifact first, or pass -DllPath)"
    }
    # The COM probe is a script, so it needs the same interpreter this script is
    # running on; $PSHOME differs between Windows PowerShell 5.1 and PowerShell 7.
    $hostExe = (Get-Process -Id $PID).Path
    $probeArgs = @(
        '-NoProfile', '-File', "`"$probeScript`"",
        '-DllPath', "`"$DllPath`"",
        '-BufferSize', "$BufferSize",
        '-ToneHz', "$ToneHz",
        '-Seconds', "$Seconds"
    )
    Write-Host "driver : $DllPath"
    Write-Host "tone   : ${ToneHz} Hz base x$Channels channels for ${Seconds}s"
    Start-Process -FilePath $hostExe -ArgumentList $probeArgs `
        -RedirectStandardOutput $probeOut -RedirectStandardError $probeLog -PassThru
}

if ($Orender) {
    # ---- engine-as-server mode ------------------------------------------------
    if (-not (Test-Path $Orender)) { throw "missing: $Orender" }
    if (-not (Test-Path $Layout)) { throw "missing: $Layout" }

    $engineOut = Join-Path $ProbeDir 'asio_pipe_render.f32'
    Remove-Item $engineOut -ErrorAction SilentlyContinue
    # The engine creates the pipe and waits for a client; the driver is that
    # client, exactly as `viola_feeder/src/pipe.rs` records.
    $argLine = @(
        'render', "`"$pipePath`"",
        '--continuous',
        '--enable-vbap',
        '--speaker-layout', "`"$Layout`"",
        '--output-backend', 'file',
        '--output-file', "`"$engineOut`"",
        '--loglevel', 'info'
    ) -join ' '
    Write-Host "engine : $Orender"
    Write-Host "pipe   : $pipePath"
    $engine = Start-Process -FilePath $Orender -ArgumentList $argLine `
        -RedirectStandardOutput (Join-Path $ProbeDir 'asio_pipe_engine_stdout.txt') `
        -RedirectStandardError (Join-Path $ProbeDir 'asio_pipe_engine.log') -PassThru

    try {
        $probe = Start-AsioProbe
        if (-not $probe.WaitForExit(($Seconds + 30) * 1000)) {
            $probe.Kill()
            Write-Warning 'the COM probe did not exit in time; killed'
        }
    }
    finally {
        $deadline = (Get-Date).AddSeconds($TimeoutSeconds)
        while ((Get-Date) -lt $deadline -and -not $engine.HasExited) { Start-Sleep -Milliseconds 500 }
        if (-not $engine.HasExited) {
            $engine.Kill()
            Write-Host '(engine kept running after the stream - upstream behaviour; killed)'
        }
    }

    Write-Host '--- COM probe output ---'
    Get-Content $probeOut, $probeLog -ErrorAction SilentlyContinue | ForEach-Object { Write-Host "  $_" }

    if (-not (Test-Path $engineOut) -or (Get-Item $engineOut).Length -eq 0) {
        throw "no render output produced: $engineOut (see $ProbeDir\asio_pipe_engine.log)"
    }
    # The engine may narrow the dump after the first write, so its own log line is
    # authoritative for the channel count.
    $renderChannels = $Channels
    $logText = Get-Content (Join-Path $ProbeDir 'asio_pipe_engine.log') -Raw -ErrorAction SilentlyContinue
    if ($logText) {
        $hits = [regex]::Matches($logText, 'Writing rendered audio to .*?\((\d+) Hz, (\d+) channels')
        if ($hits.Count -gt 0) { $renderChannels = [int]$hits[$hits.Count - 1].Groups[2].Value }
    }
    Write-Host "--- engine dump: $((Get-Item $engineOut).Length) bytes, $renderChannels channel(s) ---"
    python $analyzerScript $engineOut $renderChannels $SampleRate
    exit
}

# ---- pipe-server mode ---------------------------------------------------------
# This script plays orender: create the pipe, wait for the driver to attach,
# then read the streaming WAV it sends.
$server = New-Object System.IO.Pipes.NamedPipeServerStream(
    $PipeName,
    [System.IO.Pipes.PipeDirection]::In,
    1,
    [System.IO.Pipes.PipeTransmissionMode]::Byte,
    [System.IO.Pipes.PipeOptions]::None,
    65536,
    65536)

try {
    $wait = $server.BeginWaitForConnection($null, $null)
    Write-Host "pipe   : $pipePath (this script is the server)"
    # The probe has to start after the wait is armed, or the driver's connect
    # races the server into existence and fails once before retrying.
    $probe = Start-AsioProbe
    Write-Host "waiting for the driver to attach (timeout ${TimeoutSeconds}s)..."
    if (-not $wait.AsyncWaitHandle.WaitOne($TimeoutSeconds * 1000)) {
        throw "the driver never attached to $pipePath"
    }
    $server.EndWaitForConnection($wait)
    Write-Host 'driver attached; reading'

    $stream = New-Object System.IO.MemoryStream
    $buffer = New-Object byte[] 65536
    $deadline = (Get-Date).AddSeconds($TimeoutSeconds)
    # One read is always in flight; a 250 ms wait lets us notice the probe exit
    # without blocking forever on a driver that has gone quiet.
    $async = $server.BeginRead($buffer, 0, $buffer.Length, $null, $null)
    while ((Get-Date) -lt $deadline) {
        if ($async.AsyncWaitHandle.WaitOne(250)) {
            $n = $server.EndRead($async)
            if ($n -le 0) { break }
            $stream.Write($buffer, 0, $n)
            $async = $server.BeginRead($buffer, 0, $buffer.Length, $null, $null)
        } elseif ($probe.HasExited) {
            break
        }
    }
    if (-not $probe.HasExited) {
        $probe.Kill()
        Write-Warning 'the COM probe did not exit in time; killed'
    }

    $bytes = $stream.ToArray()
    Write-Host "received   : $($bytes.Length) bytes"
    if ($bytes.Length -le 44) { throw "the driver sent no audio after the header ($($bytes.Length) bytes)" }

    $header = New-Object byte[] 44
    [Array]::Copy($bytes, 0, $header, 0, 44)
    Assert-RiffHeader $header

    # Skip the 44-byte streaming header: analyze_raw_f32.py reads headerless f32.
    $body = New-Object byte[] ($bytes.Length - 44)
    [Array]::Copy($bytes, 44, $body, 0, $body.Length)
    [System.IO.File]::WriteAllBytes($f32File, $body)
    Write-Host "dump       : $f32File ($($body.Length) bytes, $([int]($body.Length / (4 * $Channels))) frames)"
}
finally {
    if ($server.IsConnected) { $server.Disconnect() }
    $server.Dispose()
}

Write-Host '--- COM probe output ---'
Get-Content $probeOut, $probeLog -ErrorAction SilentlyContinue | ForEach-Object { Write-Host "  $_" }

python $analyzerScript $f32File $Channels $SampleRate
