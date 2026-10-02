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
//! Status of this revision (M5.3): the static facts are answered, the double
//! buffers are real memory, and a high-resolution waitable timer drives the
//! host's `bufferSwitch` cadence. Each tick also lifts the half the host has
//! just finished writing out of its output buffers, interleaves the active
//! channels, and pushes the block onto a lock-free ring. A third thread drains
//! that ring into `\.\pipe\orender.input`, where `orender` is already waiting.
//!
//! The input buffers stay silent on purpose: a host *reads* those, and M5.3 only
//! carries what a host plays into us.

use core::ffi::{c_char, c_void};
use core::ptr;
use core::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::fs::OpenOptions;
use std::io::Write as _;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};
use crate::ffi::{
    ASIOBool, ASIOBufferInfo, ASIOCallbacks, ASIOChannelInfo, ASIOClockSource, ASIOError,
    ASIOSampleRate, ASIOSamples, ASIOSTFloat32LSB, ASIOTime, ASIOTimeCode, ASIOTimeStamp,
    ASIO_FALSE, ASIO_TRUE, AsioTimeInfo, ASE_HWMalfunction, ASE_InvalidMode, ASE_InvalidParameter,
    ASE_NoClock, ASE_NotPresent, ASE_OK, ASE_SPNotAdvancing, E_NOINTERFACE, E_POINTER, HResult,
    S_OK,
};
use crate::guid::{guid_ref, Guid, CLSID_VIOLA_ASIO, IID_IUNKNOWN};
use crate::pipe::{pipe_loop, Pipeline};
use crate::ring::Tap;
use crate::{
    rate_is_supported, DEFAULT_BUFFER_SIZE, DEFAULT_SAMPLE_RATE, DRIVER_NAME, LIVE_OBJECTS,
    MAX_BUFFER_SIZE, MIN_BUFFER_SIZE, BUFFER_SIZE_GRANULARITY, CHANNEL_COUNT, PIPE_PATH,
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
/// What `getErrorMessage` reports. asio.h: the string should describe "the type
/// of error that occured during ASIOInit()", and this revision has none to
/// report, so it says so rather than claiming a stub that no longer exists.
const NO_ERROR_MESSAGE: &str = "viola-bridge ASIO: no error";

/// `ASIOChannelInfo.name[32]` (asio.h).
const ASIO_CHANNEL_NAME_CAP: usize = 32;
/// `SetWaitableTimer` takes a relative time in 100 ns units.
const HUNDRED_NS_PER_SECOND: u64 = 10_000_000;
/// `CREATE_WAITABLE_TIMER_HIGH_RESOLUTION` (winbase.h). Without it a timer is
/// rounded to the ~15.6 ms scheduler tick, which is slower than one buffer.
const CREATE_WAITABLE_TIMER_HIGH_RESOLUTION: u32 = 0x0000_0002;
/// `TIMER_ALL_ACCESS` (winnt.h).
const TIMER_ALL_ACCESS: u32 = 0x001F_0003;
/// `THREAD_PRIORITY_TIME_CRITICAL` (winbase.h).
const THREAD_PRIORITY_TIME_CRITICAL: i32 = 15;
/// `INFINITE` and `WAIT_OBJECT_0` (winbase.h).
const INFINITE: u32 = 0xFFFF_FFFF;
const WAIT_OBJECT_0: u32 = 0;
/// How often the logger thread appends a status line, and how finely it wakes so
/// that `stop()` does not have to wait a whole interval for it.
const LOG_INTERVAL: Duration = Duration::from_millis(1000);
const LOG_SLICE: Duration = Duration::from_millis(50);
/// `AsioTimeInfo.flags` (asio.h): system time, sample position and sample rate
/// are valid; speed is deliberately not claimed.
const TIME_INFO_FLAGS_VALID: u32 = 1 | 2 | 4;

#[link(name = "kernel32")]
unsafe extern "system" {
    fn CreateWaitableTimerExW(
        attributes: *mut c_void,
        name: *const u16,
        flags: u32,
        desired_access: u32,
    ) -> *mut c_void;
    fn SetWaitableTimer(
        timer: *mut c_void,
        due_time: *const i64,
        period: i32,
        completion: *mut c_void,
        argument: *mut c_void,
        resume: i32,
    ) -> i32;
    fn WaitForSingleObject(handle: *mut c_void, milliseconds: u32) -> u32;
    fn CloseHandle(handle: *mut c_void) -> i32;
    fn GetCurrentThread() -> *mut c_void;
    fn SetThreadPriority(thread: *mut c_void, priority: i32) -> i32;
    fn GetSystemTimeAsFileTime(out: *mut u64);
    fn GetLocalTime(out: *mut LocalSystemTime);
}

/// `SYSTEMTIME` (minwinbase.h), the layout `GetLocalTime` fills in. Only used to
/// stamp the log line with local wall-clock time, so it can be lined up against
/// orender's own log; the audio path never reads it.
#[repr(C)]
struct LocalSystemTime {
    year: u16,
    month: u16,
    day_of_week: u16,
    day: u16,
    hour: u16,
    minute: u16,
    second: u16,
    milliseconds: u16,
}

/// A Win32 handle that is moved into the thread owning it. The raw pointer is not
/// `Send` by itself, but this one is only ever closed by the thread that owns it
/// (`Running::shutdown`), after that thread has been joined.
#[derive(Clone, Copy)]
struct WinHandle(*mut c_void);
unsafe impl Send for WinHandle {}

/// What the timer thread, the logger thread and the pipe thread share with the
/// object.
///
/// The audio path reads only atomics and immutable fields: no lock, no
/// allocation, no I/O. Everything that can block lives in the control methods,
/// which the host calls from its own threads.
struct Tick {
    /// Copied out of the host's `ASIOCallbacks` during `createBuffers`. The host
    /// guarantees these stay callable while the driver holds them.
    callbacks: ASIOCallbacks,
    buffer_size: i32,
    sample_rate: f64,
    started: Instant,
    /// Which half of each double buffer the next callback refers to.
    buffer_index: AtomicU32,
    /// Samples played since `start`, the value `getSamplePosition` reports.
    position: AtomicU64,
    callbacks_served: AtomicU64,
    last_tick_ns: AtomicU64,
    /// The worst gap between two consecutive ticks, in ns. This is the number
    /// that says whether the timer is good enough to feed the pipe.
    worst_gap_ns: AtomicU64,
    /// Lifts one finished half out of the host's output buffers, interleaved. The
    /// ticker owns the only copy of it, so `fill` is only ever called here.
    tap: Tap,
    /// The ring the ticker pushes each finished block onto, and the counters the
    /// log line reports. The audio path only ever calls `write_block` on it.
    pipeline: Arc<Pipeline>,
}

/// The state between `createBuffers` and `disposeBuffers`.
struct Session {
    /// One allocation per activated channel. Boxed so the addresses handed to the
    /// host in `ASIOBufferInfo::buffers` stay put for as long as the host uses
    /// them - a `Vec` of `f32` would move on the next push.
    blocks: Vec<Box<[f32]>>,
    buffer_size: i32,
    callbacks: ASIOCallbacks,
    running: Option<Running>,
    /// `buffers[0]` of every output channel the host activated, indexed by the
    /// host's own `channelNum`; null where it left a channel inactive. Captured
    /// in `createBuffers`, read every tick, and only ever read - the host owns
    /// the memory and frees it in `disposeBuffers`, which stops the ticker first.
    outputs: [*const f32; CHANNEL_COUNT as usize],
}

/// The live timer: shared counters plus the threads that own the handle.
struct Running {
    shared: Arc<Tick>,
    handle: WinHandle,
    stop: Arc<AtomicU32>,
    ticker: Option<JoinHandle<()>>,
    logger: Option<JoinHandle<()>>,
    /// The pipe thread's handle. Owned here so the thread belongs to this state,
    /// but deliberately never joined: see `shutdown`.
    pipe: Option<JoinHandle<()>>,
}

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
    /// Everything between `createBuffers` and `disposeBuffers`, plus the live
    /// timer while `start`ed. A `Mutex` is the right tool here and not in the
    /// audio path: the host calls these from its control thread, and the timer
    /// thread never touches this - it only reads the `Arc<Tick>` it was given.
    session: Mutex<Option<Session>>,
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
            session: Mutex::new(None),
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
/// inspected. The timer thread is started by `start`, not from here.
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
    unsafe { write_cstr(string, NO_ERROR_MESSAGE, ASIO_ERROR_MESSAGE_CAP) };
}

/// `start()` — 4.
///
/// Requires buffers: asio.h answers `ASE_InvalidMode` when the driver "is in a bad
/// mode or used in a bad mode", and starting without a buffer handshake is
/// exactly that. Otherwise a high-resolution waitable timer is created and a
/// thread is put on it; the thread calls the host's `bufferSwitchTimeInfo` (or
/// `bufferSwitch`, if that is the only one it gave us) once per buffer.
///
/// Idempotent: starting an already-running driver is `ASE_OK`, not an error.
unsafe extern "system" fn asio_start(this: *mut Driver) -> ASIOError {
    let Some(driver) = (unsafe { driver_ref(this) }) else {
        return ASE_InvalidParameter;
    };
    let mut guard = driver.session.lock().unwrap_or_else(|error| error.into_inner());
    let Some(session) = guard.as_mut() else {
        return ASE_InvalidMode;
    };
    if session.running.is_some() {
        return ASE_OK;
    }

    let sample_rate = f64::from_bits(driver.sample_rate_bits.load(Ordering::Acquire));
    let period_100ns = (session.buffer_size as u64 * HUNDRED_NS_PER_SECOND) / sample_rate as u64;
    if period_100ns == 0 {
        return ASE_InvalidMode;
    }

    let timer = unsafe {
        CreateWaitableTimerExW(
            ptr::null_mut(),
            ptr::null(),
            CREATE_WAITABLE_TIMER_HIGH_RESOLUTION,
            TIMER_ALL_ACCESS,
        )
    };
    if timer.is_null() {
        return ASE_HWMalfunction;
    }

    // Eight blocks of headroom, rounded up to a power of two: the ring's whole
    // point is to ride out a scheduling hiccup on either side without dropping
    // audio. `block_samples` is interleaved samples, which is what the ring counts.
    let block_samples = session.buffer_size as usize * CHANNEL_COUNT as usize;
    let capacity = block_samples.next_power_of_two() * 8;
    let pipeline = Arc::new(Pipeline::new(capacity));
    let tap = Tap::new(session.outputs, session.buffer_size as usize);

    let shared = Arc::new(Tick {
        callbacks: session.callbacks,
        buffer_size: session.buffer_size,
        sample_rate,
        started: Instant::now(),
        buffer_index: AtomicU32::new(0),
        position: AtomicU64::new(0),
        callbacks_served: AtomicU64::new(0),
        last_tick_ns: AtomicU64::new(0),
        worst_gap_ns: AtomicU64::new(0),
        tap,
        pipeline: Arc::clone(&pipeline),
    });
    let stop = Arc::new(AtomicU32::new(0));
    let handle = WinHandle(timer);

    let ticker = {
        let shared = Arc::clone(&shared);
        let stop = Arc::clone(&stop);
        thread::Builder::new()
            .name("viola-asio tick".to_string())
            .spawn(move || tick_loop(handle, period_100ns, stop, shared))
    };
    let logger = {
        let shared = Arc::clone(&shared);
        let stop = Arc::clone(&stop);
        thread::Builder::new()
            .name("viola-asio log".to_string())
            .spawn(move || log_loop(shared, stop))
    };

    let pipe = {
        let pipeline = Arc::clone(&pipeline);
        let stop = Arc::clone(&stop);
        let pipeline_rate = sample_rate as u32;
        thread::Builder::new()
            .name("viola-asio pipe".to_string())
            .spawn(move || pipe_loop(pipeline, stop, PathBuf::from(PIPE_PATH), pipeline_rate))
    };

    let (ticker, logger, pipe) = match (ticker, logger, pipe) {
        (Ok(ticker), Ok(logger), Ok(pipe)) => (ticker, logger, pipe),
        (ticker, logger, pipe) => {
            // One of the threads did not start. Wind everything down before
            // reporting, so we never leak a timer that nothing waits on.
            stop.store(1, Ordering::Release);
            if let Ok(ticker) = ticker {
                wake(&handle);
                let _ = ticker.join();
            }
            if let Ok(logger) = logger {
                let _ = logger.join();
            }
            // Not joined, for the same reason `shutdown` does not join it: a writer
            // stalled on a named pipe must never be able to block this call.
            drop(pipe);
            unsafe { CloseHandle(timer) };
            return ASE_HWMalfunction;
        }
    };

    session.running = Some(Running {
        shared,
        handle,
        stop,
        ticker: Some(ticker),
        logger: Some(logger),
        pipe: Some(pipe),
    });
    ASE_OK
}

/// `stop()` — 5.
///
/// asio.h only constrains `stop()` by requiring that no `bufferSwitch` is called
/// after it returns, so this signals the timer thread, wakes it out of its wait
/// and joins the timer thread before returning. The pipe thread is not joined
/// (see `Running::shutdown`) but it watches the same `stop` flag and leaves on its
/// own, so this still returns promptly even if the pipe's reader has stalled.
/// Idempotent on purpose: hosts call it
/// during teardown whether they started us or not, and that is `ASE_OK`.
unsafe extern "system" fn asio_stop(this: *mut Driver) -> ASIOError {
    let Some(driver) = (unsafe { driver_ref(this) }) else {
        return ASE_InvalidParameter;
    };
    let mut guard = driver.session.lock().unwrap_or_else(|error| error.into_inner());
    let running = guard.as_mut().and_then(|session| session.running.take());
    if let Some(running) = running {
        running.shutdown();
    }
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
/// flagged for review in a later revision instead.
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
/// While the timer is running this reports the samples it has advanced through,
/// which is what the host uses to align its own timeline. Before `start` - and
/// asio.h's own wording for that state - `ASE_SPNotAdvancing` is the answer, and
/// the host's two output pointers are then deliberately left untouched.
unsafe extern "system" fn asio_get_sample_position(
    this: *mut Driver,
    s_pos: *mut ASIOSamples,
    t_stamp: *mut ASIOTimeStamp,
) -> ASIOError {
    let Some(driver) = (unsafe { driver_ref(this) }) else {
        return ASE_InvalidParameter;
    };
    if s_pos.is_null() || t_stamp.is_null() {
        return ASE_InvalidParameter;
    }
    let guard = driver.session.lock().unwrap_or_else(|error| error.into_inner());
    let Some(shared) = guard
        .as_ref()
        .and_then(|session| session.running.as_ref())
        .map(|running| Arc::clone(&running.shared))
    else {
        return ASE_SPNotAdvancing;
    };
    // Dropped before touching the host's pointers: the timer thread only reads
    // the arc, but keeping the guard across the writes would be pointless.
    drop(guard);
    unsafe {
        *s_pos = split_u64(shared.position.load(Ordering::Acquire));
        *t_stamp = system_time();
    }
    ASE_OK
}

// ---------------------------------------------------------------------------
// IASIO — 15..=20: channel description, buffers, panel, future, outputReady
// ---------------------------------------------------------------------------

/// `getChannelInfo(ASIOChannelInfo *info)` — 15.
///
/// The host calls this once per channel it is about to activate, having filled in
/// `channel` and `isInput`; the driver fills in the rest. asio.h says the driver
/// must report `ASIOSTFloat32LSB` here if that is the type it will use, and the
/// name is `char[32]` including the terminator.
unsafe extern "system" fn asio_get_channel_info(
    _this: *mut Driver,
    info: *mut ASIOChannelInfo,
) -> ASIOError {
    if info.is_null() {
        return ASE_InvalidParameter;
    }
    let info = unsafe { &mut *info };
    // The host fills in `channel` and `isInput` before the call; the rest is ours.
    let channel = info.channel;
    let is_input = info.is_input;
    if channel < 0 || channel >= CHANNEL_COUNT {
        return ASE_InvalidParameter;
    }
    // Written as a whole-struct assignment, never through a `&mut` to a field:
    // `ASIOChannelInfo` is `packed(4)`, and borrowing a field of a packed struct
    // is the one thing rustc refuses (E0793).
    let mut name = [0 as c_char; ASIO_CHANNEL_NAME_CAP];
    let direction = if is_input == ASIO_FALSE { "out" } else { "in" };
    let label = format!("viola-bridge {direction} {}", channel + 1);
    unsafe { write_cstr(name.as_mut_ptr(), &label, ASIO_CHANNEL_NAME_CAP) };
    *info = ASIOChannelInfo {
        channel,
        is_input,
        is_active: ASIO_TRUE,
        channel_group: 0,
        sample_type: ASIOSTFloat32LSB,
        name,
    };
    ASE_OK
}

/// `createBuffers(ASIOBufferInfo *bufferInfos, long numChannels, long bufferSize, ASIOCallbacks *callbacks)` — 16.
///
/// The buffer handshake: the host hands over one `ASIOBufferInfo` per channel
/// and the four callbacks, and the driver must fill in the two halves of each
/// double buffer. Every channel of the handshake gets real memory. The host's
/// `channelNum` is range-checked because it indexes our own 16-slot tables, and
/// the output channels' `buffers[0]` pointers are captured for `Tap` to read
/// once `start` has put a timer thread on them. The input channels stay silent:
/// a host *reads* those, and M5.3 only carries what a host plays into us.
unsafe extern "system" fn asio_create_buffers(
    this: *mut Driver,
    buffer_infos: *mut ASIOBufferInfo,
    num_channels: i32,
    buffer_size: i32,
    callbacks: *mut ASIOCallbacks,
) -> ASIOError {
    if buffer_infos.is_null() || callbacks.is_null() || num_channels <= 0 {
        return ASE_InvalidParameter;
    }
    if !(MIN_BUFFER_SIZE..=MAX_BUFFER_SIZE).contains(&buffer_size) {
        return ASE_InvalidParameter;
    }
    // 16 in + 16 out is what `getChannels` promises; asking for more than that is
    // the host contradicting itself.
    if num_channels > 2 * CHANNEL_COUNT {
        return ASE_InvalidParameter;
    }
    let Some(driver) = (unsafe { driver_ref(this) }) else {
        return ASE_InvalidParameter;
    };

    let callbacks = unsafe { *callbacks };
    let mut guard = driver.session.lock().unwrap_or_else(|error| error.into_inner());
    if guard.is_some() {
        // asio.h: "If you want to change to another buffer size, call
        // ASIODisposeBuffers() first" - a second handshake without one is a bad mode.
        return ASE_InvalidMode;
    }

    // `channelNum` indexes the 16-slot table below, so it is range-checked before
    // any pointer is handed out: a host that names a channel outside 0..16 would
    // otherwise write past the end of it. A repeat is refused too - two entries
    // claiming one slot would silently drop one of them. asio.h numbers inputs and
    // outputs in their own spaces, so the check is per direction. It runs on its
    // own pass so a rejected handshake leaves every `buffers` entry exactly as the
    // host passed it.
    let mut seen = [false; 2 * CHANNEL_COUNT as usize];
    for index in 0..num_channels as usize {
        let info = unsafe { &*buffer_infos.add(index) };
        // Read by value: `ASIOBufferInfo` is a `packed(4)` mirror and a borrow of
        // one of its fields would be a reference to a packed field, which Rust
        // refuses outright.
        let is_input = info.is_input;
        let channel_num = info.channel_num;
        if !(0..CHANNEL_COUNT).contains(&channel_num) {
            return ASE_InvalidParameter;
        }
        // `is_input` is an `ASIOBool` long, so normalise it rather than trusting
        // the host to have sent exactly 0 or 1.
        let slot = (if is_input == ASIO_FALSE { 0 } else { CHANNEL_COUNT }) as usize
            + channel_num as usize;
        if seen[slot] {
            return ASE_InvalidParameter;
        }
        seen[slot] = true;
    }

    let mut outputs: [*const f32; CHANNEL_COUNT as usize] = [ptr::null(); CHANNEL_COUNT as usize];
    let mut blocks: Vec<Box<[f32]>> = Vec::with_capacity(num_channels as usize);
    for index in 0..num_channels as usize {
        let info = unsafe { &mut *buffer_infos.add(index) };
        // Both directions get real memory: a host reads the input buffers and
        // writes the output buffers. The output halves are the ones `Tap` lifts
        // once `start` runs. The input halves are filled by nobody, so a host that
        // reads them hears silence.
        let mut block = vec![0.0_f32; 2 * buffer_size as usize].into_boxed_slice();
        let base = block.as_mut_ptr();
        info.buffers[0] = base.cast::<c_void>();
        info.buffers[1] = unsafe { base.add(buffer_size as usize) }.cast::<c_void>();
        let is_input = info.is_input;
        let channel_num = info.channel_num;
        if is_input == ASIO_FALSE {
            outputs[channel_num as usize] = base as *const f32;
        }
        blocks.push(block);
    }

    *guard = Some(Session {
        blocks,
        buffer_size,
        callbacks,
        running: None,
        outputs,
    });
    ASE_OK
}

/// `disposeBuffers()` — 17.
///
/// asio.h: "If no buffer were ever prepared, `ASE_InvalidMode` will be
/// returned." A running driver is stopped first, so that no `bufferSwitch` can
/// still be on its way when the host frees its side of the arrangement.
unsafe extern "system" fn asio_dispose_buffers(this: *mut Driver) -> ASIOError {
    let Some(driver) = (unsafe { driver_ref(this) }) else {
        return ASE_InvalidParameter;
    };
    let mut guard = driver.session.lock().unwrap_or_else(|error| error.into_inner());
    match guard.take() {
        None => ASE_InvalidMode,
        Some(mut session) => {
            // `take` rather than a move: `if let Some(running) = session.running`
            // would partially move the struct and then forbid dropping it.
            if let Some(running) = session.running.take() {
                running.shutdown();
            }
            // `session` goes out of scope here, releasing the channel allocations
            // the host was pointed at - which is exactly what dispose means.
            ASE_OK
        }
    }
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
/// function" when the driver does not implement the mechanism. While the timer
/// is running we do implement it - there is no DMA to convert into, but the host
/// uses this to know the driver is ready for the next block - so it answers
/// `ASE_OK`, and before `start` it answers `ASE_NotPresent` as documented.
unsafe extern "system" fn asio_output_ready(this: *mut Driver) -> ASIOError {
    let Some(driver) = (unsafe { driver_ref(this) }) else {
        return ASE_InvalidParameter;
    };
    let guard = driver.session.lock().unwrap_or_else(|error| error.into_inner());
    let running = guard
        .as_ref()
        .is_some_and(|session| session.running.is_some());
    if running { ASE_OK } else { ASE_NotPresent }
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

/// Split a 64-bit counter into the two 32-bit halves asio.h uses.
///
/// `ASIOSamples` and `ASIOTimeStamp` are *not* 64-bit integers on the wire: they
/// are `{ hi, lo }`, and asio.h computes them as `value / 2^32` and
/// `value % 2^32`.
fn split_u64(value: u64) -> ASIOSamples {
    ASIOSamples {
        hi: (value >> 32) as u32,
        lo: (value & 0xFFFF_FFFF) as u32,
    }
}

/// The current system time in `ASIOTimeStamp`'s format: 100 ns units since
/// 1601-01-01, which is what `GetSystemTimeAsFileTime` returns.
fn system_time() -> ASIOTimeStamp {
    let mut raw: u64 = 0;
    unsafe { GetSystemTimeAsFileTime(&mut raw) };
    ASIOTimeStamp {
        hi: (raw >> 32) as u32,
        lo: (raw & 0xFFFF_FFFF) as u32,
    }
}

/// Make a waiting timer fire at once, so `stop()` does not wait a period.
fn wake(handle: &WinHandle) {
    // A small negative due time is relative and fires immediately; 0 is not
    // documented to mean "now" and a positive value would be an absolute date.
    let due: i64 = -1;
    unsafe { SetWaitableTimer(handle.0, &due, 0, ptr::null_mut(), ptr::null_mut(), 0) };
}

/// The timer thread: one waitable-timer period per `bufferSwitch`.
///
/// The thread owns the handle; `Running::shutdown` only wakes and joins it, then
/// closes the handle. Raising the priority is the whole point of having a thread
/// of our own: this is the only place that must not be late.
fn tick_loop(handle: WinHandle, period_100ns: u64, stop: Arc<AtomicU32>, shared: Arc<Tick>) {
    unsafe { SetThreadPriority(GetCurrentThread(), THREAD_PRIORITY_TIME_CRITICAL) };
    // Arm against an *absolute* schedule, not "one period from now". A relative
    // re-arm adds whatever the host callback took to every period, so the driver's
    // clock runs slow - measured at ~90/s instead of the required 48000/512 =
    // 93.75/s, a 4% deficit that starves the pipe and is audible as stutter.
    // Chasing a fixed deadline keeps the long-run rate exact no matter how long a
    // single tick takes.
    let period = period_100ns as i64;
    let start = Instant::now();
    let mut next: i64 = period;
    while stop.load(Ordering::Acquire) == 0 {
        let now = (start.elapsed().as_nanos() / 100) as i64;
        next = tick_deadline(next, now, period);
        let due: i64 = -(next - now);
        let armed = unsafe {
            SetWaitableTimer(handle.0, &due, 0, ptr::null_mut(), ptr::null_mut(), 0)
        };
        if armed == 0 {
            break;
        }
        if unsafe { WaitForSingleObject(handle.0, INFINITE) } != WAIT_OBJECT_0 {
            break;
        }
        if stop.load(Ordering::Acquire) != 0 {
            break;
        }
        shared.tick();
        next += period;
    }
}

/// The deadline the tick thread should wait for, in the same 100 ns units as
/// `period`. While the schedule is on time this is just `next`; once it has
/// slipped past, the deadline advances by whole periods until it is in the
/// future again. Re-anchoring rather than firing a burst of back-to-back
/// callbacks matters because audio is realtime: a backlog of catch-up switches
/// is not a recovery, it is a second stutter. The `+ 1` guarantees the result is
/// strictly greater than `now` for every `period > 0`.
fn tick_deadline(next: i64, now: i64, period: i64) -> i64 {
    if next > now || period <= 0 {
        return next;
    }
    next + period * ((now - next) / period + 1)
}

/// Where the driver reports what it is doing. Asio has no logging of its own, and
/// a driver that runs inside someone else's process has nowhere else to leave
/// evidence - this file is the only observable the DAW cannot hide.
fn log_path() -> PathBuf {
    let base = std::env::var("ProgramData").unwrap_or_else(|_| "C:\\ProgramData".to_string());
    PathBuf::from(base).join("viola-asio").join("viola-asio.log")
}

fn append_log(line: &str) {
    let path = log_path();
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    if let Ok(mut file) = OpenOptions::new().create(true).append(true).open(&path) {
        let _ = file.write_all(line.as_bytes());
    }
}

/// Local wall-clock time for the log line, from `GetLocalTime` (minwinbase.h).
/// Formatted by hand - this crate has no date library, and the log is the only
/// place the driver's timeline can be lined up against orender's.
fn local_timestamp() -> String {
    let mut raw = LocalSystemTime {
        year: 0,
        month: 0,
        day_of_week: 0,
        day: 0,
        hour: 0,
        minute: 0,
        second: 0,
        milliseconds: 0,
    };
    unsafe { GetLocalTime(&mut raw) };
    format!(
        "{:04}-{:02}-{:02} {:02}:{:02}:{:02}.{:03}",
        raw.year, raw.month, raw.day, raw.hour, raw.minute, raw.second, raw.milliseconds
    )
}

/// The logger thread. It wakes in small slices so that `stop()` is not stuck for
/// a whole interval, and writes one line per `LOG_INTERVAL`.
fn log_loop(shared: Arc<Tick>, stop: Arc<AtomicU32>) {
    let mut previous_served = 0_u64;
    loop {
        let mut slept = Duration::ZERO;
        while slept < LOG_INTERVAL {
            if stop.load(Ordering::Acquire) != 0 {
                return;
            }
            thread::sleep(LOG_SLICE);
            slept += LOG_SLICE;
        }
        let served = shared.callbacks_served.load(Ordering::Acquire);
        let line = format!(
            "{} callbacks=+{} total={} position={} worst_gap_ms={:.3} buffer_frames={} rate={:.0} ring={}/{} dropped={} written={} connected={}\n",
            local_timestamp(),
            served.saturating_sub(previous_served),
            served,
            shared.position.load(Ordering::Acquire),
            shared.worst_gap_ns.load(Ordering::Acquire) as f64 / 1_000_000.0,
            shared.buffer_size,
            shared.sample_rate,
            shared.pipeline.ring.len(),
            shared.pipeline.ring.capacity(),
            shared.pipeline.dropped_blocks.load(Ordering::Acquire),
            shared.pipeline.samples_written.load(Ordering::Acquire),
            shared.pipeline.connected.load(Ordering::Acquire),
        );
        previous_served = served;
        append_log(&line);
    }
}

impl Tick {
    /// Called once per buffer from the timer thread. It must not allocate, lock
    /// or touch the file system: the host's own audio callback runs inside it.
    fn tick(&self) {
        let now_ns = self.started.elapsed().as_nanos() as u64;
        let previous = self.last_tick_ns.swap(now_ns, Ordering::AcqRel);
        if previous != 0 {
            let gap = now_ns.saturating_sub(previous);
            if gap > self.worst_gap_ns.load(Ordering::Acquire) {
                self.worst_gap_ns.store(gap, Ordering::Release);
            }
        }

        let index = self.buffer_index.load(Ordering::Acquire);
        self.buffer_index.store(index ^ 1, Ordering::Release);
        let position = self.position.fetch_add(self.buffer_size as u64, Ordering::AcqRel);
        self.callbacks_served.fetch_add(1, Ordering::AcqRel);

        // The half the host has just finished writing is `index ^ 1`: asio.h says
        // `index` is the buffer it is *about to fill*. `half` is that half's frame
        // offset inside `buffers[0]`, which is what `Tap` takes. This happens
        // before the host is called back, so the block on the wire is the one it
        // just finished - not the one it is filling now.
        let half = (index ^ 1) as usize * self.buffer_size as usize;
        let block = self.tap.fill(half);
        if !self.pipeline.ring.write_block(block) {
            // The ring is full, so a reader is behind. Dropping this block is the
            // documented policy: overwriting unread audio would corrupt the stream
            // instead of losing a slice of it. The count is what makes that visible.
            self.pipeline.dropped_blocks.fetch_add(1, Ordering::Release);
        }
        // asio.h documents two ways to tell the host, and the SDK sample prefers
        // the time-info form when the host offered it.
        if let Some(time_info) = self.callbacks.buffer_switch_time_info {
            let mut time = self.build_time(position);
            unsafe { time_info(&mut time, index as i32, ASIO_TRUE) };
        } else if let Some(switch) = self.callbacks.buffer_switch {
            unsafe { switch(index as i32, ASIO_TRUE) };
        }
    }

    /// The `ASIOTime` handed to `bufferSwitchTimeInfo`: speed, system time,
    /// sample position and sample rate, and no time code.
    fn build_time(&self, position: u64) -> ASIOTime {
        ASIOTime {
            reserved: [0; 4],
            time_info: AsioTimeInfo {
                speed: 1.0,
                system_time: system_time(),
                sample_position: split_u64(position),
                sample_rate: self.sample_rate,
                flags: TIME_INFO_FLAGS_VALID,
                reserved: [0; 12],
            },
            time_code: ASIOTimeCode {
                speed: 0.0,
                time_code_samples: split_u64(0),
                flags: 0,
                future: [0; 64],
            },
        }
    }
}

impl Running {
    /// Stop the cadence and release the timer. Returns only once no callback can
    /// still be in flight, which is what asio.h's `stop()` promises the host.
    fn shutdown(self) {
        self.stop.store(1, Ordering::Release);
        wake(&self.handle);
        if let Some(ticker) = self.ticker {
            let _ = ticker.join();
        }
        if let Some(logger) = self.logger {
            let _ = logger.join();
        }
        // `self.pipe` is deliberately dropped here, never joined. asio.h's `stop()`
        // promises the host that no callback is still in flight, and the timer
        // thread is what calls back; the pipe thread only writes to a named pipe,
        // and joining it could block this call forever if the reader has stalled -
        // which would hang the DAW. It sees `stop` and exits on its own.
        let _ = self.pipe;
        unsafe { CloseHandle(self.handle.0) };
    }
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
            assert_eq!(buf[NO_ERROR_MESSAGE.len()], 0);
            assert!(buf[NO_ERROR_MESSAGE.len() + 1..].iter().all(|b| *b == 0x7F));
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
                // Null out-parameters are reported, like every other method here,
                // rather than silently read as "not advancing".
                ASE_InvalidParameter
            );
        });
    }

    #[test]
    fn create_buffers_validates_arguments_then_answers_ok() {
        with_driver(|driver| {
            let mut info: ASIOBufferInfo = unsafe { core::mem::zeroed() };
            let mut callbacks: ASIOCallbacks = unsafe { core::mem::zeroed() };

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
            // A well-formed handshake is the one case that must succeed.
            assert_eq!(
                unsafe { asio_create_buffers(driver, &mut info, 1, 512, &mut callbacks) },
                ASE_OK
            );
            assert_eq!(unsafe { asio_dispose_buffers(driver) }, ASE_OK);
        });
    }

    #[test]
    fn create_buffers_rejects_a_buffer_size_outside_the_window() {
        with_driver(|driver| {
            let mut info: ASIOBufferInfo = unsafe { core::mem::zeroed() };
            let mut callbacks: ASIOCallbacks = unsafe { core::mem::zeroed() };

            for rejected in [32, 4096] {
                assert_eq!(
                    unsafe { asio_create_buffers(driver, &mut info, 1, rejected, &mut callbacks) },
                    ASE_InvalidParameter
                );
            }
            // 16 in + 16 out is all `getChannels` promises.
            assert_eq!(
                unsafe { asio_create_buffers(driver, &mut info, 33, 512, &mut callbacks) },
                ASE_InvalidParameter
            );
        });
    }

    #[test]
    fn create_buffers_hands_out_two_stable_halves_per_channel() {
        with_driver(|driver| {
            let mut infos: [ASIOBufferInfo; 2] = unsafe { core::mem::zeroed() };
            let mut callbacks: ASIOCallbacks = unsafe { core::mem::zeroed() };
            // Two distinct output channels: a repeated `channelNum` is refused, so
            // the second entry has to name a slot of its own.
            infos[1].channel_num = 1;
            assert_eq!(
                unsafe { asio_create_buffers(driver, infos.as_mut_ptr(), 2, 512, &mut callbacks) },
                ASE_OK
            );
            for (index, info) in infos.iter().enumerate() {
                let first = info.buffers[0];
                let second = info.buffers[1];
                assert!(!first.is_null(), "channel {index} has no first half");
                assert!(!second.is_null(), "channel {index} has no second half");
                // The halves have to be exactly `buffer_size` f32 apart, or the
                // host would write across the boundary between them.
                let distance = second as usize - first as usize;
                assert_eq!(distance, 512 * core::mem::size_of::<f32>());
                // They start silent; M5.3 fills the outputs from the pipe.
                assert_eq!(unsafe { *(first as *const f32) }, 0.0);
            }
            assert_eq!(unsafe { asio_dispose_buffers(driver) }, ASE_OK);
        });
    }

    #[test]
    fn create_buffers_rejects_an_unknown_or_repeated_channel() {
        with_driver(|driver| {
            let mut infos: [ASIOBufferInfo; 2] = unsafe { core::mem::zeroed() };
            let mut callbacks: ASIOCallbacks = unsafe { core::mem::zeroed() };

            // Outside 0..16: it would index past the driver's own 16-slot table.
            infos[0].channel_num = CHANNEL_COUNT;
            assert_eq!(
                unsafe { asio_create_buffers(driver, infos.as_mut_ptr(), 1, 512, &mut callbacks) },
                ASE_InvalidParameter
            );

            // Two entries claiming the same slot would drop one of them silently.
            infos[0].channel_num = 3;
            infos[1].channel_num = 3;
            assert_eq!(
                unsafe { asio_create_buffers(driver, infos.as_mut_ptr(), 2, 512, &mut callbacks) },
                ASE_InvalidParameter
            );

            // The same number on an input and an output is not a repeat: asio.h
            // numbers the two directions in their own spaces.
            infos[1].channel_num = 3;
            infos[1].is_input = ASIO_TRUE;
            assert_eq!(
                unsafe { asio_create_buffers(driver, infos.as_mut_ptr(), 2, 512, &mut callbacks) },
                ASE_OK
            );
            assert_eq!(unsafe { asio_dispose_buffers(driver) }, ASE_OK);
        });
    }

    #[test]
    fn create_buffers_twice_without_disposing_is_a_bad_mode() {
        with_driver(|driver| {
            let mut info: ASIOBufferInfo = unsafe { core::mem::zeroed() };
            let mut callbacks: ASIOCallbacks = unsafe { core::mem::zeroed() };
            assert_eq!(
                unsafe { asio_create_buffers(driver, &mut info, 1, 512, &mut callbacks) },
                ASE_OK
            );
            // asio.h: to change the buffer size, dispose first.
            assert_eq!(
                unsafe { asio_create_buffers(driver, &mut info, 1, 1024, &mut callbacks) },
                ASE_InvalidMode
            );
            assert_eq!(unsafe { asio_dispose_buffers(driver) }, ASE_OK);
        });
    }

    #[test]
    fn dispose_and_start_without_buffers_report_invalid_mode() {
        with_driver(|driver| {
            assert_eq!(unsafe { asio_dispose_buffers(driver) }, ASE_InvalidMode);
            assert_eq!(unsafe { asio_start(driver) }, ASE_InvalidMode);
        });
    }

    #[test]
    fn sample_position_does_not_advance_before_start() {
        with_driver(|driver| {
            let mut position: ASIOSamples = unsafe { core::mem::zeroed() };
            let mut stamp: ASIOTimeStamp = unsafe { core::mem::zeroed() };
            assert_eq!(
                unsafe { asio_get_sample_position(driver, &mut position, &mut stamp) },
                ASE_SPNotAdvancing
            );
        });
    }

    #[test]
    fn get_channel_info_describes_float32_and_refuses_an_unknown_channel() {
        with_driver(|driver| {
            let mut info = ASIOChannelInfo {
                channel: 3,
                is_input: ASIO_TRUE,
                is_active: ASIO_FALSE,
                channel_group: -1,
                sample_type: 0,
                name: [0; ASIO_CHANNEL_NAME_CAP],
            };
            assert_eq!(unsafe { asio_get_channel_info(driver, &mut info) }, ASE_OK);
            assert_eq!(info.is_active, ASIO_TRUE);
            assert_eq!(info.channel_group, 0);
            assert_eq!(info.sample_type, ASIOSTFloat32LSB);
            let name: Vec<u8> = info
                .name
                .iter()
                .take_while(|byte| **byte != 0)
                .map(|byte| *byte as u8)
                .collect();
            assert_eq!(name, b"viola-bridge in 4".to_vec());

            let mut beyond = ASIOChannelInfo {
                channel: CHANNEL_COUNT,
                is_input: ASIO_FALSE,
                is_active: ASIO_FALSE,
                channel_group: 0,
                sample_type: 0,
                name: [0; ASIO_CHANNEL_NAME_CAP],
            };
            assert_eq!(
                unsafe { asio_get_channel_info(driver, &mut beyond) },
                ASE_InvalidParameter
            );
        });
    }

    /// How many times the driver called back into the host during the timer test.
    static SWITCHES: AtomicU64 = AtomicU64::new(0);

    unsafe extern "system" fn counting_buffer_switch(
        _time: *mut ASIOTime,
        _index: i32,
        _direct: ASIOBool,
    ) -> *mut ASIOTime {
        SWITCHES.fetch_add(1, Ordering::AcqRel);
        ptr::null_mut()
    }

    /// The M5.2 exit condition, expressed as a test: start the timer, let it run,
    /// and count how often the host's own callback was called. This is the only
    /// test that exercises the waitable timer and the `bufferSwitchTimeInfo` path
    /// end to end, and it is why CI can be trusted with this at all.
    #[test]
    fn start_drives_the_host_callback_and_stop_ends_it() {
        SWITCHES.store(0, Ordering::Release);
        with_driver(|driver| {
            let mut infos: [ASIOBufferInfo; 2] = unsafe { core::mem::zeroed() };
            let mut callbacks: ASIOCallbacks = unsafe { core::mem::zeroed() };
            infos[1].channel_num = 1;
            callbacks.buffer_switch_time_info = Some(counting_buffer_switch);
            assert_eq!(
                unsafe { asio_create_buffers(driver, infos.as_mut_ptr(), 2, 512, &mut callbacks) },
                ASE_OK
            );
            assert_eq!(unsafe { asio_start(driver) }, ASE_OK);
            thread::sleep(Duration::from_millis(400));
            assert_eq!(unsafe { asio_stop(driver) }, ASE_OK);

            // 512 frames at 48 kHz is 10.67 ms, so 400 ms should be around 37
            // callbacks. The bound is deliberately loose: a CI runner is not a
            // real-time machine, and this test exists to prove the cadence exists
            // at all, not to measure it (the log line does that).
            let served = SWITCHES.load(Ordering::Acquire);
            assert!(served >= 5, "the timer fired only {served} times in 400 ms");

            // And nothing may still be calling in after stop() returned.
            let after_stop = SWITCHES.load(Ordering::Acquire);
            thread::sleep(Duration::from_millis(150));
            assert_eq!(SWITCHES.load(Ordering::Acquire), after_stop);

            assert_eq!(unsafe { asio_dispose_buffers(driver) }, ASE_OK);
        });
    }

    /// The driver's tick must keep an exact long-run rate. Re-arming the timer
    /// relative to "now" adds the host callback's duration to every period, which
    /// measured out at ~90 callbacks/s instead of 48000/512 = 93.75/s - a 4%
    /// deficit that starves the pipe and is heard as stutter. This test simulates
    /// that same work-per-tick and pins the cadence, because a regression here is
    /// otherwise only visible as an audible glitch.
    #[test]
    fn tick_deadline_keeps_the_long_run_rate_exact() {
        // 512 frames at 48 kHz, in the 100 ns units `SetWaitableTimer` wants.
        let period = 106_667i64;
        // 0.45 ms - what a tick actually costs in the host's callback.
        let work = 4_500i64;
        let window = 10 * HUNDRED_NS_PER_SECOND as i64;

        let mut next = period;
        let mut now = 0i64;
        let mut ticks = 0i64;
        while now < window {
            next = tick_deadline(next, now, period);
            assert!(next > now, "deadline {next} is not in the future of {now}");
            // Deadlines only ever land on the original schedule's grid.
            assert_eq!(next % period, 0);
            now = next + work;
            ticks += 1;
        }

        // A relative re-arm would give ~900 here; the absolute schedule gives 938.
        assert!(
            (936..=940).contains(&ticks),
            "{ticks} ticks in {window} units: the schedule is not holding"
        );

        // On time, the deadline is returned untouched.
        assert_eq!(tick_deadline(period, period - 1, period), period);
        // Degenerate period must not divide by zero.
        assert_eq!(tick_deadline(5, 1_000, 0), 5);
    }
}
