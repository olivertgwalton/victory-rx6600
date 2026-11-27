//! The DirectStorage control pages. The console's factory is one static
//! object; its start (slot +0x58, run by `DStorageGetFactory`) opens an
//! adapter and asks the driver to map two pages (IOCTL 0x7800013c, two
//! addresses back); `DStorageGetMappedControlPage` gives the second once
//! that has worked. The layer's factory has no driver to ask, so it maps
//! nothing and titles get null, as on the console before the factory
//! starts. Addresses are `XCRDAPI.dll`'s.

use std::sync::Mutex;

use super::sys::{IoStatus, sys};
use super::{Adapter, HRESULT, XCrdOpenAdapter};

/// The factory's adapter and the two mapped addresses (0x10, 0x18, 0x20 of
/// the static object), once started.
struct Control {
    adapter: *mut Adapter,
    pages: [u64; 2],
}

// SAFETY: the adapter is a heap block of the library's kind, only read.
unsafe impl Send for Control {}

static CONTROL: Mutex<Option<Control>> = Mutex::new(None);

/// 0x18000d27c: a driver IOCTL whose buffer is both input and output.
fn ioctl(handle: super::sys::Handle, code: u32, buffer: &mut [u8]) -> HRESULT {
    let s = sys();
    // SAFETY: a plain event.
    let event = unsafe { (s.create_event_w)(std::ptr::null(), 0, 0, std::ptr::null()) };
    if event.is_null() {
        return super::hresult_from_win32(super::sys::last_error());
    }
    let mut io = IoStatus::default();
    let (ptr, len) = (buffer.as_mut_ptr().cast(), buffer.len() as u32);
    // SAFETY: our buffer, waited for.
    let mut status = unsafe { (s.nt_device_io_control_file)(handle, event, std::ptr::null(), std::ptr::null(), &mut io, code, ptr, len, ptr, len) };
    if status == 0x103 {
        // SAFETY: our event.
        status = unsafe { (s.nt_wait_for_single_object)(event, 0, std::ptr::null()) };
        if status >= 0 {
            status = io.status as i32;
        }
    }
    // SAFETY: our event.
    unsafe { (s.close_handle)(event) };
    super::srb::hresult(status)
}

/// 0x180010890: the factory's start; once it has worked, nothing more.
pub(crate) fn initialize() -> HRESULT {
    let mut control = CONTROL.lock().unwrap_or_else(|e| e.into_inner());
    if control.is_some() {
        return 0;
    }
    let mut adapter = std::ptr::null_mut();
    let hr = XCrdOpenAdapter(&mut adapter);
    if hr < 0 {
        return hr;
    }
    let mut pages = [0u8; 0x10];
    let hr = ioctl(super::handle_of(adapter), 0x7800_013c, &mut pages);
    if hr < 0 {
        // The library keeps the adapter in the factory regardless.
        return hr;
    }
    *control = Some(Control { adapter, pages: [u64::from_le_bytes(pages[..8].try_into().unwrap()), u64::from_le_bytes(pages[8..].try_into().unwrap())] });
    0
}

/// The two mapped pages, once started: the debug flags' and the counters'.
pub(crate) fn pages() -> Option<[u64; 2]> {
    CONTROL.lock().unwrap_or_else(|e| e.into_inner()).as_ref().map(|c| c.pages)
}

/// Forgets a start (tests begin from a fresh factory).
#[cfg(test)]
pub(crate) fn reset() {
    *CONTROL.lock().unwrap_or_else(|e| e.into_inner()) = None;
}

/// 0x180010d30.
pub extern "system" fn DStorageGetMappedControlPage(out: *mut u64) -> HRESULT {
    let page = CONTROL.lock().unwrap_or_else(|e| e.into_inner()).as_ref().map_or(0, |c| {
        let _ = c.adapter;
        c.pages[1]
    });
    // SAFETY: the caller's out (not checked).
    unsafe { *out = page };
    0
}

pub(crate) fn exports() -> Vec<crate::HleExport> {
    use crate::hle_export;
    vec![hle_export!(DStorageGetMappedControlPage)]
}
