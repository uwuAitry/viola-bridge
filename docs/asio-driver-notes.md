# Notes for `viola_asio` — how an ASIO driver is built and registered

Collected 2026-09-28 while preparing M5. Everything below is either read out of the
**pinned ASDK** (`scripts/fetch-asiosdk.ps1` → `third_party/asiosdk`, SDK 2.3.3,
SHA1 verified) or read off this machine. The sample driver shipped inside the SDK
is the reference implementation; the plan is to port its structure to Rust, not
to invent one.

## 1. An ASIO driver is a user-mode COM in-process server

The host side of the SDK says it outright:

* `host/pc/asiolist.cpp` — `instantiates an ASIO driver via the COM model` (SDK readme)
* `host/pc/asiolist.cpp:227`:

  ```c
  rc = CoCreateInstance(lpdrv->clsid, 0, CLSCTX_INPROC_SERVER, lpdrv->clsid, asiodrv);
  ```

  Note the fourth argument: **the CLSID is passed as the interface IID**. The
  sample confirms the convention — one GUID is both:

  ```c
  CLSID IID_ASIO_DRIVER = { 0x188135e1, 0xd565, 0x11d2, { 0x85,0x4f,0x0,0xa0,0xc9,0x9f,0x5d,0x19 } };
  { L"ASIOSAMPLE", &IID_ASIO_DRIVER, AsioSample::CreateInstance }   // COM template
  STDMETHODIMP AsioSample::NonDelegatingQueryInterface(REFIID riid, void **ppv) {
      if (riid == IID_ASIO_DRIVER) return GetInterface(this, ppv);
      return CUnknown::NonDelegatingQueryInterface(riid, ppv);
  }
  ```

Consequences for us: the driver object must answer `QueryInterface` for
`IID_IUnknown` **and** for a custom IID equal to our own CLSID.

### Required DLL exports

`driver/asiosample/asiosample.def`:

```
DllMain
DllGetClassObject
DllCanUnloadNow
DllRegisterServer
DllUnregisterServer
```

`DllGetClassObject` must accept `IID_IUnknown` / `IID_IClassFactory` and hand back
a class factory whose `CreateInstance` calls the driver's create function and then
`NonDelegatingQueryInterface(riid)` (`common/dllentry.cpp`).

## 2. Registration (exactly two places)

From `common/register.cpp`, and confirmed against two working drivers on this
machine:

| Key | Value |
|---|---|
| `HKCR\CLSID\{GUID}` | default = the driver description |
| `HKCR\CLSID\{GUID}\InprocServer32` | default = **full path to the DLL** |
| `HKCR\CLSID\{GUID}\InprocServer32\ThreadingModel` | `Apartment` |
| `HKLM\SOFTWARE\ASIO\<registered name>` | `Description` = REG_SZ, `CLSID` = REG_SZ |

Local samples (`HKLM\SOFTWARE\ASIO`):

```
ASIO4ALL v2                 CLSID {232685C6-6548-49D8-846D-4141A3EF7560}
                            → C:\Program Files (x86)\ASIO4ALL v2\asio4all64.dll
Voicemeeter Virtual ASIO    CLSID {9175CF07-885D-46B4-9EA1-4126D6648DE6}
                            → c:\program files (x86)\vb\voicemeeter\vbvm_asiodriver64.dll
DSDTranscoder               CLSID {B9D4285C-EF06-4E3A-8080-851904C667FB}
```

Both hives carry the entries (the 32-bit copies live under `WOW6432Node`); a
64-bit-only driver only needs the plain ones. `DllRegisterServer` /
`DllUnregisterServer` do this in the SDK sample; we will do it from an elevated
PowerShell script instead, so the DLL stays passive.

## 3. The interface to implement

`common/iasiodrv.h` — `interface IASIO : public IUnknown`, 21 methods, **in this
order** (the vtable order is the ABI; getting it wrong crashes the host):

```
init, getDriverName, getDriverVersion, getErrorMessage,
start, stop,
getChannels, getLatencies, getBufferSize,
canSampleRate, getSampleRate, setSampleRate,
getClockSources, setClockSource,
getSamplePosition, getChannelInfo,
createBuffers, disposeBuffers,
controlPanel, future, outputReady
```

The buffer handshake lives in `createBuffers(ASIOBufferInfo*, long numChannels,
long bufferSize, ASIOCallbacks*)`: the host hands over the (double-buffered)
channel pointers and the callbacks; `start()` then begins the cadence.

`common/asio.h` and `common/asiosys.h` carry the plain-C types
(`ASIOBufferInfo`, `ASIOCallbacks`, `ASIOChannelInfo`, `ASIOClockSource`,
`ASIOSamples`, `ASIOTime`, `ASIOError`, `ASIOBool`, …) — our Rust side declares
`#[repr(C)]` mirrors of exactly those.

## 4. The clock is ours

A virtual device has no hardware clock: the driver must call the host's callbacks
on its own timer. The SDK sample does the simplest possible thing
(`driver/asiosample/wintimer.cpp`):

```c
ASIOThreadHandle = CreateThread(0, 0, &ASIOThread, 0, 0, &asioId);
...
theDriver->bufferSwitch();
Sleep(theDriver->getMilliSeconds());
```

`Sleep`-based pacing is not good enough for us (coarse granularity, no period
control). Plan: a waitable timer (`CreateWaitableTimerExW` with
`CREATE_WAITABLE_TIMER_HIGH_RESOLUTION`) on a thread raised to
`THREAD_PRIORITY_TIME_CRITICAL`, with `timeBeginPeriod(1)` for the process, and
the sample position published from that thread.

## 5. Licence

The SDK zip ships `Steinberg ASIO 2.3.3 Licensing Agreement V2.0.3 - 2023.pdf`.
Its text layer is CID-encoded and **could not be extracted with standard-library
tooling**, so the terms below come from a plain-text copy of **V2.0.1 (same SDK
2.3.3, 2019)** obtained from a third-party project — *indicative, not the
authoritative revision*.

What V2.0.1 grants and requires:

* **§2.1** — a non-exclusive, worldwide, non-transferable licence to use the SDK
  (a) to develop ASIO device drivers and (b) to **publish, sell or otherwise
  distribute** the resulting product **under one's own brand name**.
* **§3** — you must display **"ASIO" plus Steinberg's copyright notice**, or the
  **ASIO compatible Logo** plus that notice, on the product website, in the
  documentation, and in an About box or splash screen (at least one piece of
  documentation must ship with the product). The notice text is exactly:
  *“ASIO is a trademark and software of Steinberg Media Technologies GmbH”*.
  Logo use must follow the artwork/guidelines in the SDK (`Steinberg ASIO Logo
  Artwork.zip`). No ASIO marks on merchandise, on obscene/violent products, or on
  anything not ASIO-compatible.
* **§3.1.k** — a trademark-use documentation duty, **waived** while the product
  has fewer than 1000 users.
* **No fees or royalties** (§4).
* **§1.2** — a preview/beta SDK may only be used for internal evaluation.
* **Forbidden**: publishing/selling/distributing a *modified* SDK; and
  *“re-working any part of the SDK or ASIO specification (such as ASIO API, ASIO
  API calling sequences, …) or reverse-engineering any part of the SDK”*.

**The last clause matters for a decision we had left open.** It makes the
"clean-room reimplementation of the ASIO interface" option unattractive: the
agreement appears to cover reworking the API/calling sequences, not just copying
files. The licensed path — fetch the SDK in CI, never commit it, never
redistribute it, comply with §3 notices — is the one to take.

### Outstanding

* **Read `…Licensing Agreement V2.0.3 - 2023.pdf` properly** (2 pages; a PDF
  viewer is enough) and confirm the 2023 revision did not change §2/§3 in a way
  that affects us. Everything in the milestone depends on this being right.

## 6. What this means for viola-asio

* One fixed GUID, used as CLSID **and** IID; written into the source, not
  generated per install.
* `viola_asio.dll` exports **two** names, not the sample's five:
  `DllGetClassObject` and `DllCanUnloadNow`. `DllMain` belongs to the CRT (std
  already provides it for an MSVC cdylib, so defining our own would be a duplicate
  symbol), and `DllRegisterServer`/`DllUnregisterServer` are replaced by the
  PowerShell scripts. The driver object implements the 21-method `IASIO` vtable
  plus `IUnknown`.
* Registration is done by `scripts/register-asio.ps1` (elevated), not by
  `DllRegisterServer` — easier to audit and to reverse.
* The callback thread only ever `memcpy`s into a lock-free SPSC ring; a separate
  thread writes the streaming-WAV bytes into `\\.\pipe\orender.input`.
* Notices per §3 go into the README and `NOTICE`; the ASIO logo artwork comes from
  the SDK (`Steinberg ASIO Logo Artwork.zip`) and is **not** committed — the
  build/README point at the SDK copy, or we ship the notice text alone if the
  logo proves awkward inside a GPL repo.

## 7. Follow-ups

* **Independent ABI verification.** `crates/viola_asio/src/ffi.rs` mirrors the SDK
  structs as `#[repr(C, packed(4))]` and pins every size and offset in a test. The
  first draft of that test computed `ASIOCallbacks` as 16 bytes at 0/4/8/12 - the
  32-bit layout - and CI caught it (the real layout is 32 bytes at 0/8/16/24). That
  is a hand-computed expectation being checked against a hand-written mirror, so
  the next rigour step is a probe compiled by **MSVC itself**
  (`cl.exe` + the SDK's `asio.h`), printing `sizeof`/`alignof`/`offsetof`, and a CI
  step that diffs its output against the Rust values. Two independent compilers
  agreeing is proof; the current test is only a guard against drift.
* **Export verification.** LNK4104 fires on the two exported names. The linker
  still produces the DLL, but the export table should be read back from the built
  artifact rather than assumed - a COM server whose `DllGetClassObject` is not
  exported fails in a way that looks like a registration problem.
