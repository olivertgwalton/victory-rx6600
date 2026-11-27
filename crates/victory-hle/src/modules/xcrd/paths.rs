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

/// Two paths, one after the other: the first at `header`, the second right
/// after the first's NUL; `put` gets the second's offset.
fn two_paths(p: &mut Payload, header: usize, a: &[u16], b: &[u16]) -> Result<usize, HRESULT> {
    let status = copy_string(p.body(), header, a.len() * 2 + 2, a);
    if status < 0 {
        return Err(super::srb::hresult(status));
    }
    let second = header + a.len() * 2 + 2;
    let status = copy_string(p.body(), second, b.len() * 2 + 2, b);
    if status < 0 {
        return Err(super::srb::hresult(status));
    }
    Ok(second)
}

/// 0x180002a20: defragments an XVC; three optional counts come back (set
/// to 0 first, then from +0x10, +0x14, +0x18 whatever the driver said).
pub extern "system" fn XCrdDefragXvc(adapter: *const Adapter, path: *const u16, flags: u32, a: *mut u32, b: *mut u32, c: *mut u32) -> HRESULT {
    result((|| {
        check(!adapter.is_null(), "Invalid Adapter Handle\n")?;
        check(!path.is_null(), "XCrdPath must be specified\n")?;
        check(flags & !1 == 0, "Invalid Flags\n")?;
        for out in [a, b, c] {
            if !out.is_null() {
                // SAFETY: checked non-null.
                unsafe { *out = 0 };
            }
        }
        let path = text(path);
        check(path.len() * 2 + 0x42 <= super::srb::MAX_BODY, "Payload too large for XVIO\n")?;
        let mut p = Payload::new(0x7800_0150, path.len() * 2 + 0x42);
        p.u32(0, 0x40);
        p.u32(4, (path.len() * 2) as u32);
        p.u32(8, flags & 1);
        let status = copy_string(p.body(), 0x40, path.len() * 2 + 2, path);
        if status < 0 {
            return Ok(super::srb::hresult(status));
        }
        let hr = send(handle_of(adapter), &mut p);
        for (out, at) in [(a, 0x10), (b, 0x14), (c, 0x18)] {
            if !out.is_null() {
                // SAFETY: checked non-null.
                unsafe { *out = p.read_u32(at) };
            }
        }
        Ok(hr)
    })())
}

/// 0x180002090: copies extended attributes; flag bits 0-2 pass, bit 3 is
/// accepted and dropped.
pub extern "system" fn XCrdCopyExtendedAttributes(adapter: *const Adapter, source: *const u16, dest: *const u16, flags: u32) -> HRESULT {
    result((|| {
        check(!adapter.is_null(), "Invalid adapter handle.\n")?;
        check(!source.is_null(), "Invalid source CRD path.\n")?;
        check(!dest.is_null(), "Invalid destination CRD path.\n")?;
        check(flags & !0xf == 0, "Invalid EA flags.\n")?;
        let (s, t) = (text(source), text(dest));
        let mut p = Payload::new(0x7800_01a8, (s.len() + t.len()) * 2 + 0x18);
        p.u32(0x10, flags & 7);
        p.u32(0, 0x14);
        p.u32(4, (s.len() * 2) as u32);
        p.u32(8, (0x14 + 2 + s.len() * 2) as u32);
        p.u32(0xc, (t.len() * 2) as u32);
        if let Err(hr) = two_paths(&mut p, 0x14, s, t) {
            return Ok(hr);
        }
        Ok(send(handle_of(adapter), &mut p))
    })())
}

/// 0x180003b30: moves sidecar files; flag bit 0 at +0x14.
pub extern "system" fn XCrdMoveSidecarFiles(adapter: *const Adapter, source: *const u16, dest: *const u16, flags: u32) -> HRESULT {
    result((|| {
        check(!adapter.is_null(), "Invalid Adapter Handle.\n")?;
        check(!source.is_null(), "Invalid Src Input Path.\n")?;
        check(!dest.is_null(), "Invalid Dst Input Path.\n")?;
        check(flags & !1 == 0, "Invalid Move Flags\n")?;
        let (s, t) = (text(source), text(dest));
        let total = (s.len() + t.len()) * 2 + 0x1c;
        let mut p = Payload::new(0x7800_01b0, total);
        p.u32(0, total as u32);
        p.u32(0x14, flags & 1);
        p.u32(4, 0x18);
        p.u32(8, (s.len() * 2) as u32);
        p.u32(0xc, (0x18 + s.len() * 2 + 2) as u32);
        p.u32(0x10, (t.len() * 2) as u32);
        if let Err(hr) = two_paths(&mut p, 0x18, s, t) {
            return Ok(hr);
        }
        Ok(send(handle_of(adapter), &mut p))
    })())
}

/// 0x180006880: extended attributes; flag bits 0, 3 and 31 only.
pub extern "system" fn XCrdSetExtendedAttributes(adapter: *const Adapter, path: *const u16, flags: u32, value: u64) -> HRESULT {
    result((|| {
        check(!adapter.is_null(), "Invalid adapter handle.\n")?;
        check(!path.is_null(), "Invalid CRD path.\n")?;
        check(flags & 0x7fff_fff6 == 0, "Invalid EA flags.\n")?;
        let path = text(path);
        let mut p = Payload::new(0x7800_01a4, path.len() * 2 + 0x1a);
        p.u64(0x10, value);
        p.u32(8, flags & 0x8000_0009);
        p.u32(0, 0x18);
        p.u32(4, (path.len() * 2) as u32);
        let status = copy_string(p.body(), 0x18, path.len() * 2 + 2, path);
        if status < 0 {
            return Ok(super::srb::hresult(status));
        }
        Ok(send(handle_of(adapter), &mut p))
    })())
}

/// 0x1800066c0: an encrypted EA with a 32-byte key and a 16-byte IV; bad
/// arguments fail with no message.
pub extern "system" fn XCrdSetEncryptedEa(adapter: *const Adapter, path: *const u16, value: u64, key: *const [u8; 32], iv: *const [u8; 16]) -> HRESULT {
    if adapter.is_null() || path.is_null() || key.is_null() || iv.is_null() {
        return E_INVALIDARG;
    }
    let path = text(path);
    let mut p = Payload::new(0x7800_01e4, path.len() * 2 + 0x42);
    p.u64(8, value);
    // SAFETY: checked non-null.
    p.put(0x10, unsafe { &*key });
    // SAFETY: as above.
    p.put(0x30, unsafe { &*iv });
    p.u32(0, 0x40);
    p.u32(4, (path.len() * 2) as u32);
    let status = copy_string(p.body(), 0x40, path.len() * 2 + 2, path);
    if status < 0 {
        return super::srb::hresult(status);
    }
    send(handle_of(adapter), &mut p)
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
        hle_export!(XCrdDefragXvc),
        hle_export!(XCrdCopyExtendedAttributes),
        hle_export!(XCrdMoveSidecarFiles),
        hle_export!(XCrdSetExtendedAttributes),
        hle_export!(XCrdSetEncryptedEa),
    ]
}
