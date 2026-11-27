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
        return Err(status);
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
        return Err(status);
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
            return Ok(status);
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
            return Ok(status);
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
        return Err(status);
    }
    let second = header + a.len() * 2 + 2;
    let status = copy_string(p.body(), second, b.len() * 2 + 2, b);
    if status < 0 {
        return Err(status);
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
            return Ok(status);
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
            return Ok(status);
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
        return status;
    }
    send(handle_of(adapter), &mut p)
}

/// 0x180005f50: a remap test; the driver's answer (a string at +0x10) is
/// copied into the caller's buffer of `out_bytes`.
pub extern "system" fn XCrdRemapTest(adapter: *const Adapter, path: *const u16, out: *mut u16, out_bytes: u32) -> HRESULT {
    result((|| {
        check(!adapter.is_null(), "Invalid Adapter Handle.\n")?;
        check(!path.is_null(), "Invalid Input Path.\n")?;
        check(!out.is_null() && out_bytes != 0, "Invalid output buffer.\n")?;
        let mut p = headed(0x7800_00f4, 0x428, text(path))?;
        let hr = send(handle_of(adapter), &mut p);
        if hr < 0 {
            return Ok(hr);
        }
        let body = &p.bytes[super::srb::HEADER + 0x10..];
        let answer: Vec<u16> = body.chunks(2).map(|c| u16::from_le_bytes([c[0], c[1]])).collect();
        // SAFETY: the caller's buffer of `out_bytes`.
        let dest = unsafe { std::slice::from_raw_parts_mut(out.cast::<u8>(), out_bytes as usize & !1) };
        Ok((copy_string(dest, 0, out_bytes as usize, &answer)))
    })())
}

/// `XCrdRepairXvc`'s request: a kind, the XVC's path, a source path, a
/// value, flags (bits 0-1).
#[repr(C)]
pub struct Repair {
    pub kind: u32,
    pub xvc: *const u16,
    pub source: *const u16,
    pub value: u32,
    pub flags: u32,
}

/// 0x1800060e0: kinds 0-2 and 5 need the XVC's path; kinds 0 and 2 a
/// source path too unless flag 2 is set.
pub extern "system" fn XCrdRepairXvc(adapter: *const Adapter, repair: *const Repair) -> HRESULT {
    result((|| {
        check(!adapter.is_null(), "Invalid Adapter Handle\n")?;
        check(!repair.is_null(), "Invalid Repair Information\n")?;
        // SAFETY: checked non-null.
        let r = unsafe { &*repair };
        if r.kind <= 2 || r.kind == 5 {
            check(!r.xvc.is_null(), "XvcPath must be specified\n")?;
        }
        if (r.kind == 0 || r.kind == 2) && r.source.is_null() && r.flags & 2 == 0 {
            check(false, "SrcPath must be specified\n")?;
        }
        check(r.flags & !3 == 0, "Invalid flags\n")?;
        let xvc = (!r.xvc.is_null()).then(|| text(r.xvc));
        let source = (!r.source.is_null()).then(|| text(r.source));
        let mut len = 0x1c;
        if let Some(x) = xvc {
            len = x.len() * 2 + 0x1e;
        }
        if let Some(s) = source {
            len += s.len() * 2 + 2;
        }
        let mut p = Payload::new(0x7800_0104, len);
        p.u32(0, r.kind);
        p.u32(8, r.value);
        p.u32(4, r.flags & 3);
        let mut next = 0x1c;
        if let Some(x) = xvc {
            p.u32(0xc, 0x1c);
            p.u32(0x10, (x.len() * 2) as u32);
            let status = copy_string(p.body(), 0x1c, x.len() * 2 + 2, x);
            next = x.len() * 2 + 0x1e;
            if status < 0 {
                return Ok(status);
            }
        } else {
            p.u64(0xc, 0);
        }
        if let Some(s) = source {
            p.u32(0x14, next as u32);
            p.u32(0x18, (s.len() * 2) as u32);
            let status = copy_string(p.body(), next, s.len() * 2 + 2, s);
            if status < 0 {
                return Ok(status);
            }
        } else {
            p.u64(0x14, 0);
        }
        Ok(send(handle_of(adapter), &mut p))
    })())
}

/// 0x180006a50: a CRD path and the flat content path it maps to.
pub extern "system" fn XCrdSetFlatContentPath(adapter: *const Adapter, path: *const u16, content: *const u16) -> HRESULT {
    result((|| {
        check(!adapter.is_null(), "Invalid adapter handle.\n")?;
        check(!path.is_null(), "Invalid CRD path.\n")?;
        check(!content.is_null(), "Invalid content path.\n")?;
        let (c, t) = (text(path), text(content));
        let mut p = Payload::new(0x7800_0194, (c.len() + t.len()) * 2 + 0x14);
        p.u32(0, 0x10);
        p.u32(4, (c.len() * 2) as u32);
        p.u32(8, (c.len() * 2 + 0x12) as u32);
        p.u32(0xc, (t.len() * 2) as u32);
        if let Err(hr) = two_paths(&mut p, 0x10, c, t) {
            return Ok(hr);
        }
        Ok(send(handle_of(adapter), &mut p))
    })())
}

/// 0x180007bb0: a blob's attributes, 0x34 bytes from +0x10.
pub extern "system" fn XCrdStorageQueryBlobAttributes(adapter: *const Adapter, path: *const u16, attributes: *mut [u8; 0x34]) -> HRESULT {
    result((|| {
        check(!adapter.is_null(), "Invalid Adapter Handle.\n")?;
        check(!path.is_null(), "Invalid Input Path.\n")?;
        check(!attributes.is_null(), "Attributes must not be NULL\n")?;
        let mut p = headed(0x7800_00c4, 0x48, text(path))?;
        let hr = send(handle_of(adapter), &mut p);
        if hr >= 0 {
            // SAFETY: checked non-null.
            unsafe { (*attributes).copy_from_slice(&p.bytes[super::srb::HEADER + 0x10..super::srb::HEADER + 0x44]) };
        }
        Ok(hr)
    })())
}

/// The library's streaming flags as the driver's: bit 0 -> 0x10, bit 3 ->
/// 0x100, bit 16 -> 0x80000.
fn streaming_flags(f: u32) -> u32 {
    (if f & 1 != 0 { 0x10 } else { 0 }) | (if f & 8 != 0 { 0x100 } else { 0 }) | (if f & 0x1_0000 != 0 { 0x8_0000 } else { 0 })
}

/// 0x180008280: flags to set and to clear on a streaming instance (bits
/// 0, 3 and 16).
pub extern "system" fn XCrdStreamingAdjustStreamingFlags(adapter: *const Adapter, id: u64, enable: u32, disable: u32) -> HRESULT {
    result((|| {
        check(!adapter.is_null(), "Invalid Adapter Handle\n")?;
        check(enable & !0x1_ffff == 0, "Invalid Enable flags\n")?;
        check(disable & !0x1_ffff == 0, "Invalid Disable flags\n")?;
        check(enable & disable == 0, "Cannot enable and disable same flags\n")?;
        check(enable & !0x1_0009 == 0, "Invalid Enable flags\n")?;
        check(disable & !0x1_0009 == 0, "Invalid Disable flags\n")?;
        let mut p = Payload::new(0x7800_00dc, 0x10);
        p.u64(0, id);
        p.u32(8, streaming_flags(enable));
        p.u32(0xc, streaming_flags(disable));
        Ok(send(handle_of(adapter), &mut p))
    })())
}

/// 0x18000c870: the driver's streaming information (its body) as the
/// caller's 0x108-byte structure; ten flag bits at +0x98 are merged into
/// what the caller had there.
fn streaming_info(out: *mut u8, body: &[u8]) {
    // (from, to, bytes)
    const MAP: &[(usize, usize, usize)] = &[
        (0x58, 0x58, 8), (0x60, 0x60, 8), (0x50, 0x50, 8), (0x78, 0x78, 4), (0x80, 0x80, 8), (0x68, 0x68, 8),
        (0x70, 0x70, 8), (0xac, 0x9c, 4), (0xb8, 0xa8, 4), (0xbc, 0xac, 1), (0x8, 0x8, 8), (0x38, 0x38, 8),
        (0x18, 0x18, 8), (0x20, 0x20, 8), (0x30, 0x30, 8), (0x40, 0x40, 8), (0x10, 0x10, 8), (0x0, 0x0, 8),
        (0xbe, 0xae, 2), (0x88, 0x88, 8), (0x9c, 0x90, 4), (0xa4, 0x7c, 4), (0xa0, 0x94, 4),
        (0xb0, 0xa0, 4), (0xb4, 0xa4, 4), (0xd0, 0xc0, 8), (0xc8, 0xb8, 8), (0xd8, 0xc8, 8), (0xe0, 0xd0, 4),
        (0xe8, 0xd8, 16), (0xf8, 0xe8, 8), (0x100, 0xf0, 8), (0x108, 0xf8, 8), (0x110, 0x100, 8),
    ];
    // SAFETY: the caller's 0x108-byte structure.
    let out = unsafe { std::slice::from_raw_parts_mut(out, 0x108) };
    for &(from, to, n) in MAP {
        if from < 0xa8 || from > 0xab {
            out[to..to + n].copy_from_slice(&body[from..from + n]);
        }
    }
    const BITS: u32 = 0x1 | 0x2 | 0x8 | 0x10 | 0x20 | 0x80 | 0x40 | 0x100 | 0x200 | 0x400;
    let flags = u32::from_le_bytes(body[0xa8..0xac].try_into().unwrap());
    let mine = u32::from_le_bytes(out[0x98..0x9c].try_into().unwrap());
    out[0x98..0x9c].copy_from_slice(&((mine & !BITS) | (flags & BITS)).to_le_bytes());
}

/// 0x180008670: a streaming instance's information; failures are noted
/// in the caller's record (source line 0xae8 for a missing adapter).
pub extern "system" fn XCrdStreamingQueryInformationEx(out: *mut u8, failure: *mut super::srb::Failure, adapter: *const Adapter, id: u64) -> HRESULT {
    // SAFETY: the caller's record (not checked).
    let failure = unsafe { &mut *failure };
    if adapter.is_null() {
        super::srb::log(1, "Invalid Adapter Handle\n");
        failure.note(0xae8, 0);
        return E_INVALIDARG;
    }
    let mut p = Payload::new(0x7800_0038, 0x118);
    p.u64(0, id);
    let hr = super::srb::send_noting(handle_of(adapter), &mut p, Some(0x28), failure);
    if hr >= 0 {
        streaming_info(out, &p.bytes[super::srb::HEADER..]);
    }
    hr
}

/// 0x1800084a0: as the `Ex`, with a record of its own.
pub extern "system" fn XCrdStreamingQueryInformation(out: *mut u8, adapter: *const Adapter, id: u64) -> HRESULT {
    XCrdStreamingQueryInformationEx(out, &mut super::srb::Failure::default(), adapter, id)
}

/// 0x1800084f0: by the content's path (non-empty); flag 1 sets bit 1 of
/// the mode at +0x98.
pub extern "system" fn XCrdStreamingQueryInformationByPath(out: *mut u8, adapter: *const Adapter, path: *const u16, flags: u32) -> HRESULT {
    result((|| {
        check(!adapter.is_null(), "Invalid Adapter Handle\n")?;
        check(!path.is_null(), "Invalid XCRD Path\n")?;
        let path = text(path);
        check(!path.is_empty(), "Invalid XCRD String Length\n")?;
        check(flags & !1 == 0, "Invalid Flags\n")?;
        let bytes = path.len() * 2;
        let mut p = Payload::new(0x7800_0038, bytes + 0x11a);
        p.u64(0, u64::MAX);
        p.u32(0x98, if flags & 1 != 0 { 3 } else { 1 });
        let status = copy_string(p.body(), 0x118, bytes + 2, path);
        if status < 0 {
            return Ok(status);
        }
        p.u32(0x90, 0x118);
        p.u32(0x94, (bytes + 2) as u32);
        let hr = send(handle_of(adapter), &mut p);
        if hr >= 0 {
            streaming_info(out, &p.bytes[super::srb::HEADER..]);
        }
        Ok(hr)
    })())
}

/// 0x18000ab80: copies an XBlob; offset and length must be 0; flag 2 adds
/// bit 1 to flag 1; a value comes back from +0x28.
pub extern "system" fn XCrdXBlobCopy(adapter: *const Adapter, source: *const u16, dest: *const u16, offset: u64, length: u64, flags: u32, out: *mut u64) -> HRESULT {
    result((|| {
        check(!adapter.is_null(), "Invalid Adapter Handle.\n")?;
        check(!source.is_null(), "Invalid Src Input Path.\n")?;
        check(!dest.is_null(), "Invalid Dst Input Path.\n")?;
        check(offset == 0 && length == 0, "Offset, Length must be zero.\n")?;
        let (s, t) = (text(source), text(dest));
        let total = (s.len() + t.len()) * 2 + 0x54;
        let mut p = Payload::new(0x7800_00ec, total);
        p.u32(0, total as u32);
        p.u32(0x14, if flags & 2 != 0 { (flags & 1) | 2 } else { flags & 1 });
        p.u32(4, 0x50);
        p.u32(8, (s.len() * 2) as u32);
        p.u32(0xc, (0x50 + s.len() * 2 + 2) as u32);
        p.u32(0x10, (t.len() * 2) as u32);
        if let Err(hr) = two_paths(&mut p, 0x50, s, t) {
            return Ok(hr);
        }
        let hr = send(handle_of(adapter), &mut p);
        if hr >= 0 && !out.is_null() {
            // SAFETY: checked non-null.
            unsafe { *out = u64::from_le_bytes(p.bytes[super::srb::HEADER + 0x28..super::srb::HEADER + 0x30].try_into().unwrap()) };
        }
        Ok(hr)
    })())
}

/// 0x180009d30: trims an XVD (flag 1 also reports the size trimmed, from
/// +0x20); whether it is done (+0x1c) comes back whatever the driver said.
pub extern "system" fn XCrdTrimXvd(adapter: *const Adapter, path: *const u16, flags: u32, trimmed: *mut u64, done: *mut u8) -> HRESULT {
    if !done.is_null() {
        // SAFETY: checked non-null.
        unsafe { *done = 0 };
    }
    result((|| {
        check(!adapter.is_null(), "Invalid Adapter Handle\n")?;
        check(!path.is_null(), "XCrdPath must be specified\n")?;
        check(flags & !1 == 0, "Invalid Flags\n")?;
        check(flags & 1 == 0 || !trimmed.is_null(), "TrimmedSize cannot be null\n")?;
        let path = text(path);
        check(path.len() * 2 + 0x4a <= super::srb::MAX_BODY, "Payload too large for XVIO\n")?;
        let mut p = Payload::new(0x7800_00ac, path.len() * 2 + 0x4a);
        p.u32(0x14, if flags & 1 != 0 { 3 } else { 2 });
        p.u32(0, 0x48);
        p.u32(4, (path.len() * 2) as u32);
        let status = copy_string(p.body(), 0x48, path.len() * 2 + 2, path);
        if status < 0 {
            return Ok(status);
        }
        let hr = send(handle_of(adapter), &mut p);
        let b = &p.bytes[super::srb::HEADER..];
        if hr >= 0 && !trimmed.is_null() {
            // SAFETY: checked non-null.
            unsafe { *trimmed = u64::from_le_bytes(b[0x20..0x28].try_into().unwrap()) };
        }
        if !done.is_null() {
            // SAFETY: as above.
            unsafe { *done = b[0x1c] };
        }
        Ok(hr)
    })())
}

/// 0x180009a10: trims an XVC, keeping what the specifiers (pairs of
/// strings) name: a table of `{offset, bytes}` pairs at +8's offset and the
/// strings after it.
pub extern "system" fn XCrdTrimXvc(adapter: *const Adapter, path: *const u16, count: u32, specs: *const [*const u16; 2], flags: u32, trimmed: *mut u64, done: *mut u8) -> HRESULT {
    if !done.is_null() {
        // SAFETY: checked non-null.
        unsafe { *done = 0 };
    }
    result((|| {
        check(!adapter.is_null(), "Invalid Adapter Handle\n")?;
        check(!path.is_null(), "XCrdPath must be specified\n")?;
        check(flags & !1 == 0, "Invalid Flags\n")?;
        check(flags & 1 == 0 || !trimmed.is_null(), "TrimmedSize cannot be null\n")?;
        if count == 0 {
            check(specs.is_null(), "Specifiers must be NULL if zero specifiers are passed\n")?;
        } else {
            check(!specs.is_null(), "Specifiers must not be NULL\n")?;
        }
        let path = text(path);
        // SAFETY: `count` pairs of strings (non-null when count is not 0).
        let pairs: Vec<(&[u16], &[u16])> = (0..count as usize).map(|i| unsafe { let s = *specs.add(i); (text(s[0]), text(s[1])) }).collect();
        let mut size = (path.len() as u32).wrapping_add(count.wrapping_mul(8)).wrapping_mul(2).wrapping_add(0x4a);
        for (a, b) in &pairs {
            size = size.wrapping_add(((a.len() + b.len()) * 2 + 4) as u32);
        }
        check(size as usize <= super::srb::MAX_BODY, "Payload too large for XVIO\n")?;
        let mut p = Payload::new(0x7800_00ac, size as usize);
        p.u32(0x14, flags & 1);
        p.u32(0, 0x48);
        p.u32(4, (path.len() * 2) as u32);
        let status = copy_string(p.body(), 0x48, path.len() * 2 + 2, path);
        let table = path.len() * 2 + 0x4a;
        p.u32(0x10, count);
        p.u32(8, table as u32);
        p.u32(0xc, count.wrapping_mul(16));
        if status < 0 {
            return Ok(status);
        }
        let mut cursor = table + count as usize * 16;
        for (i, (a, b)) in pairs.iter().enumerate() {
            let entry = table + i * 16;
            let status = copy_string(p.body(), cursor, a.len() * 2 + 2, a);
            let grown = p.read_u32(0xc) + (a.len() * 2 + 2) as u32;
            p.u32(0xc, grown);
            p.u32(entry, cursor as u32);
            p.u32(entry + 4, (a.len() * 2) as u32);
            let second = cursor + 2 + a.len() * 2;
            if status < 0 {
                return Ok(status);
            }
            let status = copy_string(p.body(), second, b.len() * 2 + 2, b);
            let grown = p.read_u32(0xc) + (b.len() * 2 + 2) as u32;
            p.u32(0xc, grown);
            p.u32(entry + 8, second as u32);
            p.u32(entry + 12, (b.len() * 2) as u32);
            cursor = second + b.len() * 2 + 2;
            if status < 0 {
                return Ok(status);
            }
        }
        let hr = send(handle_of(adapter), &mut p);
        let body = &p.bytes[super::srb::HEADER..];
        if hr >= 0 && !trimmed.is_null() {
            // SAFETY: checked non-null.
            unsafe { *trimmed = u64::from_le_bytes(body[0x20..0x28].try_into().unwrap()) };
        }
        if !done.is_null() {
            // SAFETY: as above.
            unsafe { *done = body[0x1c] };
        }
        Ok(hr)
    })())
}

/// 0x180002260: creates an XVD of `pages` 4 KiB pages (1 MiB to 1 TiB in
/// all), of a type (0 or 1) and content type (up to 0x28), with optional
/// ids, a sandbox (up to 16 bytes) and a second path; the new XVD's id
/// comes back (+0x60). Streaming roamable XVDs (flag 0x20000) are for server
/// consoles, which a title's is not.
pub extern "system" fn XCrdCreateXVD(
    adapter: *const Adapter,
    path: *const u16,
    id: *const [u8; 16],
    flags: u32,
    pages: u64,
    kind: u32,
    content: u32,
    created: *mut [u8; 16],
    id2: *const [u8; 16],
    sandbox: *const u8,
    sandbox_len: u32,
    second: *const u16,
) -> HRESULT {
    result((|| {
        check(!adapter.is_null(), "Invalid Adapter Handle.\n")?;
        check(!path.is_null(), "Invalid Input Path.\n")?;
        check(flags & 0xff8c_0c18 == 0, "Invalid Create Flags.\n")?;
        if flags & 0x2_0000 != 0 {
            super::srb::log(1, "Streaming roamable XVDs can only be created on server consoles.\n");
            return Err(super::NOT_SUPPORTED);
        }
        check(flags & 0x3_0000 != 0x3_0000 && flags & 0x42_0000 != 0x42_0000, "Incompatible Create Flags.\n")?;
        check(sandbox.is_null() || sandbox_len <= 0x10, "Invalid Sandbox ID.\n")?;
        let bytes = pages << 12;
        check(bytes.wrapping_sub(0xa0_0000) <= 0xff_ff60_0000 && bytes >= pages, "Invalid Drive Size. Size must be between 1MB and 1TB, and multiples of 512 bytes.\n")?;
        check(kind <= 1, "Invalid XVD type.\n")?;
        check(content <= 0x28, "Invalid XVD content type.\n")?;
        if flags & 0x10_0000 != 0 {
            check(kind == 1 && content == 0x22 && flags & 2 == 0, "Invalid flags. Cannot enable trim unless the XVD is dynamic and thin-provisioned, and has integrity disabled\n")?;
        }
        let path = text(path);
        let second = if second.is_null() || text(second).is_empty() { None } else { Some(text(second)) };
        let mut total = path.len() * 2 + 0x72;
        if let Some(s) = second {
            total += s.len() * 2 + 2;
        }
        let mut p = Payload::new(0x7800_0020, total);
        p.u32(0, total as u32);
        p.u32(0x1c, content);
        const MAP: [(u32, u32); 12] = [
            (1, 4), (2, 2), (4, 1), (0x20, 8), (0x40, 0x10), (0x100, 0x40), (0x200, 0x80), (0x1_0000, 0x1000),
            (0x2_0000, 0x2000), (0x10_0000, 0x4000), (0x20_0000, 0x8000), (0x40_0000, 0x1_0000),
        ];
        let mut mapped = MAP.iter().filter(|(f, _)| flags & f != 0).fold(0, |m, (_, v)| m | v);
        match flags & 0xf080 {
            0 => {}
            0x80 => mapped |= 0x20,
            0x1000 => mapped |= 0x100,
            0x1080 => mapped |= 0x120,
            _ => return Ok(E_INVALIDARG),
        }
        p.u32(4, mapped);
        p.u64(0x20, (bytes >> 12) as u64);
        p.u32(0x18, kind);
        p.u32(0x28, 0x70);
        p.u32(0x2c, (path.len() * 2) as u32);
        p.u64(0x30, 0);
        // SAFETY: the caller's ids and sandbox, when given.
        unsafe {
            if !id.is_null() {
                p.put(8, &*id);
            }
            if !id2.is_null() {
                p.put(0x40, &*id2);
            }
            if !sandbox.is_null() {
                p.put(0x50, std::slice::from_raw_parts(sandbox, sandbox_len as usize));
            }
        }
        let status = copy_string(p.body(), 0x70, path.len() * 2 + 2, path);
        if status < 0 {
            return Ok(status);
        }
        if let Some(s) = second {
            let at = 0x70 + path.len() * 2 + 2;
            p.u32(0x30, at as u32);
            p.u32(0x34, (s.len() * 2) as u32);
            let status = copy_string(p.body(), at, s.len() * 2 + 2, s);
            if status < 0 {
                return Ok(status);
            }
        }
        let hr = send(handle_of(adapter), &mut p);
        if hr >= 0 && !created.is_null() {
            // SAFETY: checked non-null.
            unsafe { (*created).copy_from_slice(&p.bytes[super::srb::HEADER + 0x60..super::srb::HEADER + 0x70]) };
        }
        Ok(hr)
    })())
}

/// 0x1800026b0: a pointer XVD: the CRD path, the path it points to,
/// optional XVD header (0xe00 bytes), XVC information (0xda8) and embedded
/// header (0xe00). Flags: 1 mark as streamed, 2 skip the pointer, 4.
/// Each part's offset follows the one before it, given or not (as the
/// library's: an embedded header without XVC information lands at 0).
pub extern "system" fn XCrdCreateXvdPointer(
    adapter: *const Adapter,
    path: *const u16,
    pointer: *const u16,
    blob_size: u64,
    flags: u32,
    header: *const u8,
    xvc: *const u8,
    embedded: *const u8,
) -> HRESULT {
    result((|| {
        check(flags & !7 == 0, "Invalid Create Pointer Flags.\n")?;
        let (streamed, skip) = (flags & 1 != 0, flags & 2 != 0);
        check(!adapter.is_null(), "Invalid Adapter Handle.\n")?;
        check(!path.is_null(), "Invalid XcrdPath.\n")?;
        if streamed {
            check(header.is_null() == xvc.is_null(), "Header & XvcInfo must be either NULL or valid.\n")?;
        }
        check(xvc.is_null() || streamed, "XvcInfo only supported with MarkAsStreamed.\n")?;
        check(header.is_null() || blob_size != 0, "BlobSize must not be 0, if Header is supplied.\n")?;
        check(embedded.is_null() || !header.is_null(), "Xvd Header must be supplied, if Embedded Header is supplied.\n")?;
        if pointer.is_null() {
            check(skip, "Pointer path null is only supported for SkipPointer flag.\n")?;
            check(streamed, "SkipPointer is only supported for MarkAsStreamable flag.\n")?;
        } else {
            check(!skip, "Pointer path should be null for SkipPointer flag.\n")?;
        }
        let path = text(path);
        if path.is_empty() {
            return Err(E_INVALIDARG);
        }
        let target = if pointer.is_null() { &[][..] } else { text(pointer) };
        let mut size = (path.len() + target.len()) * 2 + 0x3c;
        for (part, n) in [(header, 0xe00), (xvc, 0xda8), (embedded, 0xe00)] {
            if !part.is_null() {
                size += n;
            }
        }
        let mut p = Payload::new(0x7800_00e0, size);
        p.u32(0, size as u32);
        if !target.is_empty() {
            p.u32(0xc, 0x38);
            p.u32(0x10, (target.len() * 2) as u32);
        }
        let path_at = p.read_u32(0x10) + 0x3a;
        p.u32(4, path_at);
        p.u32(8, (path.len() * 2) as u32);
        p.u64(0x28, blob_size);
        if !header.is_null() {
            p.u32(0x14, path_at + 2 + (path.len() * 2) as u32);
            p.u32(0x18, 0xe00);
        }
        if !xvc.is_null() {
            let at = p.read_u32(0x18) + p.read_u32(0x14);
            p.u32(0x1c, at);
            p.u32(0x20, 0xda8);
        }
        if !embedded.is_null() {
            let at = p.read_u32(0x20) + p.read_u32(0x1c);
            p.u32(0x30, at);
            p.u32(0x34, 0xe00);
        }
        let mut mode = 0;
        if streamed {
            mode |= 1;
        }
        if skip {
            mode |= 2;
        }
        if flags & 4 != 0 {
            mode |= 4;
        }
        p.u32(0x24, mode);
        if !target.is_empty() {
            let status = copy_string(p.body(), 0x38, target.len() * 2 + 2, target);
            if status < 0 {
                return Ok(status);
            }
        }
        let status = copy_string(p.body(), path_at as usize, path.len() * 2 + 2, path);
        if status < 0 {
            return Ok(status);
        }
        for (part, field) in [(header, 0x14), (xvc, 0x1c), (embedded, 0x30)] {
            if !part.is_null() {
                let (at, n) = (p.read_u32(field) as usize, p.read_u32(field + 4) as usize);
                // SAFETY: the caller's part of its fixed size.
                p.put(at, unsafe { std::slice::from_raw_parts(part, n) });
            }
        }
        Ok(send(handle_of(adapter), &mut p))
    })())
}

/// The specifier table of `XCrdStreamingStart` and `XCrdTrimXvc`: entries of
/// `{offset, bytes, offset, bytes}` at `table`, the strings from `strings`
/// on; the running size at `size_field` grows with each string.
fn specifier_table(p: &mut Payload, table: usize, mut strings: usize, size_field: usize, pairs: &[(&[u16], &[u16])]) -> Result<(), HRESULT> {
    for (i, (a, b)) in pairs.iter().enumerate() {
        let entry = table + i * 16;
        for (k, s) in [a, b].into_iter().enumerate() {
            let status = copy_string(p.body(), strings, s.len() * 2 + 2, s);
            let grown = p.read_u32(size_field) + (s.len() * 2 + 2) as u32;
            p.u32(size_field, grown);
            p.u32(entry + 8 * k, strings as u32);
            p.u32(entry + 8 * k + 4, (s.len() * 2) as u32);
            strings += s.len() * 2 + 2;
            if status < 0 {
                return Err(status);
            }
        }
    }
    Ok(())
}

/// 0x1800092f0: starts streaming content from a source to a destination
/// (flag 0x1000: as flat files, with a content path); specifier pairs
/// select regions. The instance id comes back (+0x18), and two values
/// (+0x38, +0x40) whatever the driver said.
pub extern "system" fn XCrdStreamingStart(
    id: *mut u64,
    adapter: *const Adapter,
    source: *const u16,
    name: *const u16,
    dest: *const u16,
    content: *const u16,
    extra: *const u16,
    count: u32,
    specs: *const [*const u16; 2],
    flags: u32,
    guid: *const [u8; 16],
    out64: *mut u64,
    out32: *mut u32,
) -> HRESULT {
    // SAFETY: the caller's optional outs, checked non-null.
    unsafe {
        if !out64.is_null() {
            *out64 = 0;
        }
        if !out32.is_null() {
            *out32 = 0;
        }
    }
    result((|| {
        check(!adapter.is_null(), "Invalid Adapter Handle\n")?;
        check(!source.is_null(), "Src Path must be specified\n")?;
        check(!dest.is_null(), "Dst Path must be specified\n")?;
        let flat = flags & 0x1000 != 0;
        check(flat || content.is_null(), "Dst Content Path must not be specified\n")?;
        check(flags & 0xe800 == 0 || flat, "Invalid combination of flat file flags\n")?;
        check(flags & 0xfffe_0000 == 0, "Invalid flags\n")?;
        if count == 0 {
            check(specs.is_null(), "Specifiers must be NULL if zero specifiers are passed\n")?;
        } else {
            check(!specs.is_null(), "Specifiers must not be NULL\n")?;
        }
        let (src, dst) = (text(source), text(dest));
        let opt = |s: *const u16| if s.is_null() { None } else { Some(text(s)) };
        let (name, content, extra) = (opt(name), opt(content), opt(extra));
        // SAFETY: `count` pairs of strings.
        let pairs: Vec<(&[u16], &[u16])> = (0..count as usize).map(|i| unsafe { let s = *specs.add(i); (text(s[0]), text(s[1])) }).collect();
        let mut size = ((src.len() + dst.len()) as u32).wrapping_add(count.wrapping_mul(8)).wrapping_mul(2).wrapping_add(0x6c);
        for (a, b) in &pairs {
            size = size.wrapping_add(((a.len() + b.len()) * 2 + 4) as u32);
        }
        for s in [&name, &content, &extra].into_iter().flatten() {
            size = size.wrapping_add((s.len() * 2 + 2) as u32);
        }
        check(size as usize <= super::srb::MAX_BODY, "Payload too large for XVIO\n")?;
        let mut p = Payload::new(0x7800_0030, size as usize);
        if !guid.is_null() {
            // SAFETY: checked non-null.
            p.put(0x54, unsafe { &*guid });
        }
        const MAP: [(u32, u64); 14] = [
            (1, 0x10), (2, 0x40), (4, 0x80), (8, 0x100), (0x10, 0x200), (0x100, 0x4000), (0x200, 0x8000), (0x400, 0x1_0000),
            (0x1000, 0x4_0000), (0x800, 0x400), (0x2000, 0x800), (0x4000, 0x1000), (0x8000, 0x2_0000), (0x1_0000, 0x8_0000),
        ];
        p.u64(0x10, MAP.iter().filter(|(f, _)| flags & f != 0).fold(0, |m, (_, v)| m | v));
        p.u32(0, 0x68);
        p.u32(4, (src.len() * 2) as u32);
        let dst_at = src.len() * 2 + 0x6a;
        p.u32(8, dst_at as u32);
        p.u32(0xc, (dst.len() * 2) as u32);
        let mut cursor = dst_at + 2 + dst.len() * 2;
        let mut place_opt = |p: &mut Payload, s: &Option<&[u16]>, off: usize| -> Option<usize> {
            let s = (*s)?;
            p.u32(off + 4, (s.len() * 2) as u32);
            if s.is_empty() {
                return None;
            }
            p.u32(off, cursor as u32);
            let at = cursor;
            cursor += s.len() * 2 + 2;
            Some(at)
        };
        let name_at = place_opt(&mut p, &name, 0x20);
        let content_at = place_opt(&mut p, &content, 0x4c);
        let extra_at = place_opt(&mut p, &extra, 0x44);
        if count != 0 {
            p.u32(0x34, count);
            p.u32(0x2c, cursor as u32);
            p.u32(0x30, count.wrapping_mul(16));
        }
        let status = copy_string(p.body(), 0x68, src.len() * 2 + 2, src);
        if status < 0 {
            return Ok(status);
        }
        let status = copy_string(p.body(), dst_at, dst.len() * 2 + 2, dst);
        if status < 0 {
            return Ok(status);
        }
        for (at, s) in [(name_at, &name), (content_at, &content), (extra_at, &extra)] {
            if let (Some(at), Some(s)) = (at, s) {
                let status = copy_string(p.body(), at, s.len() * 2 + 2, s);
                if status < 0 {
                    return Ok(status);
                }
            }
        }
        if count != 0 {
            let table = p.read_u32(0x2c) as usize;
            let strings = table + p.read_u32(0x30) as usize;
            if let Err(status) = specifier_table(&mut p, table, strings, 0x30, &pairs) {
                return Ok(status);
            }
        }
        let hr = send(handle_of(adapter), &mut p);
        let b = &p.bytes[super::srb::HEADER..];
        let q = |at: usize| u64::from_le_bytes(b[at..at + 8].try_into().unwrap());
        // SAFETY: the caller's id (not checked) and optional outs.
        unsafe {
            if hr >= 0 {
                *id = q(0x18);
            }
            if !out64.is_null() {
                *out64 = q(0x38);
            }
            if !out32.is_null() {
                *out32 = q(0x40) as u32;
            }
        }
        Ok(hr)
    })())
}

/// The caller's optional outs, zeroed when given.
fn zero<T: Default>(out: *mut T) {
    if !out.is_null() {
        // SAFETY: the caller's, checked non-null.
        unsafe { *out = T::default() };
    }
}

fn store<T>(out: *mut T, value: T) {
    if !out.is_null() {
        // SAFETY: as above.
        unsafe { *out = value };
    }
}

fn spec_pairs<'a>(count: u32, specs: *const [*const u16; 2]) -> Vec<(&'a [u16], &'a [u16])> {
    // SAFETY: `count` pairs of strings.
    (0..count as usize).map(|i| unsafe { let s = *specs.add(i); (text(s[0]), text(s[1])) }).collect()
}

fn spec_size(pairs: &[(&[u16], &[u16])]) -> u32 {
    pairs.iter().fold(0u32, |n, (a, b)| n.wrapping_add(((a.len() + b.len()) * 2 + 4) as u32))
}

/// 0x18000a0e0: updates a base XVC from an updated one (control 0x30, as
/// streaming). Flag 1 asks only the update's size, 2 a hybrid update, 8
/// specifier selection (not with 2), 0x10 another mode; the size and free
/// space come back, and a byte and a value whatever the driver said.
#[allow(clippy::too_many_arguments)]
pub extern "system" fn XCrdUpdateXvc(
    adapter: *const Adapter,
    base: *const u16,
    updated: *const u16,
    extra: *const u16,
    count: u32,
    specs: *const [*const u16; 2],
    flags: u32,
    update_size: *mut u64,
    free_space: *mut u64,
    out8: *mut u8,
    out32: *mut u32,
) -> HRESULT {
    zero(out8);
    zero(out32);
    zero(free_space);
    result((|| {
        check(!adapter.is_null(), "Invalid Adapter Handle\n")?;
        check(!base.is_null(), "BaseXvcPath must be specified\n")?;
        check(!updated.is_null(), "UpdatedXvcPath must be specified\n")?;
        check(flags & 0xffff_ffc4 == 0, "Invalid Flags\n")?;
        check(flags & 0xa != 0xa, "Cannot combine XCRD_UPDATE_XVC_FLAG_SPECIFIER_UPDATE with HYBRID flags\n")?;
        check(flags & 3 == 0 || !update_size.is_null() || !free_space.is_null(), "UpdateSize and FreeSpaceRequired cannot be null\n")?;
        if count == 0 {
            check(specs.is_null(), "Specifiers must be NULL if zero specifiers are passed\n")?;
        } else {
            check(!specs.is_null(), "Specifiers must not be NULL\n")?;
        }
        let (base, updated) = (text(base), text(updated));
        let extra = (!extra.is_null()).then(|| text(extra));
        let pairs = spec_pairs(count, specs);
        let mut size = ((base.len() + updated.len()) as u32).wrapping_mul(2).wrapping_add(0x6c);
        if let Some(e) = extra {
            size = size.wrapping_add((e.len() * 2 + 2) as u32);
        }
        size = size.wrapping_add(count.wrapping_shl(4)).wrapping_add(spec_size(&pairs));
        check(size as usize <= super::srb::MAX_BODY, "Payload too large for XVIO\n")?;
        let mut p = Payload::new(0x7800_0030, size as usize);
        p.u32(0, 0x68);
        p.u32(4, (updated.len() * 2) as u32);
        let base_at = updated.len() * 2 + 0x6a;
        p.u32(8, base_at as u32);
        p.u32(0xc, (base.len() * 2) as u32);
        let mut cursor = base_at + 2 + base.len() * 2;
        if let Some(e) = extra {
            p.u32(0x48, (e.len() * 2) as u32);
            if !e.is_empty() {
                p.u32(0x44, cursor as u32);
                cursor += e.len() * 2 + 2;
            }
        }
        if count != 0 {
            p.u32(0x34, count);
            p.u32(0x2c, cursor as u32);
            p.u32(0x30, count.wrapping_shl(4));
        }
        for (at, s) in [(0x68, updated), (base_at, base)] {
            let status = copy_string(p.body(), at, s.len() * 2 + 2, s);
            if status < 0 {
                return Ok(status);
            }
        }
        if let Some(e) = extra.filter(|e| !e.is_empty()) {
            let at = p.read_u32(0x44) as usize;
            let status = copy_string(p.body(), at, e.len() * 2 + 2, e);
            if status < 0 {
                return Ok(status);
            }
        }
        if count != 0 {
            let table = p.read_u32(0x2c) as usize;
            let strings = table + p.read_u32(0x30) as usize;
            if let Err(status) = specifier_table(&mut p, table, strings, 0x30, &pairs) {
                return Ok(status);
            }
        }
        let mut mode: u64 = if flags & 1 != 0 { 2 } else if flags & 2 != 0 { 4 } else { 1 };
        if flags & 8 != 0 {
            mode |= 0x20;
        }
        if flags & 0x10 != 0 {
            mode |= 0x2000;
        }
        p.u64(0x10, mode);
        let hr = send(handle_of(adapter), &mut p);
        let b = &p.bytes[super::srb::HEADER..];
        let q = |at: usize| u64::from_le_bytes(b[at..at + 8].try_into().unwrap());
        if hr >= 0 {
            store(update_size, q(0x18));
        }
        store(out8, b[0x28]);
        store(out32, q(0x40) as u32);
        store(free_space, q(0x38));
        Ok(hr)
    })())
}

/// 0x18000a560 (control 0x110): action 0 sizes the update (both outs
/// needed), 1 applies it, anything else (negative too) a third action;
/// flag 0x10 is the only flag.
#[allow(clippy::too_many_arguments)]
pub extern "system" fn XCrdUpdateXvcEx(
    adapter: *const Adapter,
    base: *const u16,
    updated: *const u16,
    extra: *const u16,
    count: u32,
    specs: *const [*const u16; 2],
    action: i32,
    flags: u32,
    update_size: *mut u64,
    free_space: *mut u64,
    out32: *mut u32,
) -> HRESULT {
    zero(out32);
    zero(free_space);
    result((|| {
        check(!adapter.is_null(), "Invalid Adapter Handle\n")?;
        check(!base.is_null(), "BaseXvcPath must be specified\n")?;
        check(!updated.is_null(), "UpdatedXvcPath must be specified\n")?;
        check(action < 3, "Invalid update action\n")?;
        if action == 0 {
            check(!update_size.is_null(), "UpdateSize cannot be null\n")?;
            check(!free_space.is_null(), "FreeSpaceRequired cannot be null\n")?;
        }
        check(flags & 0xffff_ffef == 0, "Invalid flags\n")?;
        if count == 0 {
            check(specs.is_null(), "Specifiers must be NULL\n")?;
        } else {
            check(!specs.is_null(), "Specifiers must not be NULL\n")?;
        }
        let (base, updated) = (text(base), text(updated));
        let extra = (!extra.is_null()).then(|| text(extra));
        let pairs = spec_pairs(count, specs);
        let mut size = ((base.len() + updated.len()) as u32).wrapping_mul(2).wrapping_add(0x4c);
        if let Some(e) = extra {
            size = size.wrapping_add((e.len() * 2 + 2) as u32);
        }
        size = size.wrapping_add(count.wrapping_shl(4)).wrapping_add(spec_size(&pairs));
        check(size as usize <= super::srb::MAX_BODY, "Payload too large for XVIO\n")?;
        let mut p = Payload::new(0x7800_0110, size as usize);
        p.u32(0x24, match action { 0 => 0, 1 => 1, _ => 2 });
        if flags & 0x10 != 0 {
            p.u32(0x28, 1);
        }
        p.u32(0, 0x48);
        p.u32(4, (base.len() * 2) as u32);
        let updated_at = base.len() * 2 + 0x4a;
        p.u32(8, updated_at as u32);
        p.u32(0xc, (updated.len() * 2) as u32);
        let mut cursor = updated_at + 2 + updated.len() * 2;
        if let Some(e) = extra {
            p.u32(0x14, (e.len() * 2) as u32);
            if !e.is_empty() {
                p.u32(0x10, cursor as u32);
                cursor += e.len() * 2 + 2;
            }
        }
        if count != 0 {
            p.u32(0x20, count);
            p.u32(0x18, cursor as u32);
            p.u32(0x1c, count.wrapping_shl(4));
        }
        for (at, s) in [(0x48, base), (updated_at, updated)] {
            let status = copy_string(p.body(), at, s.len() * 2 + 2, s);
            if status < 0 {
                return Ok(status);
            }
        }
        if let Some(e) = extra {
            // An empty string is copied too: its NUL lands at offset 0.
            let (at, cap) = (p.read_u32(0x10) as usize, p.read_u32(0x14) as usize + 2);
            let status = copy_string(p.body(), at, cap, e);
            if status < 0 {
                return Ok(status);
            }
        }
        if count != 0 {
            let table = p.read_u32(0x18) as usize;
            let strings = table + p.read_u32(0x1c) as usize;
            if let Err(status) = specifier_table(&mut p, table, strings, 0x1c, &pairs) {
                return Ok(status);
            }
        }
        let hr = send(handle_of(adapter), &mut p);
        let b = &p.bytes[super::srb::HEADER..];
        let q = |at: usize| u64::from_le_bytes(b[at..at + 8].try_into().unwrap());
        if hr >= 0 {
            store(update_size, q(0x30));
        }
        store(free_space, q(0x38));
        store(out32, q(0x40) as u32);
        Ok(hr)
    })())
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
        hle_export!(XCrdRemapTest),
        hle_export!(XCrdRepairXvc),
        hle_export!(XCrdXBlobCopy),
        hle_export!(XCrdTrimXvd),
        hle_export!(XCrdTrimXvc),
        hle_export!(XCrdCreateXVD),
        hle_export!(XCrdCreateXvdPointer),
        hle_export!(XCrdStreamingStart),
        hle_export!(XCrdUpdateXvc),
        hle_export!(XCrdUpdateXvcEx),
        hle_export!(XCrdSetFlatContentPath),
        hle_export!(XCrdStorageQueryBlobAttributes),
        hle_export!(XCrdStreamingAdjustStreamingFlags),
        hle_export!(XCrdStreamingQueryInformationEx),
        hle_export!(XCrdStreamingQueryInformation),
        hle_export!(XCrdStreamingQueryInformationByPath),
    ]
}
