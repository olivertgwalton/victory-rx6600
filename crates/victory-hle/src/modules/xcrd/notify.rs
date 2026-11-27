//! Notifications from the system's storage (WNF state changes):
//! corruption, external storage changes, write budget. A registration is
//! the library's 0x18-byte record (subscription, context, callback).
//! Addresses are `XCRDAPI.dll`'s.

use std::ffi::c_void;

use super::sys::sys;
use super::{HRESULT, heap_alloc, heap_free};

/// The library's state names (0x180017170, 0x180017160, 0x180017168).
const CORRUPTION: u64 = 0x1989_0c35_a3bf_8875;
const EXTERNAL_STORAGE: u64 = 0x1989_0c35_a3bc_f075;
const WRITE_BUDGET: u64 = 0x1895_0c35_a3bc_2075;

#[repr(C)]
pub struct Registration {
    pub subscription: *mut c_void,
    pub context: *mut c_void,
    pub callback: Option<extern "system" fn(*mut c_void, u32)>,
}

type Handler = unsafe extern "system" fn(u64, u32, *const c_void, *mut Registration, *const u8, u32) -> i32;

/// 0x18000cac0: corruption: the callback with 0.
unsafe extern "system" fn on_corruption(_: u64, _: u32, _: *const c_void, r: *mut Registration, _: *const u8, _: u32) -> i32 {
    // SAFETY: our registration, its callback the caller's.
    unsafe { ((*r).callback.unwrap())((*r).context, 0) };
    0
}

/// 0x18000ea90: external storage: the first two words of the data.
unsafe extern "system" fn on_external_storage(_: u64, _: u32, _: *const c_void, r: *mut Registration, data: *const u8, _: u32) -> i32 {
    // SAFETY: as above; the notification's data.
    unsafe {
        let v = u32::from(data.cast::<u16>().read_unaligned()) | u32::from(data.add(2).cast::<u16>().read_unaligned()) << 16;
        ((*r).callback.unwrap())((*r).context, v);
    }
    0
}

/// 0x18000f3f0: write budget: the first word.
unsafe extern "system" fn on_write_budget(_: u64, _: u32, _: *const c_void, r: *mut Registration, data: *const u8, _: u32) -> i32 {
    // SAFETY: as above.
    unsafe { ((*r).callback.unwrap())((*r).context, u32::from(data.cast::<u16>().read_unaligned())) };
    0
}

/// 0x18000e994: the current change stamp, then a subscription from it.
fn register(name: u64, handler: Handler, callback: *const c_void, context: *mut c_void, out: *mut *mut Registration) -> HRESULT {
    let record = heap_alloc(0x18).cast::<Registration>();
    if record.is_null() {
        return super::E_OUTOFMEMORY;
    }
    let s = sys();
    // SAFETY: a new record; the system's calls as the library makes them.
    unsafe {
        (*record).context = context;
        (*record).callback = std::mem::transmute::<*const c_void, Option<extern "system" fn(*mut c_void, u32)>>(callback);
        let mut stamp = 0u32;
        let mut status = (s.rtl_query_wnf_state_data)(&mut stamp, name, handler as *const c_void, record.cast(), std::ptr::null());
        if status >= 0 {
            status = (s.rtl_subscribe_wnf)(record.cast(), name, stamp, handler as *const c_void, record.cast(), std::ptr::null(), 0, 0);
        }
        if status >= 0 {
            *out = record;
        } else {
            heap_free(record.cast());
        }
        super::srb::hresult(status)
    }
}

/// 0x180005e20 (no context).
pub extern "system" fn XCrdRegisterCorruptionCallback(callback: *const c_void, out: *mut *mut Registration) -> HRESULT {
    register(CORRUPTION, on_corruption, callback, std::ptr::null_mut(), out)
}

/// 0x180005e50.
pub extern "system" fn XCrdRegisterExternalStorageChangeCallback(callback: *const c_void, context: *mut c_void, out: *mut *mut Registration) -> HRESULT {
    register(EXTERNAL_STORAGE, on_external_storage, callback, context, out)
}

/// 0x180005e80.
pub extern "system" fn XCrdRegisterWriteBudgetCallback(callback: *const c_void, context: *mut c_void, out: *mut *mut Registration) -> HRESULT {
    register(WRITE_BUDGET, on_write_budget, callback, context, out)
}

/// 0x18000f228: ends the subscription (waiting for a callback running),
/// frees the record.
fn unregister(r: *mut Registration) -> HRESULT {
    // SAFETY: a registration of ours.
    let status = unsafe { (sys().rtl_unsubscribe_wnf)((*r).subscription) };
    heap_free(r.cast());
    super::srb::hresult(status)
}

/// 0x18000a020 (the three share it).
pub extern "system" fn XCrdUnregisterCorruptionCallback(r: *mut Registration) -> HRESULT {
    unregister(r)
}
/// 0x18000a020.
pub extern "system" fn XCrdUnregisterExternalStorageChangeCallback(r: *mut Registration) -> HRESULT {
    unregister(r)
}
/// 0x18000a020.
pub extern "system" fn XCrdUnregisterWriteBudgetCallback(r: *mut Registration) -> HRESULT {
    unregister(r)
}

pub(crate) fn exports() -> Vec<crate::HleExport> {
    use crate::hle_export;
    vec![
        hle_export!(XCrdRegisterCorruptionCallback),
        hle_export!(XCrdRegisterExternalStorageChangeCallback),
        hle_export!(XCrdRegisterWriteBudgetCallback),
        hle_export!(XCrdUnregisterCorruptionCallback),
        hle_export!(XCrdUnregisterExternalStorageChangeCallback),
        hle_export!(XCrdUnregisterWriteBudgetCallback),
    ]
}
