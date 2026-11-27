//! Storage devices and blobs: every export here fills in one request and
//! hands it to the library's storage operation (0x18000ddc0), which checks
//! it by operation, builds a 0x430-byte body (control code 0x78000088) and
//! copies the driver's answer back. Addresses are `XCRDAPI.dll`'s.

use std::ffi::c_void;

use super::srb::{HEADER, Payload, check, log, send};
use super::{Adapter, E_INVALIDARG, HRESULT, XCrdIsInternalStorageDevice, handle_of, wide, INTERNAL};

/// The library's request (0x68 bytes, on its stack).
#[repr(C)]
#[derive(Clone, Copy)]
struct Request {
    id: u32,
    op: u32,
    /// Strings: at most 0x20 characters, then three of at most 0x103.
    a: *const u16,
    b: *const u16,
    c: *const u16,
    d: *const u16,
    value: *const u32,
    guid: *const [u8; 16],
    flags: *const u64,
    check_flags: *const u32,
    blob: *const [u8; 0x218],
    console: *const u32,
    out: *mut [u8; 0x430],
    guid2: *const [u8; 16],
}

impl Request {
    fn new(id: u32, op: u32) -> Request {
        use std::ptr::null;
        Request {
            id,
            op,
            a: null(),
            b: null(),
            c: null(),
            d: null(),
            value: null(),
            guid: null(),
            flags: null(),
            check_flags: null(),
            blob: null(),
            console: null(),
            out: std::ptr::null_mut(),
            guid2: null(),
        }
    }
}

fn len(s: *const u16) -> usize {
    // SAFETY: a NUL-terminated string.
    unsafe { wide(s) }.len()
}

/// Ids of removable (0x13-0x1a) or external (0xb-0x12) storage.
fn removable_or_external(id: u32) -> bool {
    id.wrapping_sub(0x13) <= 7 || id.wrapping_sub(0xb) <= 7
}

/// 0x18000ddc0.
fn operate(adapter: *const Adapter, r: &Request) -> HRESULT {
    let invalid = || {
        log(1, "Invalid parameter.\n");
        E_INVALIDARG
    };
    // String lengths first, with no message.
    if !r.a.is_null() && len(r.a) >= 0x21 {
        return E_INVALIDARG;
    }
    for s in [r.b, r.c, r.d] {
        if !s.is_null() && len(s) >= 0x104 {
            return E_INVALIDARG;
        }
    }
    if adapter.is_null() {
        log(1, "Invalid Adapter Handle\n");
        return E_INVALIDARG;
    }
    // SAFETY: the caller's flags (checked non-null).
    if !r.flags.is_null() && unsafe { *(r.flags as *const u32) } >= 2 {
        log(1, "Invalid External Storage Flags\n");
        return E_INVALIDARG;
    }
    let not_internal = |g: *const [u8; 16]| !g.is_null() && !XCrdIsInternalStorageDevice(g, std::ptr::null_mut());
    let ok = match r.op as i32 {
        1 | 0xc => not_internal(r.guid),
        5 => !r.value.is_null(),
        6 => !r.guid.is_null(),
        8 => !r.b.is_null(),
        9 | 0x12 | 0x13 => !r.c.is_null(),
        0xd => !r.check_flags.is_null() && !INTERNAL.iter().any(|&(_, i)| i == r.id),
        0xe => !r.blob.is_null(),
        0xf => !r.c.is_null() && (r.id.wrapping_sub(0x13) <= 7 || r.id == u32::MAX),
        0x10 => !r.guid.is_null() && !r.guid2.is_null(),
        0x11 => !r.guid.is_null() && !r.out.is_null(),
        // SAFETY: checked non-null.
        0x14 => !r.console.is_null() && unsafe { *r.console as i32 } < 3 && r.id.wrapping_sub(0x13) <= 7,
        0x15 => !r.c.is_null() && !r.d.is_null(),
        _ => true,
    };
    if !ok {
        return invalid();
    }
    let mut p = Payload::new(0x7800_0088, 0x430);
    p.u32(4, r.id);
    p.u32(0, r.op);
    let op = r.op;
    let put_string = |p: &mut Payload, at: usize, s: *const u16| {
        // SAFETY: a NUL-terminated string (length checked above).
        let units = unsafe { std::slice::from_raw_parts(s, len(s) + 1) };
        let bytes: Vec<u8> = units.iter().flat_map(|u| u.to_le_bytes()).collect();
        p.put(at, &bytes);
    };
    if (op.wrapping_sub(1) <= 1 || op == 0xc) && !r.a.is_null() {
        put_string(&mut p, 0x20, r.a);
    }
    if op <= 0x13 && 0xc_8210_u32 & (1 << op) != 0 && !r.c.is_null() {
        put_string(&mut p, 0x20, r.c);
    }
    // SAFETY (all reads below): pointers the checks above required.
    unsafe {
        if op == 0x15 {
            if !r.c.is_null() && !r.d.is_null() {
                put_string(&mut p, 0x20, r.c);
                put_string(&mut p, 0x228, r.d);
            }
        } else if op == 5 {
            if !r.value.is_null() {
                p.u32(8, *r.value);
            }
        } else {
            if op <= 0xc && 0x1042_u32 & (1 << op) != 0 && !r.guid.is_null() {
                p.put(0xc, &*r.guid);
            }
            match op {
                8 => {
                    if !r.b.is_null() {
                        put_string(&mut p, 0x20, r.b);
                    }
                }
                0xa => p.u64(0x20, *r.flags),
                0xd => p.u32(0x20, *r.check_flags),
                0xe => p.put(0x20, &*r.blob),
                0x10 => {
                    p.put(0xc, &*r.guid);
                    p.put(0x20, &*r.guid2);
                }
                _ => {}
            }
        }
        if op != 0x10 {
            if op == 0x11 {
                p.put(0xc, &*r.guid);
            } else if op == 0x14 {
                p.u32(0x20, *r.console);
            }
        }
    }
    let hr = send(handle_of(adapter), &mut p);
    if hr >= 0 && !r.out.is_null() {
        // SAFETY: the 0x430-byte buffer the request names.
        unsafe { (*r.out).copy_from_slice(&p.bytes[HEADER..]) };
    }
    hr
}

fn result(r: Result<HRESULT, HRESULT>) -> HRESULT {
    r.unwrap_or_else(|hr| hr)
}

/// 0x180003150: the id of the device with this GUID (+8 of the answer).
pub extern "system" fn XCrdFindStorageDevice(adapter: *const Adapter, device: *const [u8; 16], id: *mut u32) -> HRESULT {
    result((|| {
        check(!adapter.is_null(), "Invalid Adapter Handle.\n")?;
        check(!id.is_null(), "Invalid CRD ID pointer.\n")?;
        let mut out = [0u8; 0x430];
        let mut r = Request::new(0x25, 6);
        r.guid = device;
        r.out = &mut out;
        let hr = operate(adapter, &r);
        if hr >= 0 {
            // SAFETY: checked non-null.
            unsafe { *id = u32::from_le_bytes(out[8..12].try_into().unwrap()) };
        }
        Ok(hr)
    })())
}

/// 0x180003260: the device id mapped to this one (+0x20 of the answer).
pub extern "system" fn XCrdGetDeviceIdMapping(adapter: *const Adapter, device: *const [u8; 16], target: *mut [u8; 16]) -> HRESULT {
    result((|| {
        check(!adapter.is_null(), "Invalid Adapter Handle.\n")?;
        check(!target.is_null(), "Target DeviceId is null.\n")?;
        let mut out = [0u8; 0x430];
        let mut r = Request::new(0x25, 0x11);
        r.guid = device;
        r.out = &mut out;
        let hr = operate(adapter, &r);
        if hr >= 0 {
            // SAFETY: checked non-null.
            unsafe { (*target).copy_from_slice(&out[0x20..0x30]) };
        }
        Ok(hr)
    })())
}

/// 0x180006640.
pub extern "system" fn XCrdSetDeviceIdMapping(adapter: *const Adapter, device: *const [u8; 16], target: *const [u8; 16]) -> HRESULT {
    result((|| {
        check(!adapter.is_null(), "Invalid Adapter Handle.\n")?;
        let mut r = Request::new(0x25, 0x10);
        r.guid = device;
        r.guid2 = target;
        Ok(operate(adapter, &r))
    })())
}

/// 0x180006820.
pub extern "system" fn XCrdSetEraLaunchMapping(adapter: *const Adapter, path: *const u16, id: u32) -> HRESULT {
    let mut r = Request::new(id, 4);
    r.c = path;
    operate(adapter, &r)
}

/// 0x1800069f0.
pub extern "system" fn XCrdSetExternalPlsMapping(adapter: *const Adapter, path: *const u16, id: u32) -> HRESULT {
    let mut r = Request::new(id, 0xf);
    r.c = path;
    operate(adapter, &r)
}

/// 0x1800072a0: attaches external storage (ids 0xb-0x12); its address
/// (lun, path, target, port) and the id come back as 8 bytes.
pub extern "system" fn XCrdStorageAttachExternalDevice(address: *mut [u8; 8], adapter: *const Adapter, id: u32, flag: u8) -> HRESULT {
    result((|| {
        check(!adapter.is_null(), "Invalid Adapter Handle.\n")?;
        check(id.wrapping_sub(0xb) <= 7, "Invalid XCrdId.\n")?;
        let flags = u64::from(flag & 1);
        let mut out = [0u8; 0x430];
        let mut r = Request::new(id, 0xa);
        r.flags = &flags;
        r.out = &mut out;
        let hr = operate(adapter, &r);
        if hr >= 0 {
            let a = &out[0x24..0x28];
            log(4, &format!("[XCRD]: Attached device address Lun:{}, Path:{}, Target:{}, Port:{}\n", a[0], a[1], a[2], a[3]));
            let mut v = [0u8; 8];
            v[..4].copy_from_slice(a);
            v[4..].copy_from_slice(&id.to_le_bytes());
            // SAFETY: the caller's (not checked).
            unsafe { *address = v };
        }
        Ok(hr)
    })())
}

/// 0x1800073e0: flags bits 0-1, checked before the adapter.
pub extern "system" fn XCrdStorageCheck(adapter: *const Adapter, id: u32, flags: u32) -> HRESULT {
    result((|| {
        check(flags & !3 == 0, "Invalid Check Flags\n")?;
        let value = flags & 3;
        let mut out = [0u8; 0x430];
        let mut r = Request::new(id, 0xd);
        r.check_flags = &value;
        r.out = &mut out;
        Ok(operate(adapter, &r))
    })())
}

/// 0x1800074d0.
pub extern "system" fn XCrdStorageConnectDevice(adapter: *const Adapter, path: *const u16) -> HRESULT {
    result((|| {
        check(!adapter.is_null(), "Invalid Adapter Handle.\n")?;
        check(!path.is_null(), "Invalid VolumePath.\n")?;
        let mut r = Request::new(0x25, 8);
        r.b = path;
        Ok(operate(adapter, &r))
    })())
}

/// 0x180007560: a blob of `size` at the path (under 0x104 characters).
pub extern "system" fn XCrdStorageCreateBlob(adapter: *const Adapter, path: *const u16, size: u64, flags: u32) -> HRESULT {
    result((|| {
        check(!path.is_null(), "Invalid Input Path.\n")?;
        check(len(path) < 0x104, "XCrdPath length is too long.\n")?;
        check(flags == 0, "Flags must be zero.\n")?;
        let mut blob = [0u8; 0x218];
        // SAFETY: the path and its NUL (under 0x104 characters).
        let units = unsafe { std::slice::from_raw_parts(path, len(path) + 1) };
        for (i, u) in units.iter().enumerate() {
            blob[2 * i..2 * i + 2].copy_from_slice(&u.to_le_bytes());
        }
        blob[0x208..0x210].copy_from_slice(&size.to_le_bytes());
        let mut r = Request::new(0x25, 0xe);
        r.blob = &blob;
        Ok(operate(adapter, &r))
    })())
}

/// 0x180007680.
pub extern "system" fn XCrdStorageCreateDirectory(adapter: *const Adapter, path: *const u16, flags: u32) -> HRESULT {
    result((|| {
        check(flags == 0, "Flags must be zero.\n")?;
        let mut r = Request::new(0x25, 0x12);
        r.c = path;
        Ok(operate(adapter, &r))
    })())
}

/// 0x180007700.
pub extern "system" fn XCrdStorageDeleteBlob(adapter: *const Adapter, path: *const u16) -> HRESULT {
    let mut r = Request::new(0x25, 9);
    r.c = path;
    operate(adapter, &r)
}

/// 0x180007760.
pub extern "system" fn XCrdStorageDeleteDirectory(adapter: *const Adapter, path: *const u16) -> HRESULT {
    let mut r = Request::new(0x25, 0x13);
    r.c = path;
    operate(adapter, &r)
}

/// 0x1800078f0: disconnects removable or external storage; its volume
/// path comes back with its length (both counting the NUL); a path longer than the
/// caller's room reconnects the device and is ERROR_INSUFFICIENT_BUFFER.
pub extern "system" fn XCrdStorageDisconnectDevice(adapter: *const Adapter, id: u32, path: *mut u16, count: *mut u32) -> HRESULT {
    result((|| {
        check(!adapter.is_null(), "Invalid Adapter Handle.\n")?;
        check(removable_or_external(id), "Invalid XCrdId.\n")?;
        check(!path.is_null(), "Invalid VolumePath.\n")?;
        check(!count.is_null(), "Invalid VolumePathCount.\n")?;
        let mut out = [0u8; 0x430];
        let mut r = Request::new(id, 7);
        r.out = &mut out;
        let mut hr = operate(adapter, &r);
        if hr < 0 {
            return Ok(hr);
        }
        // The path and its NUL, counted with the NUL.
        let mut volume: Vec<u16> = out[0x20..].chunks(2).map(|c| u16::from_le_bytes([c[0], c[1]])).take_while(|&c| c != 0).collect();
        volume.push(0);
        // SAFETY: checked non-null.
        unsafe {
            if volume.len() as u64 > u64::from(*count) {
                XCrdStorageConnectDevice(adapter, volume.as_ptr());
                hr = 0x8007_007a_u32 as i32;
            } else {
                std::ptr::copy_nonoverlapping(volume.as_ptr(), path, volume.len());
            }
            *count = volume.len() as u32;
        }
        Ok(hr)
    })())
}

/// 0x180007a30: formats (flag 1: a full format, op 0xc; else op 1) a
/// device by GUID, with a name; the GUID comes back from +0xc.
pub extern "system" fn XCrdStorageFormat(adapter: *const Adapter, id: u32, flags: u32, name: *const u16, device: *mut [u8; 16]) -> HRESULT {
    result((|| {
        check(flags & !1 == 0, "Invalid Format Flags\n")?;
        let mut out = [0u8; 0x430];
        let mut r = Request::new(id, if flags & 1 != 0 { 0xc } else { 1 });
        r.a = name;
        r.guid = device;
        r.out = &mut out;
        let hr = operate(adapter, &r);
        if hr >= 0 && !device.is_null() {
            // SAFETY: checked non-null.
            unsafe { (*device).copy_from_slice(&out[0xc..0x1c]) };
        }
        Ok(hr)
    })())
}

/// 0x180007b20: the flags are checked after the request is built.
pub extern "system" fn XCrdStorageMoveBlob(adapter: *const Adapter, source: *const u16, dest: *const u16, flags: u32) -> HRESULT {
    result((|| {
        let mut r = Request::new(0x25, 0x15);
        r.c = source;
        r.d = dest;
        check(flags == 0, "Flags must be zero.\n")?;
        Ok(operate(adapter, &r))
    })())
}

/// 0x180007f30: both ids removable or external.
pub extern "system" fn XCrdStorageRemapDevice(adapter: *const Adapter, existing: u32, new: u32) -> HRESULT {
    result((|| {
        check(!adapter.is_null(), "Invalid Adapter Handle.\n")?;
        check(removable_or_external(existing), "Invalid XCrdIdExisting.\n")?;
        check(removable_or_external(new), "Invalid XCrdIdNew.\n")?;
        let mut r = Request::new(existing, 5);
        r.value = &new;
        Ok(operate(adapter, &r))
    })())
}

/// 0x180007fe0.
pub extern "system" fn XCrdStorageSetConsoleAssociation(adapter: *const Adapter, id: u32, console: u32) -> HRESULT {
    let mut r = Request::new(id, 0x14);
    r.console = &console;
    operate(adapter, &r)
}

/// 0x180008040.
pub extern "system" fn XCrdStorageSetDeviceName(adapter: *const Adapter, id: u32, name: *const u16) -> HRESULT {
    let mut r = Request::new(id, 2);
    r.a = name;
    operate(adapter, &r)
}

/// 0x1800080a0.
pub extern "system" fn XCrdStorageSetLastUsedConsole(adapter: *const Adapter, id: u32) -> HRESULT {
    operate(adapter, &Request::new(id, 3))
}

#[allow(dead_code)]
fn _unused(_: *const c_void) {}

pub(crate) fn exports() -> Vec<crate::HleExport> {
    use crate::hle_export;
    vec![
        hle_export!(XCrdFindStorageDevice),
        hle_export!(XCrdGetDeviceIdMapping),
        hle_export!(XCrdSetDeviceIdMapping),
        hle_export!(XCrdSetEraLaunchMapping),
        hle_export!(XCrdSetExternalPlsMapping),
        hle_export!(XCrdStorageAttachExternalDevice),
        hle_export!(XCrdStorageCheck),
        hle_export!(XCrdStorageConnectDevice),
        hle_export!(XCrdStorageCreateBlob),
        hle_export!(XCrdStorageCreateDirectory),
        hle_export!(XCrdStorageDeleteBlob),
        hle_export!(XCrdStorageDeleteDirectory),
        hle_export!(XCrdStorageDisconnectDevice),
        hle_export!(XCrdStorageFormat),
        hle_export!(XCrdStorageMoveBlob),
        hle_export!(XCrdStorageRemapDevice),
        hle_export!(XCrdStorageSetConsoleAssociation),
        hle_export!(XCrdStorageSetDeviceName),
        hle_export!(XCrdStorageSetLastUsedConsole),
    ]
}
