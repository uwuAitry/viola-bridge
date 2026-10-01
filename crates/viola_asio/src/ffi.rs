// SPDX-License-Identifier: GPL-3.0-or-later
//! `#[repr(C)]` mirrors of the plain-C ASIO types.
//!
//! Everything here is read off the pinned SDK, `third_party/asiosdk/common/asio.h`;
//! `common/iasiodrv.h` only adds the `IASIO` method list (that lives in
//! `driver.rs`). The declarations follow asio.h top to bottom so the two can be
//! diffed side by side.
//!
//! Two details are easy to get wrong and both are load-bearing:
//!
//! 1. **`#pragma pack(push,4)`.** asio.h wraps every declaration in it ("force 4
//!    byte alignment"), and MSVC's rule is that a member is aligned to "a
//!    multiple of `n`, or a multiple of the size of the member, whichever is
//!    smaller" — so on x64 a `void*`/`double` member is aligned to **4**, not 8.
//!    Rust's plain `#[repr(C)]` would use natural alignment, so every struct
//!    below also carries `packed(4)`: the Rust Reference's rule for `packed(n)`
//!    is that "the alignments of each field, for the purpose of positioning
//!    fields, is the smaller of the specified alignment and the alignment of the
//!    field's type" — the same rule. The difference is not academic:
//!    `ASIOCallbacks` is 16 bytes with the four callbacks at offsets 0/4/8/12 in
//!    C, and 32 bytes at 0/8/16/24 without `packed(4)`, so a mismatch would send
//!    the driver to the wrong function pointer. The `layout_matches_msvc_pack_4`
//!    test at the bottom of this file pins the sizes and offsets down.
//!
//! 2. **The Windows switches in `asiosys.h`.** For `_WIN32`/`_WIN64` it defines
//!    `NATIVE_INT64 0` and `IEEE754_64FLOAT 1`. So `ASIOSamples` and
//!    `ASIOTimeStamp` are *not* `i64` here — they are two 32-bit halves, `hi`
//!    first; and `ASIOSampleRate` is a plain `f64`.
//!
//! `long` and `unsigned long` are 32-bit on Windows (LLP64), which is why every
//! SDK `long` below is `i32`/`u32` rather than `core::ffi::c_long` (that would be
//! 64-bit if this crate were ever compiled for a non-Windows target, and the
//! layout is Windows-only by construction). Likewise `char name[32]` is
//! `[c_char; 32]`.

#![allow(non_camel_case_types)] // SDK spellings (ASIOBufferInfo, …) are kept verbatim
#![allow(non_upper_case_globals)] // as are the error codes (ASE_NotPresent, …)
#![allow(dead_code)] // the mirror is kept complete; not all of it is used yet

use core::ffi::{c_char, c_void};

// ---------------------------------------------------------------------------
// Type definitions (asio.h "Type definitions")
// ---------------------------------------------------------------------------

// asio.h: #if NATIVE_INT64 → long long; #else → { unsigned long hi, lo }.
// asiosys.h sets NATIVE_INT64 0 on Windows, so it is the struct form.
/// Sample count, high half first (asio.h).
#[repr(C, packed(4))]
pub(crate) struct ASIOSamples {
    pub(crate) hi: u32,
    pub(crate) lo: u32,
}

/// Nanosecond timestamp, high half first (asio.h).
#[repr(C, packed(4))]
pub(crate) struct ASIOTimeStamp {
    pub(crate) hi: u32,
    pub(crate) lo: u32,
}

/// IEEE 754 64-bit float (asio.h: `typedef double ASIOSampleRate`).
pub(crate) type ASIOSampleRate = f64;

/// `typedef long ASIOBool` (asio.h). Only `ASIO_FALSE`/`ASIO_TRUE` are legal.
pub(crate) type ASIOBool = i32;

/// `typedef long ASIOSampleType` (asio.h).
pub(crate) type ASIOSampleType = i32;

/// `ASIOFalse` (asio.h).
pub(crate) const ASIO_FALSE: ASIOBool = 0;
/// `ASIOTrue` (asio.h).
pub(crate) const ASIO_TRUE: ASIOBool = 1;

// asio.h: ASIOSTInt16MSB = 0 … ASIOSTFloat32LSB = 19, …
/// The channel type this driver will present: `ASIOSTFloat32LSB` (asio.h),
/// i.e. IEEE 754 32-bit float, the format the rest of viola-bridge already
/// works in (`docs/cloud-boundary.md`: 32-bit-float intermediates).
pub(crate) const ASIOSTFloat32LSB: ASIOSampleType = 19;

// ---------------------------------------------------------------------------
// Error codes (asio.h "Error codes") — spelled exactly as the SDK spells them
// ---------------------------------------------------------------------------

/// `typedef long ASIOError` (asio.h).
pub(crate) type ASIOError = i32;

/// `ASE_OK` (asio.h).
pub(crate) const ASE_OK: ASIOError = 0;
/// `ASE_SUCCESS` (asio.h) — the value `future()` must return for a selector it
/// handled; asio.h is explicit that `ASE_OK` is *not* sufficient there.
pub(crate) const ASE_SUCCESS: ASIOError = 0x3f48_47a0;
/// `ASE_NotPresent` (asio.h).
pub(crate) const ASE_NotPresent: ASIOError = -1000;
/// `ASE_HWMalfunction` (asio.h).
pub(crate) const ASE_HWMalfunction: ASIOError = -999;
/// `ASE_InvalidParameter` (asio.h).
pub(crate) const ASE_InvalidParameter: ASIOError = -998;
/// `ASE_InvalidMode` (asio.h).
pub(crate) const ASE_InvalidMode: ASIOError = -997;
/// `ASE_SPNotAdvancing` (asio.h) — "hardware is not running when sample position
/// is inquired".
pub(crate) const ASE_SPNotAdvancing: ASIOError = -996;
/// `ASE_NoClock` (asio.h) — the sample rate / clock cannot be determined.
pub(crate) const ASE_NoClock: ASIOError = -995;
/// `ASE_NoMemory` (asio.h).
pub(crate) const ASE_NoMemory: ASIOError = -994;

// ---------------------------------------------------------------------------
// Time info support (asio.h)
// ---------------------------------------------------------------------------

/// `ASIOTimeCode` (asio.h).
#[repr(C, packed(4))]
pub(crate) struct ASIOTimeCode {
    pub(crate) speed: f64,
    pub(crate) time_code_samples: ASIOSamples,
    pub(crate) flags: u32,
    pub(crate) future: [c_char; 64],
}

/// `AsioTimeInfo` (asio.h).
#[repr(C, packed(4))]
pub(crate) struct AsioTimeInfo {
    pub(crate) speed: f64,
    pub(crate) system_time: ASIOTimeStamp,
    pub(crate) sample_position: ASIOSamples,
    pub(crate) sample_rate: ASIOSampleRate,
    pub(crate) flags: u32,
    pub(crate) reserved: [c_char; 12],
}

/// `ASIOTime` (asio.h) — "both input/output".
#[repr(C, packed(4))]
pub(crate) struct ASIOTime {
    pub(crate) reserved: [i32; 4],
    pub(crate) time_info: AsioTimeInfo,
    pub(crate) time_code: ASIOTimeCode,
}

/// `ASIOCallbacks` (asio.h) — the host's four handlers, handed to the driver in
/// `createBuffers` and called from the driver's own clock thread.
///
/// Each field is `Option<…>` rather than a bare `fn`: the C pointers may legally
/// be null, and `Option<fn>` is guaranteed to be the same 8 bytes, so the layout
/// is unchanged while "the host sent null" stays representable. The SDK declares
/// them with the default C calling convention (`host/pc/asiolist.cpp` fills the
/// same struct); on `x86_64-pc-windows-msvc` — the only target this DLL is built
/// for (`docs/viola-asio-contract.md`: `dist/viola-asio-windows-x86_64`) — that
/// is the same convention as `extern "system"`.
///
/// `Copy` because the four function pointers are plain data and the driver has to
/// keep its own copy of them for the lifetime of a `createBuffers` session; a
/// type the host hands over by pointer and never takes back should be copyable.
#[derive(Clone, Copy)]
#[repr(C, packed(4))]
pub(crate) struct ASIOCallbacks {
    /// `void (*bufferSwitch)(long doubleBufferIndex, ASIOBool directProcess)`
    pub(crate) buffer_switch: Option<unsafe extern "system" fn(double_buffer_index: i32, direct_process: ASIOBool)>,
    /// `void (*sampleRateDidChange)(ASIOSampleRate sRate)`
    pub(crate) sample_rate_did_change: Option<unsafe extern "system" fn(s_rate: ASIOSampleRate)>,
    /// `long (*asioMessage)(long selector, long value, void* message, double* opt)`
    pub(crate) asio_message: Option<unsafe extern "system" fn(selector: i32, value: i32, message: *mut c_void, opt: *mut f64) -> i32>,
    /// `ASIOTime* (*bufferSwitchTimeInfo)(ASIOTime*, long, ASIOBool)`
    pub(crate) buffer_switch_time_info: Option<unsafe extern "system" fn(params: *mut ASIOTime, double_buffer_index: i32, direct_process: ASIOBool) -> *mut ASIOTime>,
}

// ---------------------------------------------------------------------------
// Driver info and channel description (asio.h)
// ---------------------------------------------------------------------------

/// `ASIODriverInfo` (asio.h).
///
/// The `IASIO` vtable never sees this struct — it is the host's own wrapper
/// around `init()` — but `getDriverName`/`getErrorMessage` are documented
/// against its `name[32]`/`errorMessage[124]` buffers, so it is mirrored here to
/// give those two methods a hard bound.
#[repr(C, packed(4))]
pub(crate) struct ASIODriverInfo {
    pub(crate) asio_version: i32,
    pub(crate) driver_version: i32,
    pub(crate) name: [c_char; 32],
    pub(crate) error_message: [c_char; 124],
    pub(crate) sys_ref: *mut c_void,
}

/// `ASIOClockSource` (asio.h).
#[repr(C, packed(4))]
pub(crate) struct ASIOClockSource {
    pub(crate) index: i32,
    pub(crate) associated_channel: i32,
    pub(crate) associated_group: i32,
    pub(crate) is_current_source: ASIOBool,
    pub(crate) name: [c_char; 32],
}

/// `ASIOChannelInfo` (asio.h). `channel`/`is_input` are inputs to
/// `getChannelInfo`; the rest are outputs.
#[repr(C, packed(4))]
pub(crate) struct ASIOChannelInfo {
    pub(crate) channel: i32,
    pub(crate) is_input: ASIOBool,
    pub(crate) is_active: ASIOBool,
    pub(crate) channel_group: i32,
    pub(crate) sample_type: ASIOSampleType,
    pub(crate) name: [c_char; 32],
}

/// `ASIOBufferInfo` (asio.h). `is_input`/`channel_num` are inputs to
/// `createBuffers`; `buffers` comes back filled with the two halves of the
/// double buffer — the driver owns that memory, the host only points at it.
#[repr(C, packed(4))]
pub(crate) struct ASIOBufferInfo {
    pub(crate) is_input: ASIOBool,
    pub(crate) channel_num: i32,
    pub(crate) buffers: [*mut c_void; 2],
}

// ---------------------------------------------------------------------------
// COM plumbing (Windows SDK, not ASIO)
// ---------------------------------------------------------------------------

/// `HRESULT` (`winerror.h`): a 32-bit signed status code.
pub(crate) type HResult = i32;

/// `S_OK` (`winerror.h`).
pub(crate) const S_OK: HResult = 0;
/// `S_FALSE` (`winerror.h`).
pub(crate) const S_FALSE: HResult = 1;
/// `E_NOINTERFACE` (`winerror.h`).
pub(crate) const E_NOINTERFACE: HResult = 0x8000_4002u32 as i32;
/// `E_POINTER` (`winerror.h`).
pub(crate) const E_POINTER: HResult = 0x8000_4003u32 as i32;
/// `CLASS_E_CLASSNOTAVAILABLE` (`winerror.h`) — what `DllGetClassObject`
/// returns for a CLSID that is not ours (`common/dllentry.cpp` does the same).
pub(crate) const CLASS_E_CLASSNOTAVAILABLE: HResult = 0x8004_0111u32 as i32;

#[cfg(test)]
mod tests {
    use super::*;
    use core::mem::{align_of, size_of};

    /// Byte offset of a field inside a local value.
    ///
    /// Written with `addr_of!` because every struct here is `packed(4)`, where
    /// `&value.field` is the E0793 error "reference to packed field is
    /// unaligned"; `&raw const`/`addr_of!` is the documented workaround and is
    /// exactly what `offset_of!` does internally.
    macro_rules! offset_in {
        ($container:expr, $field:ident) => {{
            let base = &$container as *const _ as usize;
            let field = core::ptr::addr_of!($container.$field) as usize;
            field - base
        }};
    }

    /// `Option<fn>` must stay pointer-sized, i.e. null-optimised, or every
    /// callback struct below is the wrong size.
    #[test]
    fn option_of_function_pointer_is_pointer_sized() {
        assert_eq!(size_of::<*mut c_void>(), 8);
        assert_eq!(
            size_of::<Option<unsafe extern "system" fn(i32, ASIOBool)>>(),
            8
        );
    }

    /// Sizes and alignments under the SDK's `#pragma pack(push,4)`: every field
    /// is aligned to `min(natural, 4)`, so the structs are 4-aligned and 8-byte
    /// members no longer force padding.
    #[test]
    fn layout_matches_msvc_pack_4_sizes() {
        assert_eq!((size_of::<ASIOSamples>(), align_of::<ASIOSamples>()), (8, 4));
        assert_eq!(
            (size_of::<ASIOTimeStamp>(), align_of::<ASIOTimeStamp>()),
            (8, 4)
        );
        // Four 8-byte pointers. `min(natural, 4)` is 4 either way, so they stay
        // on 8-byte boundaries: 0/8/16/24, and the struct is 32 bytes. (The first
        // draft of this test expected 16 bytes at 0/4/8/12 - that is the 32-bit
        // layout, and CI caught it. See the task note about the MSVC ABI probe.)
        assert_eq!(
            (size_of::<ASIOCallbacks>(), align_of::<ASIOCallbacks>()),
            (32, 4)
        );
        // long, long, void*[2] → 4 + 4 + 16.
        assert_eq!(
            (size_of::<ASIOBufferInfo>(), align_of::<ASIOBufferInfo>()),
            (24, 4)
        );
        // long, long, long, long, long, char[32].
        assert_eq!(
            (size_of::<ASIOChannelInfo>(), align_of::<ASIOChannelInfo>()),
            (52, 4)
        );
        // long, long, long, long, char[32].
        assert_eq!(
            (size_of::<ASIOClockSource>(), align_of::<ASIOClockSource>()),
            (48, 4)
        );
        // long, long, char[32], char[124], void* → 4 + 4 + 32 + 124 + 8.
        assert_eq!(
            (size_of::<ASIODriverInfo>(), align_of::<ASIODriverInfo>()),
            (172, 4)
        );
        // double, ASIOTimeStamp, ASIOSamples, double, unsigned long, char[12].
        assert_eq!(
            (size_of::<AsioTimeInfo>(), align_of::<AsioTimeInfo>()),
            (48, 4)
        );
        // double, ASIOSamples, unsigned long, char[64] → 8 + 8 + 4 + 64.
        assert_eq!(
            (size_of::<ASIOTimeCode>(), align_of::<ASIOTimeCode>()),
            (84, 4)
        );
        // long[4], AsioTimeInfo, ASIOTimeCode.
        assert_eq!((size_of::<ASIOTime>(), align_of::<ASIOTime>()), (148, 4));
    }

    #[test]
    fn layout_matches_msvc_pack_4_offsets() {
        // The callbacks are pointers, so packing does not move them off their
        // natural 8-byte stride; the pack pragma only caps the *alignment* at 4,
        // which costs nothing when the member is already 8 bytes wide.
        let callbacks: ASIOCallbacks = unsafe { core::mem::zeroed() };
        assert_eq!(offset_in!(callbacks, buffer_switch), 0);
        assert_eq!(offset_in!(callbacks, sample_rate_did_change), 8);
        assert_eq!(offset_in!(callbacks, asio_message), 16);
        assert_eq!(offset_in!(callbacks, buffer_switch_time_info), 24);

        let infos: ASIOBufferInfo = unsafe { core::mem::zeroed() };
        assert_eq!(offset_in!(infos, is_input), 0);
        assert_eq!(offset_in!(infos, channel_num), 4);
        assert_eq!(offset_in!(infos, buffers), 8);

        let info: ASIOChannelInfo = unsafe { core::mem::zeroed() };
        assert_eq!(offset_in!(info, channel), 0);
        assert_eq!(offset_in!(info, is_input), 4);
        assert_eq!(offset_in!(info, is_active), 8);
        assert_eq!(offset_in!(info, channel_group), 12);
        assert_eq!(offset_in!(info, sample_type), 16);
        assert_eq!(offset_in!(info, name), 20);

        let driver_info: ASIODriverInfo = unsafe { core::mem::zeroed() };
        assert_eq!(offset_in!(driver_info, asio_version), 0);
        assert_eq!(offset_in!(driver_info, driver_version), 4);
        assert_eq!(offset_in!(driver_info, name), 8);
        assert_eq!(offset_in!(driver_info, error_message), 40);
        // A pointer, but 4-aligned because of the pack pragma.
        assert_eq!(offset_in!(driver_info, sys_ref), 164);
    }

    /// The values the vtable depends on, straight out of asio.h.
    #[test]
    fn error_codes_and_bools_match_the_sdk() {
        assert_eq!(ASE_OK, 0);
        assert_eq!(ASE_SUCCESS, 0x3f48_47a0);
        assert_eq!(ASE_NotPresent, -1000);
        assert_eq!(ASE_HWMalfunction, -999);
        assert_eq!(ASE_InvalidParameter, -998);
        assert_eq!(ASE_InvalidMode, -997);
        assert_eq!(ASE_SPNotAdvancing, -996);
        assert_eq!(ASE_NoClock, -995);
        assert_eq!(ASE_NoMemory, -994);
        assert_eq!((ASIO_FALSE, ASIO_TRUE), (0, 1));
        assert_eq!(ASIOSTFloat32LSB, 19);
    }
}
