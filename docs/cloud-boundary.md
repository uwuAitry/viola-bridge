# Cloud development boundary

This project is built **entirely in GitHub Actions**. Nothing is compiled,
installed or configured on the operator's machine. This document states the
boundary explicitly so that every future change can be checked against it.

## 1. Who does what

| Environment | Allowed | Not allowed |
|---|---|---|
| **Cloud CI** (`windows-latest` GitHub Actions) | Compile for `x86_64-pc-windows-msvc`; run unit tests; fetch **build-time** dependencies from crates.io and from pinned git revisions; publish workflow artifacts; publish a Release on a tag | Touching any operator machine; using credentials other than the workflow's own `GITHUB_TOKEN`; code signing; committing third-party source into this repository |
| **Operator machine** | Download artifacts (`gh run download`); run `orender`, `ffmpeg`, `python` (standard library only) and `pwsh` for probes; read-only inspection of the installed renderer | Installing any toolchain, SDK, driver or software; modifying the `orender` installation; writing `%ProgramData%\omniphony\config.yaml` |

## 2. Dependency rules

1. **No vendored third-party source.** Omniphony is consumed as a *path*
   dependency into a git-ignored checkout created by `scripts/bootstrap.ps1` at a
   pinned revision; everything else comes from crates.io.
2. **No SDK is committed, and no SDK ships with the artifacts.** A dependency
   that fetches an SDK at *build time inside CI* is acceptable only if the SDK is
   neither stored in this repository nor redistributed in the outputs.
3. Prefer dependencies that are permissive (MIT / Apache-2.0 / ISC / BSD) and
   that do not require an SDK at all.

## 3. What this means for M3 (live capture)

The obvious way to capture a real ASIO device is the Steinberg **ASIO SDK**.
Under rule 2 it is **rejected**, and `asio-sys` is therefore **not** used, even
though it would only ever run inside CI.

Instead M3 captures through **WASAPI** using the [`wasapi`](https://github.com/HEnquist/wasapi-rs)
crate (MIT, pure Rust bindings to Microsoft's `windows` crate). Nothing is
installed on any machine to make this work.

**Consequence, stated plainly:** the WDM side of a virtual audio device is
stereo. So M3 can prove the *live chain* (DAW → virtual device → pipe → bridge →
renderer) but not 9.1.6's sixteen channels. Sixteen channels needs one of the
decisions in §4.

The ASIO hop is still in the signal path when Studio One outputs to a virtual
ASIO driver — the capture simply happens one step later, on the device's WDM
endpoint.

## 4. Open decision: how to reach 16 channels

| Option | What it needs | Cost |
|---|---|---|
| **(a) Allow the ASIO SDK at CI build time** | a dependency that fetches the SDK inside the runner; SDK stays out of the repo and out of the artifacts | captures real ASIO devices directly; the SDK licence terms apply to the build |
| **(b) Allow a multichannel virtual device** | installing VB-Audio Matrix (128×128) on the operator machine | the only option that also works through WASAPI; needs one install, which the boundary currently forbids |
| **(c) Stay stereo** | nothing | no 9.1.6; live chain only |

Until one is chosen, M3 targets (c).

## 5. How this is enforced

- `.github/workflows/build.yml` is the only place that compiles anything.
- `scripts/*.ps1` and `tools/*.py` only *use* already-built artifacts; they never
  build. `tools/` is standard-library-only Python.
- `scripts/verify-channel-bed.ps1` and `scripts/pipe-feed-probe.ps1` read the
  renderer's config but never write it (no `--config`, no `--save-config`).
- `.gitignore` keeps `third_party/` (the pinned upstream checkout) and every
  `dist*` staging directory out of the repository.
