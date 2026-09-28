// SPDX-License-Identifier: GPL-3.0-or-later
//! The one GUID this driver is registered under — used as the CLSID **and** as
//! the `IASIO` interface IID.
//!
//! `docs/viola-asio-contract.md` freezes the value as
//! `{6C1E7D94-3A52-4B8F-9E27-5D0B4C8A1F63}` and says it is never generated at
//! install time: "the host passes it back to us as `riid`, so CLSID and IID must
//! be the same value forever". The same literal is written into
//! `scripts/register-asio.ps1`; if the two ever disagree the host hands us a
//! `riid` that nothing answers and the driver silently fails to load.
//!
//! Why one GUID for both: `host/pc/asiolist.cpp:227` (quoted in
//! `docs/asio-driver-notes.md` §1) instantiates a driver with
//!
//! ```c
//! rc = CoCreateInstance(lpdrv->clsid, 0, CLSCTX_INPROC_SERVER, lpdrv->clsid, asiodrv);
//! ```
//!
//! — the fourth argument is the *interface* IID, and it is the CLSID again. The
//! SDK's own sample follows the same convention with `IID_ASIO_DRIVER`.

/// A COM `GUID`/`CLSID`/`IID`.
///
/// The layout is the Windows SDK's `GUID` (`winnt.h`): `unsigned long` is 32-bit
/// on Windows (LLP64), so the whole type is 16 bytes with 4-byte alignment.
///
/// `data1`/`data2`/`data3` are stored in host byte order and `data4` is a plain
/// byte array; `to_bytes` flattens the two into the 16 bytes COM actually
/// compares, and the SDK's C initialisers write the fields in exactly this
/// order (`{ 0x188135e1, 0xd565, 0x11d2, { 0x85, 0x4f, ... } }` in
/// `docs/asio-driver-notes.md` §1).
#[repr(C)]
#[derive(Debug, Clone, Copy, Eq)]
pub struct Guid {
    pub data1: u32,
    pub data2: u16,
    pub data3: u16,
    pub data4: [u8; 8],
}

impl Guid {
    /// Field-by-field equality.
    ///
    /// `const` on purpose: it is also how `PartialEq` is implemented, so a
    /// `Guid` can be compared without allocating, without trait objects, and in
    /// a `const` context.
    pub(crate) const fn equals(&self, other: &Guid) -> bool {
        if self.data1 != other.data1 || self.data2 != other.data2 || self.data3 != other.data3 {
            return false;
        }
        let mut i = 0;
        while i < 8 {
            if self.data4[i] != other.data4[i] {
                return false;
            }
            i += 1;
        }
        true
    }

    /// The 16 bytes as they sit in memory, i.e. as the host's `CLSID` comparison
    /// sees them.
    ///
    /// The first three fields are integers and therefore little-endian on
    /// Windows, so `{6C1E7D94-...}` starts with the byte `0x94` — the classic
    /// GUID byte order that trips people up when reading a `.reg` file.
    ///
    /// No runtime path reads the bytes yet — the tests below and the registry
    /// script's documentation are the only consumers — so the lint would call
    /// this dead ahead of its first real caller.
    #[allow(dead_code)]
    pub(crate) const fn to_bytes(&self) -> [u8; 16] {
        let d1 = self.data1.to_le_bytes();
        let d2 = self.data2.to_le_bytes();
        let d3 = self.data3.to_le_bytes();
        [
            d1[0], d1[1], d1[2], d1[3], d2[0], d2[1], d3[0], d3[1], self.data4[0], self.data4[1],
            self.data4[2], self.data4[3], self.data4[4], self.data4[5], self.data4[6],
            self.data4[7],
        ]
    }
}

impl PartialEq for Guid {
    fn eq(&self, other: &Guid) -> bool {
        self.equals(other)
    }
}

/// Our CLSID, which is also the `IASIO` IID (`docs/viola-asio-contract.md`).
pub(crate) const CLSID_VIOLA_ASIO: Guid = Guid {
    data1: 0x6C1E_7D94,
    data2: 0x3A52,
    data3: 0x4B8F,
    data4: [0x9E, 0x27, 0x5D, 0x0B, 0x4C, 0x8A, 0x1F, 0x63],
};

/// `IID_IUnknown`, `{00000000-0000-0000-C000-000000000046}` (`unknwn.h`; the
/// SDK's `common/combase.h` reaches the same constant).
pub(crate) const IID_IUNKNOWN: Guid = Guid {
    data1: 0,
    data2: 0,
    data3: 0,
    data4: [0xC0, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x46],
};

/// `IID_IClassFactory`, `{00000001-0000-0000-C000-000000000046}` (`unknwn.h`).
///
/// `DllGetClassObject` must accept this one alongside `IID_IUnknown`
/// (`common/dllentry.cpp`: `if ((riid == IID_IUnknown) || (riid ==
/// IID_IClassFactory))`).
pub(crate) const IID_ICLASSFACTORY: Guid = Guid {
    data1: 1,
    data2: 0,
    data3: 0,
    data4: [0xC0, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x46],
};

/// Borrow the 16 bytes at `ptr` as a `Guid`; `None` when the host passed null.
///
/// # Safety
///
/// `ptr` must be null, or point to a readable, live `Guid` (16 bytes, and it
/// must stay live for `'a`). COM passes `REFIID`/`REFCLSID`, i.e. always a real
/// pointer, but a host with a bug must not take the DAW down with it — hence
/// the null check at every FFI boundary.
pub(crate) unsafe fn guid_ref<'a>(ptr: *const Guid) -> Option<&'a Guid> {
    if ptr.is_null() {
        None
    } else {
        Some(unsafe { &*ptr })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clsid_matches_the_contract_fields() {
        assert_eq!(CLSID_VIOLA_ASIO.data1, 0x6C1E_7D94);
        assert_eq!(CLSID_VIOLA_ASIO.data2, 0x3A52);
        assert_eq!(CLSID_VIOLA_ASIO.data3, 0x4B8F);
        assert_eq!(
            CLSID_VIOLA_ASIO.data4,
            [0x9E, 0x27, 0x5D, 0x0B, 0x4C, 0x8A, 0x1F, 0x63]
        );
    }

    #[test]
    fn clsid_bytes_are_the_on_the_wire_order() {
        // {6C1E7D94-...} starts 0x94 in memory because data1 is little-endian.
        assert_eq!(
            CLSID_VIOLA_ASIO.to_bytes(),
            [
                0x94, 0x7D, 0x1E, 0x6C, 0x52, 0x3A, 0x8F, 0x4B, 0x9E, 0x27, 0x5D, 0x0B, 0x4C,
                0x8A, 0x1F, 0x63,
            ]
        );
    }

    #[test]
    fn guid_has_the_windows_guid_shape() {
        // `winnt.h`: unsigned long + unsigned short + unsigned short + char[8].
        assert_eq!(core::mem::size_of::<Guid>(), 16);
        assert_eq!(core::mem::align_of::<Guid>(), 4);
    }

    #[test]
    fn iid_iunknown_is_the_com_constant() {
        assert_eq!(IID_IUNKNOWN.data1, 0);
        assert_eq!(IID_IUNKNOWN.data2, 0);
        assert_eq!(IID_IUNKNOWN.data3, 0);
        assert_eq!(IID_IUNKNOWN.data4, [0xC0, 0, 0, 0, 0, 0, 0, 0x46]);
        assert_eq!(
            IID_IUNKNOWN.to_bytes(),
            [0, 0, 0, 0, 0, 0, 0, 0, 0xC0, 0, 0, 0, 0, 0, 0, 0x46]
        );
    }

    #[test]
    fn iid_iclassfactory_is_the_com_constant() {
        assert_eq!(IID_ICLASSFACTORY.data1, 1);
        assert_eq!(
            IID_ICLASSFACTORY.to_bytes(),
            [1, 0, 0, 0, 0, 0, 0, 0, 0xC0, 0, 0, 0, 0, 0, 0, 0x46]
        );
    }

    #[test]
    fn the_three_constants_are_distinct() {
        assert_ne!(CLSID_VIOLA_ASIO, IID_IUNKNOWN);
        assert_ne!(CLSID_VIOLA_ASIO, IID_ICLASSFACTORY);
        assert_ne!(IID_IUNKNOWN, IID_ICLASSFACTORY);
    }

    #[test]
    fn equality_compares_every_field() {
        let mut other = CLSID_VIOLA_ASIO;
        assert!(CLSID_VIOLA_ASIO.equals(&other));
        assert_eq!(CLSID_VIOLA_ASIO, other);

        other.data4[7] = 0x00;
        assert!(!CLSID_VIOLA_ASIO.equals(&other));
        assert_ne!(CLSID_VIOLA_ASIO, other);

        let mut third = CLSID_VIOLA_ASIO;
        third.data1 ^= 1;
        assert!(!CLSID_VIOLA_ASIO.equals(&third));
    }

    #[test]
    fn null_guid_pointer_reads_as_none() {
        assert!(unsafe { guid_ref(core::ptr::null()) }.is_none());
        assert!(unsafe { guid_ref(&CLSID_VIOLA_ASIO) }.is_some());
    }
}
