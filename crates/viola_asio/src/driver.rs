// SPDX-License-Identifier: GPL-3.0-or-later
//! The driver object: `IUnknown` plus all 21 `IASIO` methods.
//!
//! `common/iasiodrv.h` declares
//!
//! ```c
//! interface IASIO : public IUnknown
//! {
//!     virtual ASIOBool init(void *sysHandle) = 0;
//!     ... 21 methods in all, in this order ...
//! };
//! ```
//!
//! so the vtable the host walks is three `IUnknown` slots followed by those 21,
//! in iasiodrv.h's order — the vtable order *is* the ABI, and getting it wrong
//! crashes the host. `DriverVtbl` below numbers the ASIO slots 0..=20 so our
//! declaration can be lined up against the header without counting.
//!
//! `Driver` keeps the vtable pointer as its first field, because the host reads
//! it as `*(void***)this` (`IASIO*` is just the object pointer); every method
//! therefore takes the object pointer as its first argument (`This` in the C++).
//!
//! `STDMETHODCALLTYPE` is `extern "system"`: `__stdcall` on x86, the plain C
//! convention on x64 — the only target this DLL is built for.
//!
//! Every method is null-safe and either reports an ASIO error or returns a
//! documented default. Nothing here panics and nothing dereferences a host
//! pointer without checking it first: the DLL runs inside the host's process, so
//! a bug of ours is the DAW's crash.
//!
//! Status of this revision (M5.1a): the static facts are answered (channel
//! count, buffer window, sample rate, clock source) and everything that needs
//! buffers, a clock thread, or host memory is still a stub that reports an
//! error. `createBuffers`/`disposeBuffers`/`start` report `ASE_InvalidMode`
//! rather than `ASE_NotPresent`, because `getChannels` has already told the host
//! that the device is present; asio.h documents `ASE_InvalidMode` for exactly
//! the "no buffers were ever prepared / used in a bad mode" case.

use core::ffi::{c_char, c_void};
use core::ptr;
use core::sync::atomic::{AtomicU32, AtomicU64, Ordering};

use crate::ffi::{
    ASIOBool, ASIOBufferInfo, ASIOCallbacks, ASIOChannelInfo, ASIOClockSource, ASIOError,
    ASIOSampleRate, ASIOSamples, ASIOTimeStamp, ASIO_TRUE, ASE_InvalidMode, ASE_InvalidParameter,
    ASE_NoClock, ASE_NotPresent, ASE_OK, ASE_SPNotAdvancing, E_NOINTERFACE, E_POINTER, HResult,
    S_OK,
};
use crate::guid::{guid_ref, Guid, CLSID_VIOLA_ASIO, IID_IUNKNOWN};
use crate::{
    rate_is_supported, DEFAULT_BUFFER_SIZE, DEFAULT_SAMPLE_RATE, DRIVER_NAME, LIVE_OBJECTS,
    MAX_BUFFER_SIZE, MIN_BUFFER_SIZE, BUFFER_SIZE_GRANULARITY, CHANNEL_COUNT,
};

/// `getDriverName` writes into `ASIODriverInfo.name[32]` (asio.h).
const ASIO_DRIVER_NAME_CAP: usize = 32;
/// `getErrorMessage` writes into `ASIODriverInfo.errorMessage[124]` (asio.h).
const ASIO_ERROR_MESSAGE_CAP: usize = 124;
/// `ASIOClockSource.name[32]` (asio.h).
const ASIO_CLOCK_SOURCE_NAME_CAP: usize = 32;
/// The one source we report is the internal clock generator (asio.h:
/// "at least 1 (internal clock generator)"); its `index` is 0.
const INTERNAL_CLOCK_NAME: &str = "Internal";
/// `getDriverVersion` is "driver specific" (asio.h). Revision 1 of this driver.
const DRIVER_VERSION: i32 = 1;
/// What `getErrorMessage` reports until M5.1b has real failures to report.
/// asio.h: the string should describe "the type of error that occured during
/// ASIOInit()".
const SKELETON_MESSAGE: &str = "viola-bridge ASIO: skeleton driver, audio I/O not implemented yet";

/// The `IASIO` vtable: 3 `IUnknown` slots, then the 21 ASIO slots of
/// `common/iasiodrv.h` in declaration order.
///
/// Slot comments are the *vtable* index; `0` is `init`, matching the order in
/// iasiodrv.h and in `docs/asio-driver-notes.md` §3.
///
/// The host reads these slots out of the object; we never read them back, so the
/// field lint would call all 24 of them dead. They are the ABI.
#[allow(dead_code)]
#[repr(C)]
struct DriverVtbl {
    // --- IUnknown (objbase.h) -------------------------------------------------
    /// `HRESULT QueryInterface(REFIID riid, void **ppvObject)`
    query_interface: unsafe extern "system" fn(*mut Driver, *const Guid, *mut *mut c_void) -> HResult,
    /// `ULONG AddRef()`
    add_ref: unsafe extern "system" fn(*mut Driver) -> u32,
    /// `ULONG Release()`
    release: unsafe extern "system" fn(*mut Driver) -> u32,

    // --- IASIO (common/iasiodrv.h) -------------------------------------------
    /// 0 — `ASIOBool init(void *sysHandle)`
    init: unsafe extern "system" fn(*mut Driver, *mut c_void) -> ASIOBool,
    /// 1 — `void getDriverName(char *name)`
    get_driver_name: unsafe extern "system" fn(*mut Driver, *mut c_char),
    /// 2 — `long getDriverVersion()`
    get_driver_version: unsafe extern "system" fn(*mut Driver) -> i32,
    /// 3 — `void getErrorMessage(char *string)`
    get_error_message: unsafe extern "system" fn(*mut Driver, *mut c_char),
    /// 4 — `ASIOError start()`
    start: unsafe extern "system" fn(*mut Driver) -> ASIOError,
    /// 5 — `ASIOError stop()`
    stop: unsafe extern "system" fn(*mut Driver) -> ASIOError,
    /// 6 — `ASIOError getChannels(long *numInputChannels, long *numOutputChannels)`
    get_channels: unsafe extern "system" fn(*mut Driver, *mut i32, *mut i32) -> ASIOError,
    /// 7 — `ASIOError getLatencies(long *inputLatency, long *outputLatency)`
    get_latencies: unsafe extern "system" fn(*mut Driver, *mut i32, *mut i32) -> ASIOError,
    /// 8 — `ASIOError getBufferSize(long *minSize, long *maxSize, long *preferredSize, long *granularity)`
    get_buffer_size: unsafe extern "system" fn(*mut Driver, *mut i32, *mut i32, *mut i32, *mut i32) -> ASIOError,
    /// 9 — `ASIOError canSampleRate(ASIOSampleRate sampleRate)`
    can_sample_rate: unsafe extern "system" fn(*mut Driver, ASIOSampleRate) -> ASIOError,
    /// 10 — `ASIOError getSampleRate(ASIOSampleRate *sampleRate)`
    get_sample_rate: unsafe extern "system" fn(*mut Driver, *mut ASIOSampleRate) -> ASIOError,
    /// 11 — `ASIOError setSampleRate(ASIOSampleRate sampleRate)`
    set_sample_rate: unsafe extern "system" fn(*mut Driver, ASIOSampleRate) -> ASIOError,
    /// 12 — `ASIOError getClockSources(ASIOClockSource *clocks, long *numSources)`
    get_clock_sources: unsafe extern "system" fn(*mut Driver, *mut ASIOClockSource, *mut i32) -> ASIOError,
    /// 13 — `ASIOError setClockSource(long reference)`
    set_clock_source: unsafe extern "system" fn(*mut Driver, i32) -> ASIOError,
    /// 14 — `ASIOError getSamplePosition(ASIOSamples *sPos, ASIOTimeStamp *tStamp)`
    get_sample_position: unsafe extern "system" fn(*mut Driver, *mut ASIOSamples, *mut ASIOTimeStamp) -> ASIOError,
    /// 15 — `ASIOError getChannelInfo(ASIOChannelInfo *info)`
    get_channel_info: unsafe extern "system" fn(*mut Driver, *mut ASIOChannelInfo) -> ASIOError,
    /// 16 — `ASIOError createBuffers(ASIOBufferInfo *bufferInfos, long numChannels, long bufferSize, ASIOCallbacks *callbacks)`
    create_buffers: unsafe extern "system" fn(*mut Driver, *mut ASIOBufferInfo, i32, i32, *mut ASIOCallbacks) -> ASIOError,
    /// 17 — `ASIOError disposeBuffers()`
    dispose_buffers: unsafe extern "system" fn(*mut Driver) -> ASIOError,
    /// 18 — `ASIOError controlPanel()`
    control_panel: unsafe extern "system" fn(*mut Driver) -> ASIOError,
    /// 19 — `ASIOError future(long selector, void *opt)`
    future: unsafe extern "system" fn(*mut Driver, i32, *mut c_void) -> ASIOError,
    /// 20 — `ASIOError outputReady()`
    output_ready: unsafe extern "system" fn(*mut Driver) -> ASIOError,
}

/// The one vtable shared by every driver instance. It is a `static` because a
/// COM object holds a pointer to it, never the table itself; it contains only
/// function pointers, so it is `Sync` and can be shared across apartment
/// threads.
static DRIVER_VTABLE: DriverVtbl = DriverVtbl {
    query_interface: driver_query_interface,
    add_ref: driver_add_ref,
    release: driver_release,

    init: asio_init,
    get_driver_name: asio_get_driver_name,
    get_driver_version: asio_get_driver_version,
    get_error_message: asio_get_error_message,
    start: asio_start,
    stop: asio_stop,
    get_channels: asio_get_channels,
    get_latencies: asio_get_latencies,
    get_buffer_size: asio_get_buffer_size,
    can_sample_rate: asio_can_sample_rate,
    get_sample_rate: asio_get_sample_rate,
    set_sample_rate: asio_set_sample_rate,
    get_clock_sources: asio_get_clock_sources,
    set_clock_source: asio_set_clock_source,
    get_sample_position: asio_get_sample_position,
    get_channel_info: asio_get_channel_info,
    create_buffers: asio_create_buffers,
    dispose_buffers: asio_dispose_buffers,
    control_panel: asio_control_panel,
    future: asio_future,
    output_ready: asio_output_ready,
};

/// A driver instance, as the host sees it: `IASIO*` is this pointer.
#[repr(C)]
pub(crate) struct Driver {
    /// **Must stay the first field.** `QueryInterface`/`CreateInstance` hand the
    /// host this very pointer as the interface, and the host then reads its
    /// first word as the vtable (`*(void***)this`).
    ///
    /// Never read by *us* — the host reads it out of the object — so the field
    /// lint would call it dead. It is the ABI, not dead weight.
    #[allow(dead_code)]
    vtbl: *const DriverVtbl,
    /// COM reference count. Atomic because the host may release from a different
    /// thread than the one that called us.
    ref_count: AtomicU32,
    /// The rate `getSampleRate` answers with, as raw `f64` bits. Kept as an
    /// atomic so `setSampleRate` can honour the host without a lock: the value
    /// is a single word and there is nothing to keep consistent with it.
    sample_rate_bits: AtomicU64,
}

impl Driver {
    /// A fresh driver with one reference, which the caller owns.
    ///
    /// This is the `CreateInstance` factory function of the COM template in
    /// `common/combase.h`, except that the reference count starts at 1 instead
    /// of 0: COM will ask us for an interface pointer next, and that is the
    /// reference the host ends up holding.
    pub(crate) fn new_boxed() -> Box<Driver> {
        let driver = Box::new(Driver {
            vtbl: &DRIVER_VTABLE as *const DriverVtbl,
            ref_count: AtomicU32::new(1),
            sample_rate_bits: AtomicU64::new(DEFAULT_SAMPLE_RATE.to_bits()),
        });
        LIVE_OBJECTS.fetch_add(1, Ordering::AcqRel);
        driver
    }
}

/// Borrow a live driver, or `None` for a null `This`.
///
/// # Safety
///
/// `this` must be null or a pointer returned by `Driver::new_boxed` that is
/// still alive, and it must stay live for `'a`.
unsafe fn driver_ref<'a>(this: *mut Driver) -> Option<&'a Driver> {
    if this.is_null() {
        None
    } else {
        Some(unsafe { &*this })
    }
}

// ---------------------------------------------------------------------------
// IUnknown (objbase.h)
// ---------------------------------------------------------------------------

/// `QueryInterface`.
///
/// Answers `IID_IUnknown` and our CLSID-as-IID, and nothing else — the same
/// shape as the SDK sample's `AsioSample::NonDelegatingQueryInterface`, which
/// compares against `IID_ASIO_DRIVER` and defers the rest
/// (`docs/asio-driver-notes.md` §1).
///
/// A successful `QueryInterface` must AddRef, and a failed one must leave both
/// `*ppvObject` and the reference count untouched.
pub(crate) unsafe extern "system" fn driver_query_interface(
    this: *mut Driver,
    riid: *const Guid,
    ppv: *mut *mut c_void,
) -> HResult {
    // COM's first rule on failure: hand back a null interface pointer.
    if ppv.is_null() {
        return E_POINTER;
    }
    unsafe { *ppv = ptr::null_mut() };

    if this.is_null() {
        return E_POINTER;
    }
    let Some(riid) = (unsafe { guid_ref(riid) }) else {
        return E_POINTER;
    };

    if *riid == IID_IUNKNOWN || *riid == CLSID_VIOLA_ASIO {
        unsafe { *ppv = this as *mut c_void };
        unsafe { driver_add_ref(this) };
        S_OK
    } else {
        E_NOINTERFACE
    }
}

/// `AddRef`.
///
/// # Safety
///
/// `this` must be a live driver pointer.
pub(crate) unsafe extern "system" fn driver_add_ref(this: *mut Driver) -> u32 {
    match unsafe { driver_ref(this) } {
        Some(driver) => driver.ref_count.fetch_add(1, Ordering::AcqRel) + 1,
        None => 0,
    }
}

/// `Release`, dropping (and freeing) the object at zero.
///
/// # Safety
///
/// `this` must be null or a live driver pointer; after the last reference is
/// released the pointer is dangling, so the caller must not use it again.
pub(crate) unsafe extern "system" fn driver_release(this: *mut Driver) -> u32 {
    let Some(driver) = (unsafe { driver_ref(this) }) else {
        return 0;
    };

    // `fetch_update` rather than `fetch_sub`: a host that releases once too
    // often must not be able to wrap the counter into freeing twice.
    match driver
        .ref_count
        .fetch_update(Ordering::AcqRel, Ordering::Acquire, |n| n.checked_sub(1))
    {
        Ok(previous) => {
            if previous == 1 {
                LIVE_OBJECTS.fetch_sub(1, Ordering::AcqRel);
                // Back to the `Box` the object came from, and drop it here.
                drop(unsafe { Box::from_raw(this) });
                0
            } else {
                previous - 1
            }
        }
        Err(_) => 0,
    }
}

// ---------------------------------------------------------------------------
// IASIO — 0..=8: init, naming, transport state, channel/buffer enquiry
// ---------------------------------------------------------------------------

/// `init(void *sysHandle)` — 0.
///
/// `ASIOTrue` means accepted. `sysHandle` is the host's main window handle
/// (`ASIODriverInfo.sysRef`, asio.h) and may legitimately be null, so it is not
/// inspected. M5.1b will start the timer thread from here.
unsafe extern "system" fn asio_init(_this: *mut Driver, _sys_handle: *mut c_void) -> ASIOBool {
    ASIO_TRUE
}

/// `getDriverName(char *name)` — 1.
///
/// The host passes the `char name[32]` of its `ASIODriverInfo`; we write the
/// contract's driver name and always NUL-terminate.
unsafe extern "system" fn asio_get_driver_name(_this: *mut Driver, name: *mut c_char) {
    unsafe { write_cstr(name, DRIVER_NAME, ASIO_DRIVER_NAME_CAP) };
}

/// `getDriverVersion()` — 2.
///
/// Returns `long`, so it cannot report an error (asio.h: the format is "driver
/// specific"); revision 1 is the documented default.
unsafe extern "system" fn asio_get_driver_version(_this: *mut Driver) -> i32 {
    DRIVER_VERSION
}

/// `getErrorMessage(char *string)` — 3.
///
/// The host passes the `char errorMessage[124]` of its `ASIODriverInfo`.
unsafe extern "system" fn asio_get_error_message(_this: *mut Driver, string: *mut c_char) {
    unsafe { write_cstr(string, SKELETON_MESSAGE, ASIO_ERROR_MESSAGE_CAP) };
}

/// `start()` — 4.
///
/// Nothing is prepared, so there is no cadence to start: `ASE_InvalidMode` ("the
/// hardware is in a bad mode or used in a bad mode", asio.h). M5.1b starts the
/// waitable-timer thread here.
unsafe extern "system" fn asio_start(_this: *mut Driver) -> ASIOError {
    ASE_InvalidMode
}

/// `stop()` — 5.
///
/// `ASE_OK`: we are not running, and asio.h only constrains `stop()` by
/// requiring that no `bufferSwitch` is called after it returns — trivially true.
/// Idempotent on purpose, because hosts call it during teardown whether they
/// started us or not.
unsafe extern "system" fn asio_stop(_this: *mut Driver) -> ASIOError {
    ASE_OK
}

/// `getChannels(long *numInputChannels, long *numOutputChannels)` — 6.
///
/// 16 in / 16 out, the contract's default and the point of the exercise.
unsafe extern "system" fn asio_get_channels(
    _this: *mut Driver,
    num_input_channels: *mut i32,
    num_output_channels: *mut i32,
) -> ASIOError {
    if num_input_channels.is_null() || num_output_channels.is_null() {
        return ASE_InvalidParameter;
    }
    unsafe { *num_input_channels = CHANNEL_COUNT };
    unsafe { *num_output_channels = CHANNEL_COUNT };
    ASE_OK
}

/// `getLatencies(long *inputLatency, long *outputLatency)` — 7.
///
/// A virtual device that hands the host's buffer straight on has exactly one
/// block of latency in each direction, so the contract's 512-frame default is
/// the honest answer (asio.h: the input latency is "usually the size of one
/// block in sample frames, plus device specific latencies").
unsafe extern "system" fn asio_get_latencies(
    _this: *mut Driver,
    input_latency: *mut i32,
    output_latency: *mut i32,
) -> ASIOError {
    if input_latency.is_null() || output_latency.is_null() {
        return ASE_InvalidParameter;
    }
    unsafe { *input_latency = DEFAULT_BUFFER_SIZE };
    unsafe { *output_latency = DEFAULT_BUFFER_SIZE };
    ASE_OK
}

/// `getBufferSize(long *minSize, long *maxSize, long *preferredSize, long *granularity)` — 8.
///
/// The contract's window: min 64, max 2048, preferred 512, granularity `-1`.
///
/// Note: asio.h says granularity is `-1` for the common "power-of-two sizes
/// from minSize to maxSize" case and reserves `0` for "minimum and maximum buffer
/// size are equal", which is not our case. The value below is what
/// `docs/viola-asio-contract.md` freezes, so it is not changed silently — it is
/// flagged for review in the M5.1a handover instead.
unsafe extern "system" fn asio_get_buffer_size(
    _this: *mut Driver,
    min_size: *mut i32,
    max_size: *mut i32,
    preferred_size: *mut i32,
    granularity: *mut i32,
) -> ASIOError {
    if min_size.is_null() || max_size.is_null() || preferred_size.is_null() || granularity.is_null()
    {
        return ASE_InvalidParameter;
    }
    unsafe { *min_size = MIN_BUFFER_SIZE };
    unsafe { *max_size = MAX_BUFFER_SIZE };
    unsafe { *preferred_size = DEFAULT_BUFFER_SIZE };
    unsafe { *granularity = BUFFER_SIZE_GRANULARITY };
    ASE_OK
}

// ---------------------------------------------------------------------------
// IASIO — 9..=14: sample rate and clock
// ---------------------------------------------------------------------------

/// `canSampleRate(ASIOSampleRate sampleRate)` — 9.
///
/// Accepts 44100/48000/88200/96000 (the contract) and reports `ASE_NoClock` for
/// anything else, as asio.h prescribes for an unsupported rate. The comparison
/// is exact because these are exactly representable `f64` values and hosts pass
/// them literally — the SDK sample compares the same way.
unsafe extern "system" fn asio_can_sample_rate(
    _this: *mut Driver,
    sample_rate: ASIOSampleRate,
) -> ASIOError {
    if rate_is_supported(sample_rate) {
        ASE_OK
    } else {
        ASE_NoClock
    }
}

/// `getSampleRate(ASIOSampleRate *sampleRate)` — 10.
///
/// 48000 until the host asks for something else; asio.h wants `ASE_NoClock` and
/// a zero rate only when the rate is genuinely unknown.
unsafe extern "system" fn asio_get_sample_rate(
    this: *mut Driver,
    sample_rate: *mut ASIOSampleRate,
) -> ASIOError {
    if sample_rate.is_null() {
        return ASE_InvalidParameter;
    }
    let Some(driver) = (unsafe { driver_ref(this) }) else {
        return ASE_InvalidParameter;
    };
    unsafe { *sample_rate = f64::from_bits(driver.sample_rate_bits.load(Ordering::Acquire)) };
    ASE_OK
}

/// `setSampleRate(ASIOSampleRate sampleRate)` — 11.
///
/// Remembers a rate we support and reports `ASE_NoClock` for one we do not,
/// leaving the previous rate in place. `sampleRate == 0` means "use external
/// sync" (asio.h), and a virtual device has none, so 0 is rejected too.
unsafe extern "system" fn asio_set_sample_rate(
    this: *mut Driver,
    sample_rate: ASIOSampleRate,
) -> ASIOError {
    if !rate_is_supported(sample_rate) {
        return ASE_NoClock;
    }
    let Some(driver) = (unsafe { driver_ref(this) }) else {
        return ASE_InvalidParameter;
    };
    driver
        .sample_rate_bits
        .store(sample_rate.to_bits(), Ordering::Release);
    ASE_OK
}

/// `getClockSources(ASIOClockSource *clocks, long *numSources)` — 12.
///
/// asio.h requires "at least 1 (internal clock generator)", so we report the one
/// internal source rather than failing the call. `numSources` is in/out: on the
/// way in it is the capacity of the caller's array, so the array is only written
/// when there is room for the single entry.
///
/// `associatedChannel`/`associatedGroup` are -1, which asio.h prescribes for a
/// source with no input channel behind it ("like Word Clock, or internal
/// oscillator").
unsafe extern "system" fn asio_get_clock_sources(
    _this: *mut Driver,
    clocks: *mut ASIOClockSource,
    num_sources: *mut i32,
) -> ASIOError {
    if num_sources.is_null() {
        return ASE_InvalidParameter;
    }
    let capacity = unsafe { *num_sources };
    unsafe { *num_sources = 1 };
    if clocks.is_null() || capacity < 1 {
        // The host asked for the count only.
        return ASE_OK;
    }

    // Build the name in a plain local array: `ASIOClockSource` is `packed(4)`,
    // so taking `&mut` of its `name` field would be an unaligned borrow (E0793).
    let mut name = [0 as c_char; ASIO_CLOCK_SOURCE_NAME_CAP];
    unsafe { write_cstr(name.as_mut_ptr(), INTERNAL_CLOCK_NAME, ASIO_CLOCK_SOURCE_NAME_CAP) };
    let source = ASIOClockSource {
        index: 0,
        associated_channel: -1,
        associated_group: -1,
        is_current_source: ASIO_TRUE,
        name,
    };
    unsafe { *clocks = source };
    ASE_OK
}

/// `setClockSource(long reference)` — 13.
///
/// Index 0 is the internal generator; anything else is an invalid parameter
/// (there is no second source to select).
unsafe extern "system" fn asio_set_clock_source(_this: *mut Driver, reference: i32) -> ASIOError {
    if reference == 0 {
        ASE_OK
    } else {
        ASE_InvalidParameter
    }
}

/// `getSamplePosition(ASIOSamples *sPos, ASIOTimeStamp *tStamp)` — 14.
///
/// The sample counter only moves while the driver is running, and this revision
/// never runs, so `ASE_SPNotAdvancing` is exactly the documented answer
/// ("hardware is not running when sample position is inquired", asio.h). The
/// host's two output pointers are deliberately left untouched.
unsafe extern "system" fn asio_get_sample_position(
    _this: *mut Driver,
    _s_pos: *mut ASIOSamples,
    _t_stamp: *mut ASIOTimeStamp,
) -> ASIOError {
    ASE_SPNotAdvancing
}

// ---------------------------------------------------------------------------
// IASIO — 15..=20: channel description, buffers, panel, future, outputReady
// ---------------------------------------------------------------------------

/// `getChannelInfo(ASIOChannelInfo *info)` — 15.
///
/// Stub: describing 32 channels (name, group, type, active flag) belongs with
/// the buffer implementation in M5.1b, and asio.h's `name` field wants a real
/// per-channel string rather than a placeholder.
unsafe extern "system" fn asio_get_channel_info(
    _this: *mut Driver,
    info: *mut ASIOChannelInfo,
) -> ASIOError {
    if info.is_null() {
        return ASE_InvalidParameter;
    }
    ASE_NotPresent
}

/// `createBuffers(ASIOBufferInfo *bufferInfos, long numChannels, long bufferSize, ASIOCallbacks *callbacks)` — 16.
///
/// The buffer handshake: the host hands over one `ASIOBufferInfo` per channel
/// and the four callbacks, and the driver is expected to fill in the two halves
/// of each double buffer. That allocation is M5.1b, so a well-formed call is
/// answered with `ASE_InvalidMode` ("used in a bad mode") rather than
/// `ASE_NotPresent`, which would contradict the 16/16 that `getChannels` reports.
unsafe extern "system" fn asio_create_buffers(
    _this: *mut Driver,
    buffer_infos: *mut ASIOBufferInfo,
    num_channels: i32,
    _buffer_size: i32,
    callbacks: *mut ASIOCallbacks,
) -> ASIOError {
    if buffer_infos.is_null() || callbacks.is_null() || num_channels <= 0 {
        return ASE_InvalidParameter;
    }
    ASE_InvalidMode
}

/// `disposeBuffers()` — 17.
///
/// asio.h: "If no buffer were ever prepared, `ASE_InvalidMode` will be
/// returned."
unsafe extern "system" fn asio_dispose_buffers(_this: *mut Driver) -> ASIOError {
    ASE_InvalidMode
}

/// `controlPanel()` — 18.
///
/// asio.h: "If no panel is available `ASE_NotPresent` will be returned", and the
/// host ignores the result. There is nothing to configure yet; when there is,
/// this becomes a settings window rather than a bare return.
unsafe extern "system" fn asio_control_panel(_this: *mut Driver) -> ASIOError {
    ASE_NotPresent
}

/// `future(long selector, void *opt)` — 19.
///
/// Every selector is unknown in this revision, and asio.h is explicit about the
/// answer: "if the selector is unknown, `ASE_InvalidParameter` should be
/// returned to prevent further calls with this selector" — and that a
/// successful `future()` must return `ASE_SUCCESS`, never `ASE_OK`.
unsafe extern "system" fn asio_future(
    _this: *mut Driver,
    _selector: i32,
    _opt: *mut c_void,
) -> ASIOError {
    ASE_InvalidParameter
}

/// `outputReady()` — 20.
///
/// asio.h: return `ASE_NotPresent` "in order to prevent further calls to this
/// function". We never convert into a DMA buffer, so the mechanism does not
/// apply to us.
unsafe extern "system" fn asio_output_ready(_this: *mut Driver) -> ASIOError {
    ASE_NotPresent
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Copy `value` into `dst` as a NUL-terminated C string, writing at most
/// `capacity` bytes including the terminator.
///
/// `getDriverName`/`getErrorMessage` are the two `void`-returning, host-buffer
/// -writing methods in the vtable; both are bounded by the host's own
/// `ASIODriverInfo` fields (`name[32]`, `errorMessage[124]`), but the bound is
/// enforced here rather than trusted.
///
/// # Safety
///
/// `dst` must be null, or point to at least `capacity` writable bytes.
unsafe fn write_cstr(dst: *mut c_char, value: &str, capacity: usize) {
    if dst.is_null() || capacity == 0 {
        return;
    }
    let bytes = value.as_bytes();
    let mut written = 0;
    while written < bytes.len() && written + 1 < capacity {
        unsafe { *dst.add(written) = bytes[written] as c_char };
        written += 1;
    }
    unsafe { *dst.add(written) = 0 };
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::guid::IID_ICLASSFACTORY;

    /// Run `f` with a live driver and release it afterwards.
    fn with_driver<R>(f: impl FnOnce(*mut Driver) -> R) -> R {
        let driver = Box::into_raw(Driver::new_boxed());
        let result = f(driver);
        unsafe { driver_release(driver) };
        result
    }

    fn bogus_iid() -> Guid {
        Guid {
            data1: 0xDEAD_BEEF,
            data2: 0x1234,
            data3: 0x5678,
            data4: [1, 2, 3, 4, 5, 6, 7, 8],
        }
    }

    #[test]
    fn the_vtable_pointer_is_the_first_word_of_the_object() {
        let raw = Box::into_raw(Driver::new_boxed());
        // The host only ever gets `IASIO*`, whose first word must be the vtable.
        let first_word = unsafe { *(raw as *const *const DriverVtbl) };
        assert_eq!(first_word, &DRIVER_VTABLE as *const DriverVtbl);
        assert_eq!(unsafe { driver_release(raw) }, 0);
    }

    #[test]
    fn query_interface_answers_iid_iunknown() {
        let raw = Box::into_raw(Driver::new_boxed());
        let mut out: *mut c_void = ptr::null_mut();
        let hr = unsafe { driver_query_interface(raw, &IID_IUNKNOWN, &mut out) };
        assert_eq!(hr, S_OK);
        assert_eq!(out as *mut Driver, raw);
        // One reference from creation, one added by QueryInterface.
        assert_eq!(unsafe { driver_release(raw) }, 1);
        assert_eq!(unsafe { driver_release(raw) }, 0);
    }

    #[test]
    fn query_interface_answers_the_clsid_as_iid() {
        let raw = Box::into_raw(Driver::new_boxed());
        let mut out: *mut c_void = ptr::null_mut();
        let hr = unsafe { driver_query_interface(raw, &CLSID_VIOLA_ASIO, &mut out) };
        assert_eq!(hr, S_OK);
        assert_eq!(out as *mut Driver, raw);
        assert_eq!(unsafe { driver_release(raw) }, 1);
        assert_eq!(unsafe { driver_release(raw) }, 0);
    }

    #[test]
    fn query_interface_rejects_an_unknown_iid() {
        let raw = Box::into_raw(Driver::new_boxed());
        let mut out: *mut c_void = ptr::null_mut();
        let hr = unsafe { driver_query_interface(raw, &bogus_iid(), &mut out) };
        assert_eq!(hr, E_NOINTERFACE);
        assert!(out.is_null());
        // A failed QueryInterface must not have AddRef'd.
        assert_eq!(unsafe { driver_release(raw) }, 0);
    }

    #[test]
    fn query_interface_rejects_iid_iclassfactory_on_a_driver() {
        let raw = Box::into_raw(Driver::new_boxed());
        let mut out: *mut c_void = ptr::null_mut();
        let hr = unsafe { driver_query_interface(raw, &IID_ICLASSFACTORY, &mut out) };
        assert_eq!(hr, E_NOINTERFACE);
        assert!(out.is_null());
        assert_eq!(unsafe { driver_release(raw) }, 0);
    }

    #[test]
    fn query_interface_rejects_null_pointers() {
        let raw = Box::into_raw(Driver::new_boxed());
        let mut out: *mut c_void = ptr::null_mut();

        assert_eq!(
            unsafe { driver_query_interface(raw, &IID_IUNKNOWN, ptr::null_mut()) },
            E_POINTER
        );
        assert_eq!(
            unsafe { driver_query_interface(raw, ptr::null(), &mut out) },
            E_POINTER
        );
        assert_eq!(
            unsafe { driver_query_interface(ptr::null_mut(), &IID_IUNKNOWN, &mut out) },
            E_POINTER
        );
        assert!(out.is_null());
        assert_eq!(unsafe { driver_release(raw) }, 0);
    }

    #[test]
    fn release_of_a_null_pointer_is_a_no_op() {
        assert_eq!(unsafe { driver_release(ptr::null_mut()) }, 0);
        assert_eq!(unsafe { driver_add_ref(ptr::null_mut()) }, 0);
    }

    #[test]
    fn get_channels_reports_sixteen_in_sixteen_out() {
        with_driver(|driver| {
            let mut inputs: i32 = -1;
            let mut outputs: i32 = -1;
            assert_eq!(
                unsafe { asio_get_channels(driver, &mut inputs, &mut outputs) },
                ASE_OK
            );
            assert_eq!(inputs, 16);
            assert_eq!(outputs, 16);
        });
    }

    #[test]
    fn get_latencies_reports_one_block_each_way() {
        with_driver(|driver| {
            let mut input_latency: i32 = -1;
            let mut output_latency: i32 = -1;
            assert_eq!(
                unsafe { asio_get_latencies(driver, &mut input_latency, &mut output_latency) },
                ASE_OK
            );
            assert_eq!(input_latency, 512);
            assert_eq!(output_latency, 512);
        });
    }

    #[test]
    fn get_buffer_size_reports_the_contract_window() {
        with_driver(|driver| {
            let (mut min, mut max, mut preferred, mut granularity) = (0, 0, 0, -7);
            assert_eq!(
                unsafe {
                    asio_get_buffer_size(driver, &mut min, &mut max, &mut preferred, &mut granularity)
                },
                ASE_OK
            );
            assert_eq!(min, 64);
            assert_eq!(max, 2048);
            assert_eq!(preferred, 512);
            assert_eq!(granularity, -1);
        });
    }

    #[test]
    fn null_out_parameters_are_reported_not_dereferenced() {
        with_driver(|driver| {
            assert_eq!(
                unsafe { asio_get_channels(driver, ptr::null_mut(), ptr::null_mut()) },
                ASE_InvalidParameter
            );
            assert_eq!(
                unsafe { asio_get_sample_rate(driver, ptr::null_mut()) },
                ASE_InvalidParameter
            );
            assert_eq!(
                unsafe { asio_get_latencies(driver, ptr::null_mut(), ptr::null_mut()) },
                ASE_InvalidParameter
            );
            assert_eq!(
                unsafe {
                    asio_get_buffer_size(
                        driver,
                        ptr::null_mut(),
                        ptr::null_mut(),
                        ptr::null_mut(),
                        ptr::null_mut(),
                    )
                },
                ASE_InvalidParameter
            );
            assert_eq!(
                unsafe { asio_get_channel_info(driver, ptr::null_mut()) },
                ASE_InvalidParameter
            );
            assert_eq!(
                unsafe { asio_get_clock_sources(driver, ptr::null_mut(), ptr::null_mut()) },
                ASE_InvalidParameter
            );
        });
    }

    #[test]
    fn driver_name_and_version_are_the_contract_values() {
        with_driver(|driver| {
            assert_eq!(unsafe { asio_get_driver_version(driver) }, DRIVER_VERSION);

            // Fill with a canary so we can see exactly how much was written.
            let mut buf = [0x7Fu8; 32];
            unsafe { asio_get_driver_name(driver, buf.as_mut_ptr() as *mut c_char) };
            assert_eq!(&buf[..DRIVER_NAME.len()], DRIVER_NAME.as_bytes());
            assert_eq!(buf[DRIVER_NAME.len()], 0);
            assert!(buf[DRIVER_NAME.len() + 1..].iter().all(|b| *b == 0x7F));
        });
    }

    #[test]
    fn error_message_is_written_and_terminated() {
        with_driver(|driver| {
            let mut buf = [0x7Fu8; 124];
            unsafe { asio_get_error_message(driver, buf.as_mut_ptr() as *mut c_char) };
            assert_ne!(buf[0], 0);
            assert_eq!(buf[SKELETON_MESSAGE.len()], 0);
            assert!(buf[SKELETON_MESSAGE.len() + 1..].iter().all(|b| *b == 0x7F));
        });
    }

    #[test]
    fn write_cstr_never_overflows_a_short_buffer() {
        let mut buf = [0x7Fu8; 4];
        unsafe { write_cstr(buf.as_mut_ptr() as *mut c_char, "abcdef", 4) };
        assert_eq!(buf, [b'a', b'b', b'c', 0]);

        // A null destination, or no space at all, is ignored rather than fatal.
        unsafe { write_cstr(ptr::null_mut(), "abc", 8) };
        let mut empty = [0x7Fu8; 2];
        unsafe { write_cstr(empty.as_mut_ptr() as *mut c_char, "abc", 0) };
        assert_eq!(empty, [0x7F, 0x7F]);
    }

    #[test]
    fn init_accepts_a_null_sys_handle() {
        with_driver(|driver| {
            assert_eq!(unsafe { asio_init(driver, ptr::null_mut()) }, ASIO_TRUE);
        });
    }

    #[test]
    fn sample_rate_starts_at_48000_and_tracks_set_sample_rate() {
        with_driver(|driver| {
            let mut rate: ASIOSampleRate = 0.0;
            assert_eq!(unsafe { asio_get_sample_rate(driver, &mut rate) }, ASE_OK);
            assert_eq!(rate, 48_000.0);

            assert_eq!(
                unsafe { asio_set_sample_rate(driver, 96_000.0) },
                ASE_OK
            );
            assert_eq!(unsafe { asio_get_sample_rate(driver, &mut rate) }, ASE_OK);
            assert_eq!(rate, 96_000.0);

            // An unsupported rate is reported and leaves the current one alone.
            assert_eq!(
                unsafe { asio_set_sample_rate(driver, 32_000.0) },
                ASE_NoClock
            );
            assert_eq!(unsafe { asio_get_sample_rate(driver, &mut rate) }, ASE_OK);
            assert_eq!(rate, 96_000.0);

            // 0 means "external sync" (asio.h), which we do not have.
            assert_eq!(unsafe { asio_set_sample_rate(driver, 0.0) }, ASE_NoClock);
        });
    }

    #[test]
    fn can_sample_rate_accepts_the_contract_rates_only() {
        with_driver(|driver| {
            for rate in [44_100.0, 48_000.0, 88_200.0, 96_000.0] {
                assert_eq!(
                    unsafe { asio_can_sample_rate(driver, rate) },
                    ASE_OK,
                    "rate {rate} should be supported"
                );
            }
            for rate in [0.0, 22_050.0, 32_000.0, 192_000.0] {
                assert_eq!(
                    unsafe { asio_can_sample_rate(driver, rate) },
                    ASE_NoClock,
                    "rate {rate} should not be supported"
                );
            }
        });
    }

    #[test]
    fn get_clock_sources_offers_the_single_internal_clock() {
        with_driver(|driver| {
            let mut sources: [ASIOClockSource; 4] = unsafe { core::mem::zeroed() };
            let mut capacity: i32 = 4;
            assert_eq!(
                unsafe { asio_get_clock_sources(driver, sources.as_mut_ptr(), &mut capacity) },
                ASE_OK
            );
            assert_eq!(capacity, 1);

            let first = sources.as_ptr();
            // Field reads through a raw pointer are fine for a packed struct;
            // only taking a reference to a packed field is not.
            assert_eq!(unsafe { (*first).index }, 0);
            assert_eq!(unsafe { (*first).associated_channel }, -1);
            assert_eq!(unsafe { (*first).associated_group }, -1);
            assert_eq!(unsafe { (*first).is_current_source }, ASIO_TRUE);

            let mut name = [0u8; ASIO_CLOCK_SOURCE_NAME_CAP];
            let src = unsafe { ptr::addr_of!((*first).name) }.cast::<u8>();
            for (i, slot) in name.iter_mut().enumerate() {
                *slot = unsafe { *src.add(i) } as u8;
            }
            assert_eq!(&name[..INTERNAL_CLOCK_NAME.len()], INTERNAL_CLOCK_NAME.as_bytes());
            assert_eq!(name[INTERNAL_CLOCK_NAME.len()], 0);
        });
    }

    #[test]
    fn get_clock_sources_reports_the_count_without_writing_a_short_array() {
        with_driver(|driver| {
            // Capacity 0: report the count, do not touch the (null) array.
            let mut capacity: i32 = 0;
            assert_eq!(
                unsafe { asio_get_clock_sources(driver, ptr::null_mut(), &mut capacity) },
                ASE_OK
            );
            assert_eq!(capacity, 1);
        });
    }

    #[test]
    fn set_clock_source_accepts_only_the_internal_clock() {
        with_driver(|driver| {
            assert_eq!(unsafe { asio_set_clock_source(driver, 0) }, ASE_OK);
            assert_eq!(
                unsafe { asio_set_clock_source(driver, 3) },
                ASE_InvalidParameter
            );
        });
    }

    #[test]
    fn stub_methods_report_errors_without_touching_host_pointers() {
        with_driver(|driver| {
            assert_eq!(unsafe { asio_start(driver) }, ASE_InvalidMode);
            assert_eq!(unsafe { asio_stop(driver) }, ASE_OK);
            assert_eq!(unsafe { asio_dispose_buffers(driver) }, ASE_InvalidMode);
            assert_eq!(unsafe { asio_control_panel(driver) }, ASE_NotPresent);
            assert_eq!(
                unsafe { asio_future(driver, 0, ptr::null_mut()) },
                ASE_InvalidParameter
            );
            assert_eq!(unsafe { asio_output_ready(driver) }, ASE_NotPresent);
            // Both host pointers are null and neither is written.
            assert_eq!(
                unsafe { asio_get_sample_position(driver, ptr::null_mut(), ptr::null_mut()) },
                ASE_SPNotAdvancing
            );
        });
    }

    #[test]
    fn create_buffers_validates_arguments_then_reports_invalid_mode() {
        with_driver(|driver| {
            let mut info: ASIOBufferInfo = unsafe { core::mem::zeroed() };
            let mut callbacks: ASIOCallbacks = unsafe { core::mem::zeroed() };

            assert_eq!(
                unsafe { asio_create_buffers(driver, &mut info, 1, 512, &mut callbacks) },
                ASE_InvalidMode
            );
            assert_eq!(
                unsafe { asio_create_buffers(driver, ptr::null_mut(), 1, 512, &mut callbacks) },
                ASE_InvalidParameter
            );
            assert_eq!(
                unsafe { asio_create_buffers(driver, &mut info, 1, 512, ptr::null_mut()) },
                ASE_InvalidParameter
            );
            assert_eq!(
                unsafe { asio_create_buffers(driver, &mut info, 0, 512, &mut callbacks) },
                ASE_InvalidParameter
            );
        });
    }
}
