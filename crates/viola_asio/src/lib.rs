// SPDX-License-Identifier: GPL-3.0-or-later
//! `viola_asio.dll` — the viola-bridge ASIO driver.
//!
//! An ASIO driver is a COM in-process server (`host/pc/asiolist.cpp`:
//! "instantiates an ASIO driver via the COM model"), so this crate exports the
//! two entry points COM asks for and nothing else:
//!
//! | export | purpose |
//! |---|---|
//! | `DllGetClassObject` | hand out the `IClassFactory` for our CLSID |
//! | `DllCanUnloadNow` | say whether any object is still alive |
//!
//! **No `DllMain` is exported, on purpose.** `DllMain` is the CRT's own
//! initialiser: a Rust `cdylib` is linked against a CRT that already provides
//! one, and that is where the Rust runtime, the thread stack bookkeeping and
//! `std`'s Windows machinery are set up. A second `DllMain` in our source would
//! be a duplicate symbol at best and, at worst, code of ours running before the
//! runtime it depends on exists. The SDK's sample lists `DllMain` in
//! `driver/asiosample/asiosample.def` because for an MSVC DLL the *program* is
//! expected to supply it; Rust is not that program, so this file supplies the
//! two exports COM actually needs.
//!
//! `DllRegisterServer`/`DllUnregisterServer` are also not exported, even though
//! `common/register.cpp` implements them for the sample:
//! `docs/viola-asio-contract.md` moves registration to
//! `scripts/register-asio.ps1`, which is easier to audit and to reverse.
//!
//! Module layout is the one the contract fixes: [`guid`] (the single fixed GUID),
//! [`ffi`] (the plain-C type mirrors), [`driver`] (the 21-method `IASIO` vtable),
//! [`factory`] (`IClassFactory`), and this file (the COM entry points plus the
//! contract's numeric defaults, so both halves read the same literals).
//!
//! Nothing on a host-controlled path allocates or panics: this code runs inside
//! the DAW's process, so a bug of ours is a crash of theirs.

mod driver;
mod factory;
mod ffi;
// `Guid` appears in the signature of the exported `DllGetClassObject`, so the
// type has to be reachable from outside the crate; otherwise rustc warns
// (`private_interfaces`) and the export's ABI reads as if it used a private type.
pub mod guid;

use core::ffi::c_void;
use core::sync::atomic::{AtomicUsize, Ordering};

use crate::factory::ClassFactory;
use crate::ffi::{CLASS_E_CLASSNOTAVAILABLE, E_NOINTERFACE, E_POINTER, HResult, S_FALSE, S_OK};
use crate::guid::{guid_ref, Guid, CLSID_VIOLA_ASIO, IID_ICLASSFACTORY, IID_IUNKNOWN};

/// The name the host shows in its device list. The contract uses this same
/// literal for the driver name, the registry `Description` value, and the
/// `HKLM\SOFTWARE\ASIO` key name.
pub(crate) const DRIVER_NAME: &str = "viola-bridge ASIO";

/// Sample rate reported before the host picks one (`getSampleRate`).
pub(crate) const DEFAULT_SAMPLE_RATE: f64 = 48_000.0;

/// The rates `canSampleRate`/`setSampleRate` accept, per the contract. All four
/// are exactly representable as `f64`, so hosts pass them literally and an exact
/// comparison is the same test the SDK sample makes.
pub(crate) const SUPPORTED_SAMPLE_RATES: [f64; 4] = [44_100.0, 48_000.0, 88_200.0, 96_000.0];

/// Channels advertised in each direction by `getChannels`: 16 in / 16 out, the
/// point of the whole exercise.
pub(crate) const CHANNEL_COUNT: i32 = 16;

/// The buffer-size window the contract freezes: 64..=2048 frames, 512 preferred,
/// The buffer-size window the contract freezes: 64..=2048 frames, 512 preferred.
/// `getBufferSize` reports exactly these.
///
/// The granularity is `-1`, not `0`: asio.h documents `0` as "minSize == maxSize"
/// and `-1` as "power-of-two sizes from min to max". The contract was corrected
/// after the first draft; see docs/viola-asio-contract.md.
pub(crate) const MIN_BUFFER_SIZE: i32 = 64;
pub(crate) const MAX_BUFFER_SIZE: i32 = 2048;
pub(crate) const DEFAULT_BUFFER_SIZE: i32 = 512;
pub(crate) const BUFFER_SIZE_GRANULARITY: i32 = -1;

/// Where the buffer thread will stream the channel bed once M5.1b implements
/// `createBuffers`: the same named pipe `viola_feeder` already writes to
/// (`docs/viola-asio-contract.md`). Not referenced by any code yet — it is kept
/// here so the destination lives with the other contract defaults instead of
/// being re-typed from memory later.
#[allow(dead_code)]
pub(crate) const PIPE_PATH: &str = r"\\.\pipe\orender.input";

/// COM objects handed out and not yet released — factories and drivers both.
/// `DllCanUnloadNow` answers `S_FALSE` while this is non-zero.
pub(crate) static LIVE_OBJECTS: AtomicUsize = AtomicUsize::new(0);

/// Outstanding `IClassFactory::LockServer(TRUE)` calls; same job as
/// `LIVE_OBJECTS`, but for the host pinning us rather than holding an object.
pub(crate) static SERVER_LOCKS: AtomicUsize = AtomicUsize::new(0);

/// Is `rate` one of the four rates the contract supports?
pub(crate) fn rate_is_supported(rate: f64) -> bool {
    SUPPORTED_SAMPLE_RATES.contains(&rate)
}

// --- COM entry points ------------------------------------------------------
//
// (No `DllMain` and no `DllRegisterServer` here — see the module documentation.)

/// `DllGetClassObject(REFCLSID rclsid, REFIID riid, void **ppv)`.
///
/// The host reaches us through `CoCreateInstance` with the CLSID it read from
/// `HKLM\SOFTWARE\ASIO`, and COM then asks for `IID_IUnknown` or
/// `IID_IClassFactory` — exactly the two `common/dllentry.cpp` accepts:
///
/// ```c
/// if (!(riid == IID_IUnknown) && !(riid == IID_IClassFactory)) return E_NOINTERFACE;
/// ```
///
/// A CLSID that is not ours is `CLASS_E_CLASSNOTAVAILABLE`, as there too.
///
/// Two contract details are load-bearing. The CLSID is compared against the one
/// compiled into `guid.rs`, never generated at install time. And `*ppv` is
/// nulled on *every* failure path, because COM callers check the pointer, not
/// only the `HRESULT`.
#[unsafe(no_mangle)]
pub extern "system" fn DllGetClassObject(
    rclsid: *const Guid,
    riid: *const Guid,
    ppv: *mut *mut c_void,
) -> HResult {
    if ppv.is_null() {
        return E_POINTER;
    }
    // Safety: `ppv` was just checked to be non-null.
    unsafe { *ppv = core::ptr::null_mut() };

    let Some(clsid) = (unsafe { guid_ref(rclsid) }) else {
        return E_POINTER;
    };
    if *clsid != CLSID_VIOLA_ASIO {
        return CLASS_E_CLASSNOTAVAILABLE;
    }

    let Some(riid) = (unsafe { guid_ref(riid) }) else {
        return E_POINTER;
    };
    if *riid != IID_IUNKNOWN && *riid != IID_ICLASSFACTORY {
        return E_NOINTERFACE;
    }

    // One reference, which is now COM's.
    let factory = Box::into_raw(ClassFactory::new_boxed());
    // Safety: `ppv` is non-null and valid for a write by COM's contract.
    unsafe { *ppv = factory as *mut c_void };
    S_OK
}

/// `DllCanUnloadNow()`.
///
/// `S_OK` only when nothing of ours is alive: no object outstanding and no
/// `LockServer(TRUE)` still in force (`common/dllentry.cpp` checks both).
/// `S_FALSE` otherwise, which merely means the host keeps the DLL mapped.
#[unsafe(no_mangle)]
pub extern "system" fn DllCanUnloadNow() -> HResult {
    if LIVE_OBJECTS.load(Ordering::Acquire) == 0 && SERVER_LOCKS.load(Ordering::Acquire) == 0 {
        S_OK
    } else {
        S_FALSE
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::factory::factory_release;
    use core::ptr;

    #[test]
    fn dll_get_class_object_hands_out_a_factory_for_our_clsid() {
        let mut out: *mut c_void = ptr::null_mut();
        let hr = DllGetClassObject(&CLSID_VIOLA_ASIO, &IID_ICLASSFACTORY, &mut out);
        assert_eq!(hr, S_OK);
        assert!(!out.is_null());
        // Drop the reference DllGetClassObject handed over.
        assert_eq!(unsafe { factory_release(out as *mut ClassFactory) }, 0);
    }

    #[test]
    fn dll_get_class_object_accepts_iid_iunknown_too() {
        let mut out: *mut c_void = ptr::null_mut();
        let hr = DllGetClassObject(&CLSID_VIOLA_ASIO, &IID_IUNKNOWN, &mut out);
        assert_eq!(hr, S_OK);
        assert!(!out.is_null());
        assert_eq!(unsafe { factory_release(out as *mut ClassFactory) }, 0);
    }

    #[test]
    fn dll_get_class_object_rejects_a_foreign_clsid() {
        let foreign = Guid {
            data1: 0x2326_85C6,
            data2: 0x6548,
            data3: 0x49D8,
            data4: [0x84, 0x6D, 0x41, 0x41, 0xA3, 0xEF, 0x75, 0x60],
        };
        let mut out: *mut c_void = ptr::null_mut();
        let hr = DllGetClassObject(&foreign, &IID_IUNKNOWN, &mut out);
        assert_eq!(hr, CLASS_E_CLASSNOTAVAILABLE);
        assert!(out.is_null());
    }

    #[test]
    fn dll_get_class_object_rejects_a_bogus_riid() {
        let bogus = Guid {
            data1: 0x0000_FFFF,
            data2: 1,
            data3: 2,
            data4: [3; 8],
        };
        let mut out: *mut c_void = ptr::null_mut();
        let hr = DllGetClassObject(&CLSID_VIOLA_ASIO, &bogus, &mut out);
        assert_eq!(hr, E_NOINTERFACE);
        assert!(out.is_null());
    }

    #[test]
    fn dll_get_class_object_rejects_null_pointers() {
        let mut out: *mut c_void = ptr::null_mut();

        assert_eq!(
            DllGetClassObject(ptr::null(), &IID_IUNKNOWN, &mut out),
            E_POINTER
        );
        assert_eq!(
            DllGetClassObject(&CLSID_VIOLA_ASIO, ptr::null(), &mut out),
            E_POINTER
        );
        // A null `ppv` has nowhere to report the class, so it is rejected before
        // the CLSID is even looked at.
        assert_eq!(
            DllGetClassObject(&CLSID_VIOLA_ASIO, &IID_IUNKNOWN, ptr::null_mut()),
            E_POINTER
        );
        assert!(out.is_null());
    }

    #[test]
    fn dll_can_unload_now_answers_with_one_of_the_two_hresults() {
        // `LIVE_OBJECTS` is process-wide while the other tests in this binary run
        // in parallel, so the exact answer is not assertable here — this only
        // proves the export is callable and returns a documented value.
        let hr = DllCanUnloadNow();
        assert!(hr == S_OK || hr == S_FALSE, "unexpected HRESULT {hr:#x}");
    }
}
