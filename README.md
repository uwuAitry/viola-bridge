# viola-bridge

A **decoder bridge plugin for the [Omniphony](https://github.com/mgth/Omniphony) renderer**
(`orender`) that presents plain PCM as a channel bed — including a proper
**9.1.6 (16-channel) label map**, which the upstream reference bridge does not
have (it stops at 12 channels).

`orender` does not decode anything by itself: it loads a `*_bridge.dll` /
`lib*_bridge.so` at runtime and refuses to start without one. This repository
builds that plugin **in the cloud**, so no local toolchain is required.

## What it is for

Feeding a DAW's multichannel output into `orender` for rendering, on Windows,
where the engine's own `live` input is not implemented
(`live input is not implemented on this platform`) and the `input-live` CLI
subcommand is a stub.

The intended shape is:

```
[your source] ──writes PCM──▶ \\.\pipe\orender.input ──▶ orender ──▶ viola_bridge ──▶ speakers / binaural
                                                          (render … --bridge-path viola_bridge.dll)
```

`orender` reads a named pipe natively (`render \\.\pipe\orender.input
--continuous`), so the pipe is the supported half of the problem; the bridge is
the missing half, and that is what this repo provides.

## Modes

Selected by `VIOLA_BRIDGE_MODE` when the bridge instance is created:

| mode | behaviour |
|---|---|
| `auto` (default) | a `RIFF`/`WAVE` byte stream is parsed as WAV; anything else is headerless PCM |
| `wav` | always parse a WAVE header; a non-RIFF stream is a fatal error |
| `raw` | never look for a header; the stream is headerless PCM |

Headerless PCM takes its layout from the environment: `VIOLA_BRIDGE_CHANNELS`
(default `16`), `VIOLA_BRIDGE_RATE` (default `48000`), `VIOLA_BRIDGE_FORMAT`
(`f32` default, or `s16` / `s24` / `s32`).

A WAV stream may declare its `data` chunk size as `0` or `0xFFFFFFFF` to mean
"until the input ends" — which is what a live producer should write.

Recognised channel counts: `1, 2, 6, 8, 10, 12, 16`. The 16-channel map is the
9.1.6 order:

```
L R C LFE Lw Rw Ls Rs Lb Rb Tfl Tfr Tsl Tsr Tbl Tbr
```

## Installing the plugin

Build it (see below), then place the DLL **next to `orender.exe`** — the host
auto-discovers the first file matching `*_bridge.dll` in its own directory — or
point at it explicitly:

```
orender.exe render \\.\pipe\orender.input --continuous --enable-vbap ^
  --speaker-layout "layouts\9.1.6.yaml" ^
  --bridge-path "viola_bridge.dll" ^
  --output-backend file --output-file out.f32
```

`--bridge-path`, `render.bridge_path` in `%ProgramData%\omniphony\config.yaml`,
the `ORENDER_BRIDGE_DIR` environment variable and auto-discovery are consulted in
that order.

## Building in the cloud

`.github/workflows/build.yml` runs on `windows-latest` and publishes four
artifacts:

- `viola-bridge-windows-x86_64` — `viola_bridge.dll` + `SHA256SUMS.txt`
- `viola-feeder-windows-x86_64` — `viola_feeder.exe` + `SHA256SUMS.txt`
- `viola-asio-windows-x86_64` — `viola_asio.dll` + `SHA256SUMS.txt`, the virtual
  ASIO device; see [16-channel ASIO device (M5)](#16-channel-asio-device-m5)
- `upstream-reference-bridge-windows-x86_64` — the upstream reference bridge,
  built from the pinned revision, as a known-good baseline

Nothing from upstream is vendored into this repository: the workflow (and
`scripts/bootstrap.ps1` for a local run) clones the pinned revision into
`third_party/`, which is git-ignored and used as a path dependency for the
`bridge_api` crate.

The pinned revision is `b81f518831e89864a6391744cf9daa9eeadb3c51` — the commit
the author's local `orender` reports as its build string (`b81f518-dirty`), so
the plugin ABI matches the engine by construction.

## Verifying locally

```powershell
pwsh -File scripts/verify-channel-bed.ps1 `
  -BridgePath dist\viola-bridge-windows-x86_64\viola_bridge.dll
```

It renders a generated 16-channel 1 kHz probe through the plugin with the
installed `orender` and prints per-channel levels of the render dump. The
engine's config is only read — the script passes no `--config` and never uses
`--save-config`.

Two upstream behaviours the script accounts for:

- With `--output-backend file`, loading a bridge puts the engine into its live
  input manager, so it decodes the whole stream (`Processing complete: 47
  frames`) and then keeps running. The script waits for the dump to stop growing
  and kills it. **This happens with the upstream reference bridge too** — it is
  not something viola-bridge causes.
- The first write is at the layout width (16 channels); the binaural stage then
  narrows it to 2. The script reads the last `Writing rendered audio … N
  channels` line to pick the width for analysis.

Measured on 2026-09-27 with the CI artifact:

```
Speaker layout: 16 speakers (FL, FR, C, LFE, FWL, FWR, SL, SR, BL, BR, TFL, TFR, TSL, TSR, TBL, TBR)
Processing complete: 47 frames
Label to speaker mapping (by name): {…, Lw: 4, Rw: 5, Tsl: 12, Tsr: 13, Tbl: 14, Tbr: 15}
verdict    : AUDIBLE
```

The `Lw`/`Rw`/`Tsl`/`Tsr`/`Tbl`/`Tbr` entries in that mapping are the observable
difference from the upstream reference bridge, which labels at most 12 channels.

## Feeding through a pipe

`orender` reads a named pipe natively — that is how Omniphony Studio drives its
own engine (`orender render \\.\pipe\orender.input --continuous …`). Everything
else is the producer's job:

```powershell
pwsh -File scripts/pipe-feed-probe.ps1 `
  -BridgePath dist\viola-bridge-windows-x86_64\viola_bridge.dll
```

`scripts/pipe-feed-probe.ps1` creates `\\.\pipe\orender.input`, waits for the
engine to attach, streams a 16-channel WAV into it, and reports the render dump.
`-Realtime` paces the feed to the source's byte rate instead of dumping it as
fast as possible. Only PowerShell/.NET is used, so the probe needs no local
toolchain.

Measured on 2026-09-27 (3 072 114 bytes fed):

```
Decoding stream from file: \\.\pipe\orender.input (presentation: best)
Loading format bridge: …\viola_bridge.dll
Processing complete: 49 frames
Continuous mode: resetting bridge and waiting for new data...
--- render dump: 379920 bytes, 2 channel(s) ---
samples    : 94980 (47490 frames, 2 ch, 0.989s @ 48000 Hz)
verdict    : AUDIBLE
```

Note the engine's own log line `sys::input] Creating Windows named pipe server
(overlapped)`: when the pipe does not exist yet, `orender` creates it and waits
for a client. A producer may therefore either serve the pipe (as the probe does)
or simply connect to it as a client.

With `--continuous` the engine loops back to waiting for the next client after
the stream ends, which is why both probes kill it once the dump has settled.

## Live capture (M3)

`viola_feeder` captures a Windows endpoint through WASAPI and streams it into
`\\.\pipe\orender.input` as a streaming WAV channel bed, so the renderer hears
live audio. **No SDK and no vendor software are involved** — the whole boundary
between what the cloud builds and what this machine may run is written down in
[docs/cloud-boundary.md](docs/cloud-boundary.md).

```powershell
viola_feeder --list                                   # enumerate endpoints
viola_feeder --seconds 10                             # loopback of the default output
viola_feeder --device "Voicemeeter Out B1"            # a virtual cable's output
viola_feeder --device "Voicemeeter Input" --loopback  # what is played into a virtual input
```

The engine has to be running against the same pipe:

```
orender.exe render \\.\pipe\orender.input --continuous --enable-vbap ^
  --speaker-layout "layouts\9.1.6.yaml" --bridge-path "viola_bridge.dll" ^
  --output-backend file --output-file out.f32
```

`orender` creates the pipe itself when it is missing, so the feeder just
connects and retries until that succeeds; it also reconnects (and re-sends the
44-byte header) whenever the engine tears the pipe down between streams.

The format travels in-band: the feeder writes a RIFF/WAVE header whose `data`
size is `0xFFFFFFFF` ("until the input ends") and `viola_bridge` reads the
channel count and sample format from it, so no environment variable has to be
kept in sync between the two processes.

**Channel-count note (deliberate).** The WDM side of a virtual audio device is
stereo, so this path carries 2 channels: it proves the *live chain*
(DAW → virtual device → pipe → bridge → renderer), not 9.1.6's sixteen. Getting
sixteen live channels needed a decision on how to get them into the chain;
[docs/cloud-boundary.md §4](docs/cloud-boundary.md) records it and the routes that
were dropped. **M5 settles it with the ASIO route**, in a separate driver rather
than in the feeder: see [16-channel ASIO device (M5)](#16-channel-asio-device-m5)
below. `viola_feeder` itself stays the 2-channel WASAPI path.

See [docs/cloud-boundary.md](docs/cloud-boundary.md) for the rule that produced
that choice.

## 16-channel ASIO device (M5)

`viola_asio.dll` is a virtual ASIO device that presents **16 inputs and 16
outputs** to a DAW at 48 kHz (44.1 / 48 / 88.2 / 96 kHz accepted; 512-frame
default buffer, 64–2048 accepted) — the sixteen channels M3's WASAPI capture
could not carry.

It **replaces the VB-Audio Matrix route** in
[docs/cloud-boundary.md §4](docs/cloud-boundary.md): that option needs a
third-party mixer installed on the operator machine, and every Windows-visible
endpoint it offers still stops at 8 channels, so 16 channels would have to be
reassembled from two captures. `viola_asio` is built entirely in CI, needs no
other software installed, and hands the DAW's sixteen channels to the same
`\\.\pipe\orender.input` that `viola_feeder` already streams into.
The driver is a user-mode COM in-process server (`DllGetClassObject` /
`DllCanUnloadNow`). The host finds it through the two registry locations that
[docs/asio-driver-notes.md](docs/asio-driver-notes.md) records from the SDK's
`common/register.cpp`. One fixed GUID is both the CLSID and the interface IID,
and is written into the source rather than generated at install time.

### How the sixteen channels reach the pipe

The DAW writes the driver's **output** channels, and the driver copies the half
the host is *not* about to fill (`index ^ 1` of the `bufferSwitch` index — see
[docs/viola-asio-contract.md](docs/viola-asio-contract.md) for why), and the
driver copies that half into a lock-free ring. A pipe thread drains the ring,
writes one 44-byte streaming-WAV header per connection and then interleaved
little-endian `f32` samples in the fixed 16-channel order, so `viola_bridge`
parses it exactly as it parses `viola_feeder`. The driver is the pipe **client**:
`orender` creates `\\.\pipe\orender.input` and waits for it. A block that does
not fit the ring is dropped rather than overwriting samples the reader has not
taken; the driver counts the drops in `%ProgramData%\viola-asio\viola-asio.log`.

### Verifying it locally

`scripts/viola-asio-pipe-probe.ps1` loads the built DLL, drives it through the
ASIO entry points the way a host would, and reads the pipe back. With no engine
running it acts as the pipe server itself, so it needs only the DLL:

```powershell
pwsh -File scripts/viola-asio-pipe-probe.ps1 -DllPath dist\viola-asio-windows-x86_64\viola_asio.dll
```

Pass `-Orender` the engine executable to run the real engine as the server instead
(`-Layout`, `-ToneHz` and `-Seconds` shape the probe tone; `-Channels`
defaults to 16). The script never writes the renderer's config.

### Hearing it

`scripts/live-playout.ps1` closes the loop: the engine's stdout is piped straight
into `ffplay`, so the rendered audio comes out of the default output device and no
growing dump file is left behind. With a DAW open on the ASIO driver the driver is
already the pipe client, and the script only has to run the engine + player:

```powershell
pwsh -File scripts/live-playout.ps1 -NoFeeder            # DAW drives the pipe
pwsh -File scripts/live-playout.ps1 -Seconds 20         # feeder sources it
```

Without `-NoFeeder` it captures the default endpoint (or `-Device`) through
`viola_feeder` and plays a test tone so there is something to hear. Never run both
sources at once: whichever connects second fails with `os error 231`
(`ERROR_PIPE_BUSY`), because the engine serves a single client.

### Installing it

CI publishes the artifact `viola-asio-windows-x86_64` (`viola_asio.dll` +
`SHA256SUMS.txt`):

```powershell
gh run download --name viola-asio-windows-x86_64 --dir dist
```

Registration writes to `HKLM`, so it stays **the operator's decision** — nothing
in CI and no unattended script does it. Both scripts are dry-run by default: they
print every key and path they would touch, write nothing anywhere, create no
directory, and never self-elevate. Only `-Apply` writes, and it needs an elevated
prompt:

```powershell
pwsh -File scripts/register-asio.ps1            # preview: prints the plan
pwsh -File scripts/register-asio.ps1 -Apply     # elevated: DLL + both registry keys
pwsh -File scripts/unregister-asio.ps1          # preview: what would be removed
pwsh -File scripts/unregister-asio.ps1 -Apply   # elevated: backs up to .reg, then removes
```

`register-asio.ps1` copies the DLL to `C:\ProgramData\viola-asio\` and writes
exactly two locations:

- `HKLM\SOFTWARE\Classes\CLSID\{6C1E7D94-3A52-4B8F-9E27-5D0B4C8A1F63}` — the
  description as the key's default value, plus `InprocServer32` holding the
  installed DLL path and `ThreadingModel = Apartment`
- `HKLM\SOFTWARE\ASIO\viola-bridge ASIO` — `Description` and `CLSID`, both REG_SZ

No other key is written. `unregister-asio.ps1` exports every key it is about to
delete to a timestamped `.reg` file under `C:\ProgramData\viola-asio\` before
deleting it, so the removal can be undone; the installed DLL is left on disk.

### ASIO licensing notice

This product displays **ASIO** together with the notice the Steinberg ASIO SDK
licensing agreement requires:

> **ASIO is a trademark and software of Steinberg Media Technologies GmbH**

The SDK is fetched at build time inside CI (`scripts/fetch-asiosdk.ps1`); it is
never committed to this repository and never redistributed in an artifact. The
ASIO logo artwork ships inside the SDK (`Steinberg ASIO Logo Artwork.zip`) and is
not committed here either. The authoritative 2023 licence revision (V2.0.3) has
**not** been read yet — the obligations above are taken from an indicative plain
text of V2.0.1, so they are unverified against the current terms. See
[NOTICE](NOTICE).

## Layout

```
crates/viola_bridge/         the plugin (cdylib)
  src/lib.rs                 root-module export (`format_bridge`)
  src/bridge.rs              FormatBridge impl + label map
  src/pcm.rs                 sample encodings + streaming WAV header scanner
crates/viola_asio/           the virtual ASIO driver (cdylib, M5)
scripts/live-playout.ps1    render straight into ffplay (hear it)
scripts/viola-asio-pipe-probe.ps1  load the DLL and read back the pipe
scripts/bootstrap.ps1        fetch the pinned upstream bridge_api
scripts/fetch-asiosdk.ps1    fetch the pinned ASIO SDK (build time only)
scripts/register-asio.ps1    register the driver (dry run unless -Apply)
scripts/unregister-asio.ps1  remove it, backing up to .reg first
.github/workflows/build.yml
```

## Licence

**GPL-3.0-or-later.** This plugin links `bridge_api`, which is GPL-3.0-or-later,
so the combined work is GPL. It **must not** be linked into a proprietary
program. See [LICENSE](LICENSE) and [NOTICE](NOTICE).

The ASIO SDK is *not* used by `viola_bridge`; it is used by the separate
`viola_asio` driver, which fetches it at build time in CI and never commits or
redistributes it. See the [ASIO licensing
notice](#asio-licensing-notice) and [NOTICE](NOTICE).
