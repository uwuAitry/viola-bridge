// SPDX-License-Identifier: GPL-3.0-or-later
//! `IClassFactory` for the driver, the object `DllGetClassObject` hands out.
//!
//! `common/dllentry.cpp` is the shape being mirrored: its `CClassFactory` answers
//! `IID_IUnknown` and `IID_IClassFactory` with the same object pointer ("any
//! interface on this object is the object pointer"), and its `CreateInstance`
//! creates the object through the template's create function and then queries
//! the requested interface on it, dropping the creation reference if that fails.
//!
//! `IClassFactory`'s vtable (`objbase.h`) is those three `IUnknown` slots plus
//! `CreateInstance` and `LockServer`, in that order.

use core::ffi::c_void;
use core::ptr;
use core::sync::atomic::{AtomicU32, Ordering};

use crate::driver::{driver_query_interface, driver_release, Driver};
use crate::ffi::{E_NOINTERFACE, E_POINTER, HResult, S_OK};
use crate::guid::{guid_ref, Guid, IID_ICLASSFACTORY, IID_IUNKNOWN};
use crate::{LIVE_OBJECTS, SERVER_LOCKS};

/// The `IClassFactory` vtable (`objbase.h`).
///
/// The host reads these slots out of the object; we never read them back, so the
/// field lint would call them dead. They are the ABI.
#[allow(dead_code)]
#[repr(C)]
struct ClassFactoryVtbl {
    // --- IUnknown (objbase.h) -------------------------------------------------
    /// `HRESULT QueryInterface(REFIID riid, void **ppvObject)`
    query_interface: unsafe extern "system" fn(*mut ClassFactory, *const Guid, *mut *mut c_void) -> HResult,
    /// `ULONG AddRef()`
    add_ref: unsafe extern "system" fn(*mut ClassFactory) -> u32,
    /// `ULONG Release()`
    release: unsafe extern "system" fn(*mut ClassFactory) -> u32,
    // --- IClassFactory (objbase.h) -------------------------------------------
    /// `HRESULT CreateInstance(IUnknown *pUnkOuter, REFIID riid, void **ppvObject)`
    create_instance: unsafe extern "system" fn(*mut ClassFactory, *mut c_void, *const Guid, *mut *mut c_void) -> HResult,
    /// `HRESULT LockServer(BOOL fLock)`
    lock_server: unsafe extern "system" fn(*mut ClassFactory, i32) -> HResult,
}

/// One vtable for every factory instance; it holds only function pointers, so it
/// is `Sync`.
static CLASS_FACTORY_VTABLE: ClassFactoryVtbl = ClassFactoryVtbl {
    query_interface: factory_query_interface,
    add_ref: factory_add_ref,
    release: factory_release,
    create_instance: factory_create_instance,
    lock_server: factory_lock_server,
};

/// A class factory instance, as COM sees it.
#[repr(C)]
pub(crate) struct ClassFactory {
    /// Must stay first: the interface pointer COM holds is this pointer, and its
    /// first word is the vtable.
    ///
    /// Never read by *us* — COM reads it out of the object — so the field lint
    /// would call it dead. It is the ABI, not dead weight.
    #[allow(dead_code)]
    vtbl: *const ClassFactoryVtbl,
    /// COM reference count.
    ref_count: AtomicU32,
}

impl ClassFactory {
    /// A fresh factory with one reference, which the caller owns — the reference
    /// `DllGetClassObject` hands back to COM.
    pub(crate) fn new_boxed() -> Box<ClassFactory> {
        let factory = Box::new(ClassFactory {
            vtbl: &CLASS_FACTORY_VTABLE as *const ClassFactoryVtbl,
            ref_count: AtomicU32::new(1),
        });
        LIVE_OBJECTS.fetch_add(1, Ordering::AcqRel);
        factory
    }
}

/// Borrow a live factory, or `None` for a null `This`.
///
/// # Safety
///
/// `this` must be null or a pointer from `ClassFactory::new_boxed` that is still
/// alive, and it must stay live for `'a`.
unsafe fn factory_ref<'a>(this: *mut ClassFactory) -> Option<&'a ClassFactory> {
    if this.is_null() {
        None
    } else {
        Some(unsafe { &*this })
    }
}

/// `QueryInterface`: `IID_IUnknown` and `IID_IClassFactory`, both answered with
/// this same pointer (`common/dllentry.cpp`).
unsafe extern "system" fn factory_query_interface(
    this: *mut ClassFactory,
    riid: *const Guid,
    ppv: *mut *mut c_void,
) -> HResult {
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

    if *riid == IID_IUNKNOWN || *riid == IID_ICLASSFACTORY {
        unsafe { *ppv = this as *mut c_void };
        unsafe { factory_add_ref(this) };
        S_OK
    } else {
        E_NOINTERFACE
    }
}

/// `AddRef`.
///
/// # Safety
///
/// `this` must be a live factory pointer.
unsafe extern "system" fn factory_add_ref(this: *mut ClassFactory) -> u32 {
    match unsafe { factory_ref(this) } {
        Some(factory) => factory.ref_count.fetch_add(1, Ordering::AcqRel) + 1,
        None => 0,
    }
}

/// `Release`, dropping (and freeing) the factory at zero.
///
/// # Safety
///
/// `this` must be null or a live factory pointer, and must not be used again
/// once the last reference is gone.
pub(crate) unsafe extern "system" fn factory_release(this: *mut ClassFactory) -> u32 {
    let Some(factory) = (unsafe { factory_ref(this) }) else {
        return 0;
    };

    // Saturating, so a host that releases once too often cannot wrap the count
    // into a second free.
    match factory
        .ref_count
        .fetch_update(Ordering::AcqRel, Ordering::Acquire, |n| n.checked_sub(1))
    {
        Ok(previous) => {
            if previous == 1 {
                LIVE_OBJECTS.fetch_sub(1, Ordering::AcqRel);
                drop(unsafe { Box::from_raw(this) });
                0
            } else {
                previous - 1
            }
        }
        Err(_) => 0,
    }
}

/// `CreateInstance(pUnkOuter, riid, ppvObject)`.
///
/// Aggregation is refused outright: `pUnkOuter != NULL` means COM wants an inner
/// object to delegate to an outer one, and this driver has no outer object to
/// delegate to. (`common/dllentry.cpp` allows `pUnkOuter` when `riid` is
/// `IID_IUnknown`; refusing everything is the conservative subset and is what
/// `docs/viola-asio-contract.md`'s "ignores `pUnkOuter`" resolves to for a
/// non-aggregatable class.)
///
/// The reference dance is the SDK's: create with one reference, `QueryInterface`
/// the requested `riid` (which AddRefs on success), then release the reference
/// we created with. On failure `QueryInterface` has not AddRef'd, so that same
/// release drops the object — "the object will self-destruct", as
/// `common/dllentry.cpp` puts it — and `*ppvObject` stays null.
unsafe extern "system" fn factory_create_instance(
    _this: *mut ClassFactory,
    p_unk_outer: *mut c_void,
    riid: *const Guid,
    ppv: *mut *mut c_void,
) -> HResult {
    if ppv.is_null() {
        return E_POINTER;
    }
    unsafe { *ppv = ptr::null_mut() };

    if riid.is_null() {
        return E_POINTER;
    }
    if !p_unk_outer.is_null() {
        return E_NOINTERFACE;
    }

    let driver = Box::into_raw(Driver::new_boxed());
    let hr = unsafe { driver_query_interface(driver, riid, ppv) };
    unsafe { driver_release(driver) };
    hr
}

/// `LockServer(BOOL fLock)` — the host pinning us in memory.
///
/// `fLock != 0` increments, anything else decrements, and the count saturates at
/// zero so an unbalanced host cannot underflow it. `DllCanUnloadNow` consults it.
unsafe extern "system" fn factory_lock_server(
    _this: *mut ClassFactory,
    f_lock: i32,
) -> HResult {
    if f_lock != 0 {
        SERVER_LOCKS.fetch_add(1, Ordering::AcqRel);
    } else {
        let _ = SERVER_LOCKS.fetch_update(Ordering::AcqRel, Ordering::Acquire, |n| n.checked_sub(1));
    }
    S_OK
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::guid::CLSID_VIOLA_ASIO;

    fn with_factory<R>(f: impl FnOnce(*mut ClassFactory) -> R) -> R {
        let factory = Box::into_raw(ClassFactory::new_boxed());
        let result = f(factory);
        unsafe { factory_release(factory) };
        result
    }

    #[test]
    fn the_vtable_pointer_is_the_first_word_of_the_factory() {
        let raw = Box::into_raw(ClassFactory::new_boxed());
        let first_word = unsafe { *(raw as *const *const ClassFactoryVtbl) };
        assert_eq!(first_word, &CLASS_FACTORY_VTABLE as *const ClassFactoryVtbl);
        assert_eq!(unsafe { factory_release(raw) }, 0);
    }

    #[test]
    fn query_interface_answers_i_unknown_and_i_class_factory() {
        with_factory(|factory| {
            for iid in [IID_IUNKNOWN, IID_ICLASSFACTORY] {
                let mut out: *mut c_void = ptr::null_mut();
                assert_eq!(
                    unsafe { factory_query_interface(factory, &iid, &mut out) },
                    S_OK
                );
                assert_eq!(out as *mut ClassFactory, factory);
                // Drop the reference QueryInterface added.
                assert_eq!(unsafe { factory_release(factory) }, 1);
            }
        });
    }

    #[test]
    fn query_interface_rejects_our_clsid_on_the_factory() {
        // The CLSID is the *driver's* IID, not the factory's.
        with_factory(|factory| {
            let mut out: *mut c_void = ptr::null_mut();
            assert_eq!(
                unsafe { factory_query_interface(factory, &CLSID_VIOLA_ASIO, &mut out) },
                E_NOINTERFACE
            );
            assert!(out.is_null());
        });
    }

    #[test]
    fn query_interface_rejects_a_null_ppv() {
        with_factory(|factory| {
            assert_eq!(
                unsafe { factory_query_interface(factory, &IID_IUNKNOWN, ptr::null_mut()) },
                E_POINTER
            );
        });
    }

    #[test]
    fn create_instance_refuses_an_outer_object() {
        with_factory(|factory| {
            let mut out: *mut c_void = ptr::null_mut();
            // A non-null pUnkOuter means COM wants aggregation, which we refuse
            // before allocating anything; any non-null pointer will do.
            let mut outer = 0u8;
            let outer_ptr = &mut outer as *mut u8 as *mut c_void;
            let hr = unsafe {
                factory_create_instance(factory, outer_ptr, &IID_IUNKNOWN, &mut out)
            };
            assert_eq!(hr, E_NOINTERFACE);
            assert!(out.is_null());
        });
    }

    #[test]
    fn create_instance_hands_out_a_driver_for_the_clsid() {
        with_factory(|factory| {
            let mut out: *mut c_void = ptr::null_mut();
            let hr = unsafe {
                factory_create_instance(factory, ptr::null_mut(), &CLSID_VIOLA_ASIO, &mut out)
            };
            assert_eq!(hr, S_OK);
            assert!(!out.is_null());
            // Exactly one reference remains: the one the caller now owns.
            assert_eq!(unsafe { driver_release(out as *mut Driver) }, 0);
        });
    }

    #[test]
    fn create_instance_frees_the_driver_when_the_iid_is_unknown() {
        let bogus = Guid {
            data1: 0x0BAD_0BAD,
            data2: 1,
            data3: 2,
            data4: [9; 8],
        };
        with_factory(|factory| {
            let mut out: *mut c_void = ptr::null_mut();
            let hr = unsafe {
                factory_create_instance(factory, ptr::null_mut(), &bogus, &mut out)
            };
            assert_eq!(hr, E_NOINTERFACE);
            assert!(out.is_null());
        });
    }

    #[test]
    fn create_instance_rejects_a_null_riid_or_ppv() {
        with_factory(|factory| {
            let mut out: *mut c_void = ptr::null_mut();
            assert_eq!(
                unsafe {
                    factory_create_instance(
                        factory,
                        ptr::null_mut(),
                        ptr::null(),
                        &mut out
                    )
                },
                E_POINTER
            );
            assert_eq!(
                unsafe {
                    factory_create_instance(
                        factory,
                        ptr::null_mut(),
                        &CLSID_VIOLA_ASIO,
                        ptr::null_mut()
                    )
                },
                E_POINTER
            );
        });
    }

    #[test]
    fn lock_server_counts_up_and_saturates_at_zero() {
        // Only this test touches SERVER_LOCKS, so the counter is readable here.
        assert_eq!(SERVER_LOCKS.load(Ordering::Acquire), 0);
        with_factory(|factory| {
            assert_eq!(unsafe { factory_lock_server(factory, 1) }, S_OK);
            assert_eq!(SERVER_LOCKS.load(Ordering::Acquire), 1);
            assert_eq!(unsafe { factory_lock_server(factory, 0) }, S_OK);
            assert_eq!(SERVER_LOCKS.load(Ordering::Acquire), 0);
            // Unlocking more often than locking must not underflow.
            assert_eq!(unsafe { factory_lock_server(factory, 0) }, S_OK);
            assert_eq!(SERVER_LOCKS.load(Ordering::Acquire), 0);
        });
    }
}
