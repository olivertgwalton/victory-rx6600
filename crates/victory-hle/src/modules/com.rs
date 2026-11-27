//! COM start-up as the console's combase does it
//! (`api-ms-win-core-com`). The console has multithreaded apartments
//! only: `CoInitializeEx` (combase +0x6a00) refuses single-threaded ones
//! with `E_NOTIMPL` unless the `ext-ms-win-rtcore-ntuser-window-ext` API
//! set is present (+0x17a5e8, `ApiSetQueryApiSetPresence`), and the
//! console's window layer (minuserx.dll) lacks most of that set (no
//! `IsWindow`, `GetQueueStatus`, hooks). Wine makes the apartment.
#![allow(non_snake_case)]

use std::ffi::c_void;

use crate::modules::files::host;
use crate::{HleModule, hle_export};

const E_NOTIMPL: i32 = 0x8000_4001_u32 as i32;
const E_INVALIDARG: i32 = 0x8007_0057_u32 as i32;

/// The flags there are: apartment threaded (2), no OLE1 DDE (4), speed
/// over memory (8); multithreaded is 0.
const KNOWN: u32 = 0xe;
/// What needs a single-threaded apartment's window machinery.
const NEEDS_WINDOWS: u32 = 0x2 | 0x8;

/// What combase +0x6a00 answers before it starts an apartment, if not that.
fn refusal(reserved: *mut c_void, flags: u32) -> Option<i32> {
    if flags & !KNOWN != 0 {
        return Some(E_INVALIDARG);
    }
    if flags & NEEDS_WINDOWS != 0 {
        return Some(E_NOTIMPL);
    }
    // Allowed only when single-threaded apartments are (and then only for
    // EXCEL.EXE), so never here.
    if !reserved.is_null() {
        return Some(E_INVALIDARG);
    }
    None
}

pub extern "system" fn CoInitializeEx(reserved: *mut c_void, flags: u32) -> i32 {
    if let Some(hr) = refusal(reserved, flags) {
        return hr;
    }
    host!("combase.dll", "CoInitializeEx": extern "system" fn(*mut c_void, u32) -> i32)(
        reserved, flags,
    )
}

pub(crate) fn module() -> HleModule {
    HleModule {
        name: "com",
        dlls: &["combase.dll", "ole32.dll", "api-ms-win-core-com-*"],
        exports: vec![hle_export!(CoInitializeEx)],
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const S_OK: i32 = 0;
    const S_FALSE: i32 = 1;

    /// combase +0x6a00: unknown flags, then single-threaded apartments and
    /// speed over memory, then the reserved pointer; Wine accepts them all.
    #[test]
    fn only_multithreaded_apartments_start() {
        std::thread::spawn(|| {
            let null = std::ptr::null_mut();
            assert_eq!(CoInitializeEx(null, 0x1), E_INVALIDARG);
            assert_eq!(CoInitializeEx(null, 0x10), E_INVALIDARG);
            assert_eq!(CoInitializeEx(null, 0x2), E_NOTIMPL);
            assert_eq!(CoInitializeEx(null, 0x6), E_NOTIMPL);
            assert_eq!(CoInitializeEx(null, 0x8), E_NOTIMPL);
            assert_eq!(CoInitializeEx(std::ptr::dangling_mut(), 0), E_INVALIDARG);
            // Nothing started so far: the thread can still join the MTA.
            assert_eq!(CoInitializeEx(null, 0x4), S_OK);
            assert_eq!(CoInitializeEx(null, 0), S_FALSE);
        })
        .join()
        .unwrap();
    }
}
