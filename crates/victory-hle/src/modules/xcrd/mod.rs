//! `XCRDAPI.dll`'s content and storage client (`XCrd*`, `XCloud*`): the
//! console's library for its storage driver (XVD packages, storage
//! devices, blobs, streaming installs). A port of the library's own code:
//! every export checks its arguments as the console's does, builds the same
//! `SRB_IO_CONTROL` block for the driver and sends it the same way
//! ([`srb`]); local work (paths, headers, error classes) is done the same.
//! The driver is the system's: a title under the layer has none
//! (`XCrdOpenAdapter` finds no `\\.\ScsiN:` that answers), so whatever a
//! title gets is what the console library would get without its driver.
//! Each function names the address of the code it follows.
#![allow(non_snake_case)]
#![allow(clippy::not_unsafe_ptr_arg_deref)]
#![allow(clippy::too_many_arguments)]

pub mod ops;
pub mod paths;
pub mod blobs;
pub mod disks;
pub mod headers;
pub mod info;
pub mod find;
pub mod mount;
pub mod notify;
pub mod regions;
pub mod storage;
pub mod xas;
pub mod xds;
pub mod srb;
pub mod sys;
#[cfg(test)]
mod reference;
#[cfg(test)]
mod tests;

use std::ffi::c_void;

use srb::{Payload, check, copy_string, log, send};
use sys::{Handle, sys};

use crate::{HleExport, hle_export};

pub type HRESULT = i32;
pub const S_OK: HRESULT = 0;
pub const E_INVALIDARG: HRESULT = 0x8007_0057_u32 as i32;
pub const E_OUTOFMEMORY: HRESULT = 0x8007_0008_u32 as i32;
pub const E_NOTIMPL: HRESULT = 0x8000_4001_u32 as i32;
/// `HRESULT_FROM_WIN32(ERROR_NOT_SUPPORTED)`.
pub const NOT_SUPPORTED: HRESULT = 0x8007_0032_u32 as i32;

pub fn hresult_from_win32(error: u32) -> HRESULT {
    if error as i32 <= 0 { error as i32 } else { (error & 0xffff | 0x8007_0000) as i32 }
}

/// A NUL-terminated wide string, as text.
///
/// # Safety
/// `text` is null or NUL-terminated.
pub unsafe fn wide_str(text: *const u16) -> String {
    String::from_utf16_lossy(unsafe { wide(text) })
}

/// The characters before the NUL.
///
/// # Safety
/// `text` is null or NUL-terminated.
pub unsafe fn wide<'a>(text: *const u16) -> &'a [u16] {
    if text.is_null() {
        return &[];
    }
    // SAFETY: by the caller.
    unsafe {
        let len = (0..).take_while(|&i| *text.add(i) != 0).count();
        std::slice::from_raw_parts(text, len)
    }
}

pub(crate) fn units(text: &str) -> Vec<u16> {
    text.encode_utf16().collect()
}

// The process heap, which the library allocates from (RtlAllocateHeap on
// the PEB's heap) and its callers free to.

pub(crate) fn heap_alloc(bytes: usize) -> *mut u8 {
    use windows_sys::Win32::System::Memory::{GetProcessHeap, HeapAlloc};
    // SAFETY: the process heap.
    unsafe { HeapAlloc(GetProcessHeap(), 0, bytes) }.cast()
}

pub(crate) fn heap_free(block: *mut c_void) {
    use windows_sys::Win32::System::Memory::{GetProcessHeap, HeapFree};
    if !block.is_null() {
        // SAFETY: a block of the process heap.
        unsafe { HeapFree(GetProcessHeap(), 0, block) };
    }
}

// Paths. A CRD path starts with a domain prefix; the library's table of
// them (0x180013170), by domain number. Empty entries are unused numbers.

pub const DOMAINS: [&str; 37] = [
    "[XTE:]", "[XUC:]", "[XSS:]", "[XSU:]", "", "[XFG:]", "", "[XTF:]", "[XBL:]", "[XOD:]", "[XVE:]", "[XT0:]",
    "[XT1:]", "[XT2:]", "[XT3:]", "[XT4:]", "[XT5:]", "[XT6:]", "[XT7:]", "[XE0:]", "[XE1:]", "[XE2:]", "[XE3:]",
    "[XE4:]", "[XE5:]", "[XE6:]", "[XE7:]", "[XBX:]", "[XSR:]", "", "[XAS:]", "[XRT:]", "[XRU:]", "[XRS:]",
    "[XRI:]", "", "[XCC:]",
];
/// The prefix `XCrdBuildPath`'s flag 1 puts in front (0x1800131c0).
const VERSION_DOMAIN: &str = "[XVE:]";

/// 0x180001b00: the message classes to print; only bits 0-2.
pub extern "system" fn XCrdSetTraceFlags(flags: u32) -> HRESULT {
    if flags & !7 != 0 {
        return srb::hresult(0xc000_000d_u32 as i32);
    }
    srb::TRACE.store(flags, std::sync::atomic::Ordering::Relaxed);
    S_OK
}

/// 0x180001ce0: `<prefix>\<path>`, or with flag 1 `[XVE:]\<prefix>\<path>`
/// (`"%ws\%ws\%ws"` is given the version prefix first), in a process-heap
/// buffer.
pub extern "system" fn XCrdBuildPath(out: *mut *mut u16, domain: u32, path: *const u16, flags: u32) -> HRESULT {
    if domain > 0x24 || flags & !1 != 0 || path.is_null() {
        return E_INVALIDARG;
    }
    let prefix = units(DOMAINS[domain as usize]);
    // SAFETY: a NUL-terminated path.
    let path = unsafe { wide(path) };
    let mut bytes = (prefix.len() + path.len()) * 2 + 4;
    let mut text = Vec::new();
    if flags & 1 != 0 {
        let version = units(VERSION_DOMAIN);
        bytes += version.len() * 2 + 2;
        text.extend(&version);
        text.push(u16::from(b'\\'));
    }
    text.extend(&prefix);
    text.push(u16::from(b'\\'));
    text.extend(path);
    let buffer = heap_alloc(bytes).cast::<u16>();
    if buffer.is_null() {
        return E_OUTOFMEMORY;
    }
    // The buffer always holds the text and its NUL.
    // SAFETY: `bytes` covers them.
    unsafe {
        std::ptr::copy_nonoverlapping(text.as_ptr(), buffer, text.len());
        *buffer.add(text.len()) = 0;
        *out = buffer;
    }
    S_OK
}

/// 0x1800041b0: the domain whose prefix starts the path (case ignored),
/// the first in the table's order.
pub extern "system" fn XCrdParsePath(path: *const u16, domain: *mut u32) -> HRESULT {
    if path.is_null() || domain.is_null() {
        return E_INVALIDARG;
    }
    match parse_domain(path) {
        Some(i) => {
            // SAFETY: checked non-null.
            unsafe { *domain = i as u32 };
            S_OK
        }
        None => E_INVALIDARG,
    }
}

fn parse_domain(path: *const u16) -> Option<usize> {
    DOMAINS.iter().position(|prefix| {
        !prefix.is_empty() && {
            let prefix = units(prefix);
            // _wcsnicmp(prefix, path, len): the path may be shorter (its NUL
            // then differs).
            (0..prefix.len()).all(|i| {
                // SAFETY: read up to the path's NUL at most.
                let c = unsafe { *path.add(i) };
                lower(c) == lower(prefix[i])
            }) && {
                // A NUL in the path ends the comparison as a difference.
                true
            }
        }
    })
}

/// `_wcsnicmp`'s folding (C locale: ASCII only).
fn lower(c: u16) -> u16 {
    if (u16::from(b'A')..=u16::from(b'Z')).contains(&c) { c + 32 } else { c }
}

/// 0x18000a030: rewrites the path's prefix to the domain's, in place; only
/// between prefixes of the same length, and only the characters before the
/// last (the closing bracket stays).
pub extern "system" fn XCrdUpdatePath(path: *mut u16, domain: u32) -> HRESULT {
    if domain > 0x24 {
        return E_INVALIDARG;
    }
    if path.is_null() {
        return E_INVALIDARG;
    }
    let Some(current) = parse_domain(path) else {
        return E_INVALIDARG;
    };
    let new = units(DOMAINS[domain as usize]);
    if DOMAINS[current].encode_utf16().count() != new.len() {
        return E_INVALIDARG;
    }
    // SAFETY: the path holds at least the old prefix, as long as the new.
    unsafe { std::ptr::copy_nonoverlapping(new.as_ptr(), path, new.len().saturating_sub(1)) };
    S_OK
}

/// 0x180003980: the errors that mean corrupt content.
pub extern "system" fn XCrdIsCorruptionError(hr: HRESULT) -> bool {
    let hr = hr as u32;
    hr == 0x8007_01e7 || hr.wrapping_sub(0x8007_0570) <= 1 || hr == 0x8007_045d
}

/// The internal storage devices (0x1800132a0): device GUID and id.
pub(crate) const INTERNAL: [([u8; 16], u32); 2] = [
    (guid(0xa89e_ce52, 0x7e8e, 0x444f, [0xbb, 0xd0, 0xc6, 0x8b, 0x76, 0xc2, 0xec, 0xa4]), 0x1),
    (guid(0xeb2b_2f55, 0x83b3, 0x401e, [0xad, 0x7c, 0x13, 0x96, 0xd7, 0xd5, 0xb3, 0x0d]), 0x24),
];

const fn guid(a: u32, b: u16, c: u16, d: [u8; 8]) -> [u8; 16] {
    let a = a.to_le_bytes();
    let b = b.to_le_bytes();
    let c = c.to_le_bytes();
    [a[0], a[1], a[2], a[3], b[0], b[1], c[0], c[1], d[0], d[1], d[2], d[3], d[4], d[5], d[6], d[7]]
}

/// 0x1800039b0: whether the device GUID is an internal one, and its id.
pub extern "system" fn XCrdIsInternalStorageDevice(device: *const [u8; 16], id: *mut u32) -> bool {
    // SAFETY: the caller passes a GUID.
    let device = unsafe { &*device };
    match INTERNAL.iter().find(|(g, _)| g == device) {
        Some(&(_, i)) => {
            if !id.is_null() {
                // SAFETY: checked non-null.
                unsafe { *id = i };
            }
            true
        }
        None => false,
    }
}

/// 0x180003800: whether the id is an internal device's, and its GUID.
pub extern "system" fn XCrdHasInternalDeviceId(id: u32, device: *mut [u8; 16]) -> bool {
    match INTERNAL.iter().find(|&&(_, i)| i == id) {
        Some((g, _)) => {
            if !device.is_null() {
                // SAFETY: checked non-null.
                unsafe { *device = *g };
            }
            true
        }
        None => false,
    }
}

/// 0x180003220.
pub extern "system" fn XCrdFreeDevicePathBuffer(buffer: *mut c_void) -> HRESULT {
    if buffer.is_null() {
        return E_INVALIDARG;
    }
    heap_free(buffer);
    S_OK
}

/// 0x18000b620: whether an XAS key string carries the flag's setting
/// (flag 1 "PRST=1", flag 4 "NRM=1"; others, several bits, or an empty
/// string: no).
pub extern "system" fn XCrdXasKeyQueryValue(text: *const u16, flag: u32) -> u64 {
    if flag == 0 || flag & (flag - 1) != 0 {
        return 0;
    }
    let needle = match flag {
        1 => units("PRST=1"),
        4 => units("NRM=1"),
        _ => return 0,
    };
    // SAFETY: the caller passes a string.
    let text = unsafe { wide(text) };
    if text.is_empty() {
        return 0;
    }
    u64::from(text.windows(needle.len()).any(|w| w == needle.as_slice()))
}

/// 0x180004ef0: unsupported on this console.
pub extern "system" fn XCrdQueryEncryptedInfo() -> HRESULT {
    NOT_SUPPORTED
}
/// 0x180004ef0.
pub extern "system" fn XCrdQueryUnlockInfo() -> HRESULT {
    NOT_SUPPORTED
}
/// 0x180004ef0.
pub extern "system" fn XCrdRepairFiles() -> HRESULT {
    NOT_SUPPORTED
}
/// 0x180004ef0.
pub extern "system" fn XCrdTrimDeallocatedClusters() -> HRESULT {
    NOT_SUPPORTED
}
/// 0x180007290: not implemented in the library.
pub extern "system" fn XCrdShuffle() -> HRESULT {
    E_NOTIMPL
}
/// 0x180007290.
pub extern "system" fn XCrdWaitForAllRemovableDiskRemoval() -> HRESULT {
    E_NOTIMPL
}
/// 0x180007290.
pub extern "system" fn XCrdWaitForAnyRemovableDiskArrival() -> HRESULT {
    E_NOTIMPL
}

// Adapters: `{handle, port}` on the process heap (0x10 bytes).

#[repr(C)]
pub struct Adapter {
    pub handle: Handle,
    pub port: u32,
}

/// 0x180003dd0: the first `\\.\ScsiN:` (N < 64) that opens and answers the
/// driver's probe (control code 0x7800000c, the signature kept and a
/// non-negative return code); otherwise the last port's error.
pub extern "system" fn XCrdOpenAdapter(out: *mut *mut Adapter) -> HRESULT {
    let s = sys();
    let mut hr = S_OK;
    for port in 0..64u32 {
        let mut name = units(&format!("\\\\.\\Scsi{port}:"));
        name.push(0);
        // SAFETY: a NUL-terminated name.
        let handle = unsafe { (s.create_file_w)(name.as_ptr(), 0xc010_0000, 0, std::ptr::null(), 3, 0x4000_0000, std::ptr::null_mut()) };
        if handle as isize == -1 {
            hr = hresult_from_win32(sys::last_error());
            continue;
        }
        hr = probe(handle);
        if hr >= 0 {
            let adapter = heap_alloc(size_of::<Adapter>()).cast::<Adapter>();
            if !adapter.is_null() {
                // SAFETY: a new block of the adapter's size.
                unsafe {
                    adapter.write(Adapter { handle, port });
                    *out = adapter;
                }
                return S_OK;
            }
            hr = E_OUTOFMEMORY;
        }
        // SAFETY: the handle just opened.
        unsafe { (s.close_handle)(handle) };
    }
    hr
}

/// The probe: a bare header, its return code preset to
/// STATUS_INVALID_DEVICE_REQUEST, which the driver clears.
fn probe(handle: Handle) -> HRESULT {
    let s = sys();
    // SAFETY: a plain event.
    let event = unsafe { (s.create_event_w)(std::ptr::null(), 0, 0, std::ptr::null()) };
    if event.is_null() {
        return hresult_from_win32(sys::last_error());
    }
    // The header alone, its signature left 0 for the driver to fill in.
    let mut payload = Payload::new(0x7800_000c, 0);
    payload.bytes[4..12].fill(0);
    payload.bytes[0x14..0x18].copy_from_slice(&0xc000_0185_u32.to_le_bytes());
    let mut io = sys::IoStatus::default();
    let ptr = payload.bytes.as_mut_ptr().cast();
    // SAFETY: our payload, waited for.
    let mut status = unsafe {
        (s.nt_device_io_control_file)(handle, event, std::ptr::null(), std::ptr::null(), &mut io, srb::IOCTL_SCSI_MINIPORT, ptr, 0x1c, ptr, 0x1c)
    };
    if status == 0x103 {
        // SAFETY: our event.
        status = unsafe { (s.nt_wait_for_single_object)(event, 0, std::ptr::null()) };
        if status >= 0 {
            status = io.status as i32;
        }
    }
    if status >= 0 {
        status = i32::from_le_bytes(payload.bytes[0x14..0x18].try_into().unwrap());
        if status >= 0 {
            status = if payload.bytes[4..12] == srb::SIGNATURE { 0 } else { 0xc000_0185_u32 as i32 };
        }
    }
    // SAFETY: our event.
    unsafe { (s.close_handle)(event) };
    srb::hresult(status)
}

/// 0x180001f40: closes the handle, then frees the adapter (kept when the
/// close fails).
pub extern "system" fn XCrdCloseAdapter(adapter: *mut Adapter) -> HRESULT {
    // SAFETY: an adapter of ours.
    let handle = unsafe { (*adapter).handle };
    // SAFETY: its handle.
    if unsafe { (sys().close_handle)(handle) } == 0 {
        return hresult_from_win32(sys::last_error());
    }
    heap_free(adapter.cast());
    S_OK
}

/// The adapter's handle: the library reads it (a null adapter fails the
/// check first).
pub(crate) fn handle_of(adapter: *const Adapter) -> Handle {
    // SAFETY: an adapter the caller got from XCrdOpenAdapter.
    unsafe { (*adapter).handle }
}

/// A body with a path in it: `{offset 8, byte length}` and the path
/// (NUL-terminated) at the offset.
fn path_body(code: u32, path: &[u16]) -> Result<Payload, HRESULT> {
    let len = path.len() * 2 + 0xa;
    let mut payload = Payload::new(code, len);
    payload.u32(0, 8);
    payload.u32(4, (path.len() * 2) as u32);
    let status = copy_string(payload.body(), 8, path.len() * 2 + 2, path);
    if status < 0 {
        return Err(status);
    }
    Ok(payload)
}

/// 0x180001e30.
pub extern "system" fn XCrdCancelUpdate(adapter: *const Adapter, path: *const u16) -> HRESULT {
    let run = || -> Result<HRESULT, HRESULT> {
        check(!adapter.is_null(), "Invalid Adapter Handle\n")?;
        check(!path.is_null(), "Path must be specified\n")?;
        // SAFETY: a NUL-terminated path.
        let path = unsafe { wide(path) };
        check(path.len() * 2 + 0xa <= srb::MAX_BODY, "Payload too large for XVIO\n")?;
        let mut payload = path_body(0x7800_0114, path)?;
        Ok(send(handle_of(adapter), &mut payload))
    };
    run().unwrap_or_else(|hr| hr)
}

/// A metadata handle: the driver's id, on the process heap.
#[repr(C)]
pub struct Metadata {
    pub id: u32,
}

/// 0x180004000: opens the CRD path's metadata; the driver hands back its
/// id at +8 of a 0x30-byte body (`{0x30, path bytes, id}`, path at 0x30).
pub extern "system" fn XCrdOpenMetadataHandle(out: *mut *mut Metadata, adapter: *const Adapter, path: *const u16) -> HRESULT {
    let run = || -> Result<HRESULT, HRESULT> {
        check(!adapter.is_null(), "Invalid adapter handle")?;
        check(!out.is_null(), "Invalid metadata handle")?;
        check(!path.is_null(), "Invalid XCrdPath")?;
        // SAFETY: checked non-null.
        unsafe { *out = std::ptr::null_mut() };
        let metadata = heap_alloc(4).cast::<Metadata>();
        if metadata.is_null() {
            return Err(hresult_from_win32(sys::last_error()));
        }
        // SAFETY: a new block.
        unsafe { (*metadata).id = 0 };
        // SAFETY: a NUL-terminated path.
        let path = unsafe { wide(path) };
        let bytes = path.len() * 2 + 2;
        let mut payload = Payload::new(0x7800_00d0, bytes + 0x30);
        payload.u32(0, 0x30);
        payload.u32(4, bytes as u32);
        payload.u32(8, 0);
        let status = copy_string(payload.body(), 0x30, bytes, path);
        let hr = if status < 0 { status } else { send(handle_of(adapter), &mut payload) };
        if hr < 0 {
            heap_free(metadata.cast());
            return Ok(hr);
        }
        // SAFETY: ours, and the caller's out.
        unsafe {
            (*metadata).id = payload.read_u32(8);
            *out = metadata;
        }
        Ok(S_OK)
    };
    run().unwrap_or_else(|hr| hr)
}

/// 0x180001fa0: tells the driver the id (a 0x28-byte body), then frees the
/// handle and clears the caller's.
pub extern "system" fn XCrdCloseMetadataHandle(handle: *mut *mut Metadata, adapter: *const Adapter) -> HRESULT {
    // SAFETY: the caller's handle (read before the check, as the console).
    let metadata = unsafe { *handle };
    if adapter.is_null() {
        log(1, "Invalid adapter handle");
        return E_INVALIDARG;
    }
    let mut payload = Payload::new(0x7800_00d4, 0x28);
    // SAFETY: a handle of ours.
    payload.u32(0, unsafe { (*metadata).id });
    let hr = send(handle_of(adapter), &mut payload);
    if hr < 0 {
        return hr;
    }
    heap_free(metadata.cast());
    // SAFETY: the caller's handle.
    unsafe { *handle = std::ptr::null_mut() };
    S_OK
}

pub(crate) fn exports() -> Vec<HleExport> {
    let mut all = ops::exports();
    all.extend(paths::exports());
    all.extend(regions::exports());
    all.extend(storage::exports());
    all.extend(blobs::exports());
    all.extend(headers::exports());
    all.extend(info::exports());
    all.extend(disks::exports());
    all.extend(mount::exports());
    all.extend(find::exports());
    all.extend(xas::exports());
    all.extend(notify::exports());
    all.extend(xds::exports());
    all.extend([
        hle_export!(XCrdSetTraceFlags),
        hle_export!(XCrdBuildPath),
        hle_export!(XCrdParsePath),
        hle_export!(XCrdUpdatePath),
        hle_export!(XCrdIsCorruptionError),
        hle_export!(XCrdIsInternalStorageDevice),
        hle_export!(XCrdHasInternalDeviceId),
        hle_export!(XCrdFreeDevicePathBuffer),
        hle_export!(XCrdXasKeyQueryValue),
        hle_export!(XCrdQueryEncryptedInfo),
        hle_export!(XCrdQueryUnlockInfo),
        hle_export!(XCrdRepairFiles),
        hle_export!(XCrdTrimDeallocatedClusters),
        hle_export!(XCrdShuffle),
        hle_export!(XCrdWaitForAllRemovableDiskRemoval),
        hle_export!(XCrdWaitForAnyRemovableDiskArrival),
        hle_export!(XCrdOpenAdapter),
        hle_export!(XCrdCloseAdapter),
        hle_export!(XCrdCancelUpdate),
        hle_export!(XCrdOpenMetadataHandle),
        hle_export!(XCrdCloseMetadataHandle),
    ]);
    all
}
