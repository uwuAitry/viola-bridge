# viola-asio skeleton contract

Frozen by the Lead before M5 started, so that the two workstreams (the DLL and
the registration tooling) cannot drift apart. **Both halves must use exactly the
values below.** If something here turns out to be wrong, change this file first
and say so — do not silently diverge.

Background reading: [`docs/asio-driver-notes.md`](asio-driver-notes.md).

## Names

| Thing | Value |
|---|---|
| Cargo crate | `viola_asio` |
| Cargo target name | `viola_asio` (cdylib → `viola_asio.dll`) |
| Registry key under `HKLM\SOFTWARE\ASIO` | `viola-bridge ASIO` |
| `Description` value | `viola-bridge ASIO` |
| CLSID, **also used as the interface IID** | `{6C1E7D94-3A52-4B8F-9E27-5D0B4C8A1F63}` |
| Installed DLL directory | `C:\ProgramData\viola-asio\` |
| Installed DLL path | `C:\ProgramData\viola-asio\viola_asio.dll` |
| Installed panel path | `C:\ProgramData\viola-asio\viola-panel.exe` (optional) |

The GUID is fixed in source (`crates/viola_asio/src/guid.rs`) **and** written
into `scripts/register-asio.ps1`. It is never generated at install time: the host
passes it back to us as `riid`, so CLSID and IID must be the same value forever.

## Defaults the skeleton advertises

| Setting | Default | Notes |
|---|---|---|
| Channels | **16 in / 16 out** | `ASIOGetChannels`; the point of the whole exercise |
| Buffer size | 512 frames, min 64, max 2048, granularity **-1** | `ASIOGetBufferSize`; asio.h reserves `0` for "min == max" and uses `-1` to mean "power-of-two sizes from min to max" |
| Sample rate | 48000 | `ASIOGetSampleRate` / `canSampleRate` / `setSampleRate` accept 44100/48000/88200/96000 |
| Pipe | `\\.\pipe\orender.input` | same destination `viola_feeder` already writes to |

## File layout expected by both halves

```
crates/viola_asio/
  Cargo.toml
  src/lib.rs        DllMain-ish entry points: DllGetClassObject, DllCanUnloadNow
  src/guid.rs       the fixed GUID, as bytes and as a `GUID` struct
  src/factory.rs    IClassFactory vtable
  src/driver.rs     the IASIO vtable (21 methods) + IUnknown
  src/ffi.rs        #[repr(C)] mirrors of the SDK's plain-C ASIO types
  src/ring.rs       the lock-free SPSC sample ring (M5.3)
  src/pipe.rs       streaming-WAV header + the pipe client thread (M5.3)
scripts/
  register-asio.ps1     installs the DLL + writes both registry locations
                        (with -PanelExe: also stages viola-panel.exe beside it)
  unregister-asio.ps1   removes them, backing up to .reg first
```

The registration scripts take the DLL from a `-DllPath` parameter whose default
is `..\dist\viola-asio-windows-x86_64\viola_asio.dll` (where the CI artifact
lands) and copy it to the installed path above.

## House rules (both halves)

* **No git commands.** The Lead commits and pushes; a shared working tree plus
  parallel `git add` is how indexes get corrupted.
* **No writes to `HKLM`.** `register-asio.ps1` must be dry-run by default and
  only touch the registry with an explicit `-Apply`, because installing a driver
  is the operator's decision, not ours.
* **No local toolchain installs** and no new system dependencies — see
  [`docs/cloud-boundary.md`](cloud-boundary.md).
* No `windows` crate for the skeleton: the ASIO vtable has to be hand-rolled
  anyway, so the factory and `IUnknown` are hand-rolled too, keeping the
  dependency list at zero.
* Every claim in a comment that states a fact about the SDK should name the file
it came from (`common/iasiodrv.h`, `host/pc/asiolist.cpp`, …).

## `controlPanel()` — the host's control-panel button

`driver.rs` answers `controlPanel()` (slot 18) by launching the panel instead of
returning a bare error:

* The panel is looked for as `viola-panel.exe` **in this DLL's own directory**,
  which is why the installer stages it beside `viola_asio.dll`. The DLL path comes
  from `GetModuleHandleExW` (by address) + `GetModuleFileNameW`; asking about
  `NULL` would answer with the *host's* executable, not ours.
* Nothing is built in-process. `ShellExecuteW` is the only way across, since the
  panel is a separate program with a single-instance mutex of its own.
* A panel that is already up is raised via `FindWindowW("viola-panel")` +
  `ShowWindow(SW_RESTORE)` + `SetForegroundWindow`, because a second launch would
  exit at once and look like the button did nothing.
* `ASE_OK` / `ASE_NotPresent` report only whether the shell accepted the request.
  asio.h states the host **ignores** the return code, so this value never decided
  whether the button was greyed out — it only ever meant "nothing opened".

Still hand-rolled FFI, still no crates: `shell32` and `user32` are linked for
`ShellExecuteW` and the window calls, alongside the existing `kernel32` block.

## M5.3 data path — from the host's buffers to the pipe

This section freezes what `crates/viola_asio` does while `start()` is running:
where the channel bed comes from, which half of the double buffer holds it, what
bytes go into `\\.\pipe\orender.input`, and what happens when the reader falls
behind. SDK quotations are from `third_party/asiosdk/common/asio.h`.

### Direction: a DAW writes the driver's output channels

A DAW writing to this driver writes the driver's **output** channels, i.e.
`ASIOBufferInfo::isInput == ASIO_FALSE` (`common/asio.h`: "isInput: on input:
ASIOTrue: input, else output"). The driver's *input* channels
(`isInput == ASIO_TRUE`) are the direction the driver writes and the host reads;
M5.3 does not use them.

`ASIOBufferInfo::buffers[2]` receives the channel's double-buffered addresses in
`createBuffers` (`common/asio.h`). `buffers[0]` is the first half and
`buffers[1] = buffers[0] + buffer_size` frames, the split the SDK sample also
makes (`driver/asiosample/asiosmpl.cpp`).

### Which half: `index ^ 1`

`bufferSwitch(doubleBufferIndex, directProcess)`'s index is the half the host is
*about to fill*, not the one it has just filled. `common/asio.h` says the index
"determines … the output buffer that the host should start to fill. the other
buffer will be passed to output hardware regardless of whether it got filled in
time or not." So the completed half is `index ^ 1`, and that is the half copied
into the ring.

### Wire format

The pipe carries the same streaming WAV that `viola_feeder` already produces
(`crates/viola_feeder/src/wav.rs`), so `viola_bridge` parses it with no changes:

| Field | Value |
|---|---|
| Header | 44-byte RIFF/WAVE, written once per connection |
| Format code | `WAVE_FORMAT_IEEE_FLOAT` (3), 32-bit samples |
| RIFF size and `data` size | both `u32::MAX` — "until the input ends" |
| Sample bytes | interleaved little-endian `f32` |
| Channels | fixed 16, in ascending `channelNum` order |

A channel the host did not activate contributes silence; the frame width never
changes with the host's selection.

### Ring sizing and the drop policy

The ring holds a power-of-two number of samples with roughly eight blocks of
headroom — about 85 ms at the 512-frame default and 48 kHz.
`Ring::write_block` is all-or-nothing: when the whole block does not fit, nothing
is written, the block is dropped, and the `dropped_blocks` counter — which lives
beside the ring in `Pipeline` (`crates/viola_asio/src/pipe.rs`) — is incremented.
The ring never overwrites samples the pipe
thread has not taken, so a slow reader loses audio instead of having it
corrupted under it.

The driver is the pipe **client**: `orender` creates `\\.\pipe\orender.input`
and waits for a client (`README.md`, `crates/viola_feeder/src/pipe.rs`), so the
driver opens it for writing and retries when the engine tears it down. After
every connect it discards the ring's backlog and writes the 44-byte header first,
so a stalled start cannot accumulate latency.

### Log line

The driver appends one status line a second to
`%ProgramData%\viola-asio\viola-asio.log`. As of M5.3 that line also carries a
local-time stamp (`GetLocalTime`) and the pipeline counters from
`crates/viola_asio/src/pipe.rs` (`dropped_blocks`, `samples_written`,
`connected`).
