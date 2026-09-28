# viola-asio skeleton contract

Frozen by the Lead before M5.1 started, so that the two workstreams (the DLL and
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
scripts/
  register-asio.ps1     installs the DLL + writes both registry locations
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
