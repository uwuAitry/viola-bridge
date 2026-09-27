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

`.github/workflows/build.yml` runs on `windows-latest` and publishes two
artifacts:

- `viola-bridge-windows-x86_64` — `viola_bridge.dll` + `SHA256SUMS.txt`
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

## Layout

```
crates/viola_bridge/       the plugin (cdylib)
  src/lib.rs               root-module export (`format_bridge`)
  src/bridge.rs            FormatBridge impl + label map
  src/pcm.rs               sample encodings + streaming WAV header scanner
scripts/bootstrap.ps1      fetch the pinned upstream bridge_api
.github/workflows/build.yml
```

## Licence

**GPL-3.0-or-later.** This plugin links `bridge_api`, which is GPL-3.0-or-later,
so the combined work is GPL. It **must not** be linked into a proprietary
program. See [LICENSE](LICENSE) and [NOTICE](NOTICE).

The ASIO SDK is *not* used anywhere in this crate.
