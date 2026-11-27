//! The exports whose body carries a CRD path. Two shapes recur: a bare
//! path (`{8, path bytes, path}`) and a headed one (`{total, header size,
//! path bytes, fields..., path at the header's end}`). Addresses are
//! `XCRDAPI.dll`'s.

use super::srb::{Payload, check, copy_string, send};
use super::{Adapter, E_INVALIDARG, HRESULT, S_OK, handle_of, heap_alloc, heap_free, hresult_from_win32, sys, wide};

fn result(r: Result<HRESULT, HRESULT>) -> HRESULT {
    r.unwrap_or_else(|hr| hr)
}

/// The characters of a path the caller passed (checked non-null).
fn text<'a>(path: *const u16) -> &'a [u16] {
    // SAFETY: a NUL-terminated path.
    unsafe { wide(path) }
}

/// A headed body: `{total, header, path bytes}` then the caller's fields,
/// the path (and its NUL) at `header`.
pub(crate) fn headed(code: u32, header: usize, path: &[u16]) -> Result<Payload, HRESULT> {
    let total = path.len() * 2 + header + 2;
    let mut p = Payload::new(code, total);
    p.u32(0, total as u32);
    p.u32(4, header as u32);
    p.u32(8, (path.len() * 2) as u32);
    let status = copy_string(p.body(), header, path.len() * 2 + 2, path);
    if status < 0 {
        return Err(super::srb::hresult(status));
    }
    Ok(p)
}

/// A bare body: `{8, path bytes}` and the path at 8.
pub(crate) fn bare(code: u32, path: &[u16]) -> Result<Payload, HRESULT> {
    let mut p = Payload::new(code, path.len() * 2 + 0xa);
    p.u32(0, 8);
    p.u32(4, (path.len() * 2) as u32);
    let status = copy_string(p.body(), 8, path.len() * 2 + 2, path);
    if status < 0 {
        return Err(super::srb::hresult(status));
    }
    Ok(p)
}

/// 0x180002be0: flags (bit 0 only) are checked before the adapter.
pub extern "system" fn XCrdDeleteXVD(adapter: *const Adapter, path: *const u16, flags: u32) -> HRESULT {
    result((|| {
        check(flags & !1 == 0, "Invalid Delete Flags.\n")?;
        check(!adapter.is_null(), "Invalid Adapter Handle.\n")?;
        check(!path.is_null(), "Invalid Input Path.\n")?;
        let mut p = headed(0x7800_0024, 0x18, text(path))?;
        p.u32(0xc, flags);
        Ok(send(handle_of(adapter), &mut p))
    })())
}

/// 0x180002eb0: a 16-byte id and the path; the length field counts the
/// NUL here, and an empty path is refused.
pub extern "system" fn XCrdEnableSharedPls(adapter: *const Adapter, path: *const u16, id: *const [u8; 16]) -> HRESULT {
    result((|| {
        check(!adapter.is_null(), "Invalid adapter handle.\n")?;
        check(!path.is_null() && !text(path).is_empty(), "Invalid input path.\n")?;
        let path = text(path);
        let bytes = path.len() * 2 + 2;
        let mut p = Payload::new(0x7800_01cc, bytes + 0x18);
        p.u32(0, 0x18);
        p.u32(4, bytes as u32);
        // SAFETY: the caller's id (not checked).
        p.put(8, unsafe { &*id });
        let status = copy_string(p.body(), 0x18, bytes, path);
        if status < 0 {
            return Ok(super::srb::hresult(status));
        }
        Ok(send(handle_of(adapter), &mut p))
    })())
}

/// 0x1800036b0: a new size (non-zero) at +0x10.
pub extern "system" fn XCrdGrowDynamicXvd(adapter: *const Adapter, path: *const u16, size: u64) -> HRESULT {
    result((|| {
        check(!adapter.is_null(), "Invalid adapter handle.\n")?;
        check(!path.is_null(), "Invalid input path.\n")?;
        check(size != 0, "Invalid size.\n")?;
        let mut p = headed(0x7800_0170, 0x18, text(path))?;
        p.u64(0x10, size);
        Ok(send(handle_of(adapter), &mut p))
    })())
}

/// 0x180003840: corrupt when the driver succeeds; not found means not
/// corrupt (S_OK).
pub extern "system" fn XCrdIsCorrupt(adapter: *const Adapter, path: *const u16, corrupt: *mut u8) -> HRESULT {
    result((|| {
        check(!adapter.is_null(), "Invalid Adapter Handle\n")?;
        check(!path.is_null(), "XCrdPath must be specified\n")?;
        check(!corrupt.is_null(), "IsCorrupt must be specified\n")?;
        // SAFETY: checked non-null.
        unsafe { *corrupt = 0 };
        let mut p = bare(0x7800_0108, text(path))?;
        let hr = send(handle_of(adapter), &mut p);
        if hr >= 0 {
            // SAFETY: as above.
            unsafe { *corrupt = 1 };
            return Ok(hr);
        }
        Ok(if hr == 0x8007_0490_u32 as i32 { S_OK } else { hr })
    })())
}

/// 0x180004560: the path of a corrupt XVC, as the driver writes it into a
/// 0x200-byte body, copied to a process-heap string.
pub extern "system" fn XCrdQueryCorruptXvc(adapter: *const Adapter, out: *mut *mut u16) -> HRESULT {
    result((|| {
        check(!adapter.is_null(), "Invalid Adapter Handle\n")?;
        check(!out.is_null(), "XCrdPath must be specified\n")?;
        // SAFETY: checked non-null.
        unsafe { *out = std::ptr::null_mut() };
        let mut p = Payload::new(0x7800_0100, 0x200);
        let hr = send(handle_of(adapter), &mut p);
        if hr < 0 {
            return Ok(hr);
        }
        let body = &p.bytes[super::srb::HEADER..];
        let chars: Vec<u16> = body.chunks(2).map(|c| u16::from_le_bytes([c[0], c[1]])).take_while(|&c| c != 0).collect();
        let bytes = chars.len() * 2 + 2;
        let buffer = heap_alloc(bytes);
        if buffer.is_null() {
            return Ok(hresult_from_win32(sys::last_error()));
        }
        // SAFETY: a block of `bytes`.
        let dest = unsafe { std::slice::from_raw_parts_mut(buffer, bytes) };
        let status = copy_string(dest, 0, bytes, &chars);
        if status < 0 {
            heap_free(buffer.cast());
            return Ok(super::srb::hresult(status));
        }
        // SAFETY: as above.
        unsafe { *out = buffer.cast() };
        Ok(S_OK)
    })())
}

/// 0x18000af00.
pub extern "system" fn XCrdXBlobDelete(adapter: *const Adapter, path: *const u16) -> HRESULT {
    result((|| {
        check(!adapter.is_null(), "Invalid Adapter Handle.\n")?;
        check(!path.is_null(), "Invalid Input Path.\n")?;
        let mut p = headed(0x7800_00e8, 0x18, text(path))?;
        Ok(send(handle_of(adapter), &mut p))
    })())
}

/// 0x18000ada0: a size at +0x10; flags must be 0.
pub extern "system" fn XCrdXBlobCreate(adapter: *const Adapter, path: *const u16, size: u64, flags: u32) -> HRESULT {
    result((|| {
        check(!adapter.is_null(), "Invalid Adapter Handle.\n")?;
        check(!path.is_null(), "Invalid Input Path.\n")?;
        check(flags == 0, "Flags must be zero.\n")?;
        let mut p = headed(0x7800_00e4, 0x30, text(path))?;
        p.u64(0x10, size);
        Ok(send(handle_of(adapter), &mut p))
    })())
}

#[allow(dead_code)]
fn unused() -> HRESULT {
    E_INVALIDARG
}

pub(crate) fn exports() -> Vec<crate::HleExport> {
    use crate::hle_export;
    vec![
        hle_export!(XCrdDeleteXVD),
        hle_export!(XCrdEnableSharedPls),
        hle_export!(XCrdGrowDynamicXvd),
        hle_export!(XCrdIsCorrupt),
        hle_export!(XCrdQueryCorruptXvc),
        hle_export!(XCrdXBlobDelete),
        hle_export!(XCrdXBlobCreate),
    ]
}
