//! Reading and writing storage blobs (0x18000e300 and its callers).
//! Addresses are `XCRDAPI.dll`'s.

use super::srb::{HEADER, MAX_BODY, Payload, check, copy_string, hresult, send};
use super::{Adapter, HRESULT, handle_of, wide};

const READ: u32 = 0x7800_0090;
const WRITE: u32 = 0x7800_0094;
/// HRESULT of STATUS_BUFFER_TOO_SMALL.
fn too_small() -> HRESULT {
    hresult(0xc000_0023_u32 as i32)
}

fn result(r: Result<HRESULT, HRESULT>) -> HRESULT {
    r.unwrap_or_else(|hr| hr)
}

/// 0x18000e300: one read or write: `{total, 0x20, path bytes, length,
/// offset, flags}`, the path at 0x20 and the data right after its
/// characters (over its NUL, as the library's). A read hands back what the
/// driver says it read, also when the buffer was too small.
pub(crate) fn transfer(adapter: *const Adapter, path: *const u16, offset: u64, buffer: *mut u8, len: *mut u32, code: u32, flags: u32) -> HRESULT {
    result((|| {
        check(!len.is_null(), "BufferLength must not be NULL\n")?;
        // SAFETY: checked non-null.
        let n = unsafe { *len };
        if buffer.is_null() {
            check(n == 0, "BufferLength must be zero if Buffer is null\n")?;
        } else {
            check(n != 0, "Can't specify buffer with zero length\n")?;
        }
        check(!adapter.is_null(), "Invalid Adapter Handle.\n")?;
        check(!path.is_null(), "Invalid Input Path.\n")?;
        check(flags & !3 == 0, "Invalid Read Write Flags\n")?;
        check(code == WRITE || flags & 3 == 0, "Only valid to request Truncate or Hidden file on write\n")?;
        // SAFETY: a NUL-terminated path.
        let units = unsafe { wide(path) };
        let bytes = units.len() * 2;
        let total = (n as usize).wrapping_add(0x22).wrapping_add(bytes) as u32 as usize;
        let mut p = Payload::new(code, total);
        p.u32(0, total as u32);
        p.u32(4, 0x20);
        p.u32(8, bytes as u32);
        p.u64(0x10, offset);
        p.u32(0xc, n);
        p.u32(0x18, flags);
        let status = copy_string(p.body(), 0x20, bytes + 2, units);
        if status < 0 {
            return Ok(status);
        }
        if code == WRITE {
            // SAFETY: the caller's `n` bytes.
            p.put(0x20 + bytes, unsafe { std::slice::from_raw_parts(buffer, n as usize) });
        }
        let hr = send(handle_of(adapter), &mut p);
        if code == WRITE {
            return Ok(hr);
        }
        let got = p.read_u32(0xc);
        if hr < 0 {
            if hr == too_small() {
                // SAFETY: checked non-null.
                unsafe { *len = got };
            }
            return Ok(hr);
        }
        if n != 0 {
            // The driver's count, as far as the block holds it.
            let data = &p.bytes[HEADER + 0x20 + bytes..];
            let k = (got as usize).min(data.len());
            // SAFETY: the caller's buffer (the driver keeps to its size).
            unsafe { std::ptr::copy_nonoverlapping(data.as_ptr(), buffer, k) };
        }
        // SAFETY: as above.
        unsafe { *len = got };
        Ok(hr)
    })())
}

/// The most of `left` bytes one block carries after a path of `path`
/// bytes (its NUL counted): all of it if it fits, else what fits in whole
/// pages (or a full 1 MiB past a very long path).
fn chunk(path: u32, left: u32) -> u32 {
    if path.wrapping_add(left) as usize <= MAX_BODY {
        left
    } else if path > 0xf_f000 {
        MAX_BODY as u32
    } else {
        (MAX_BODY as u32 - path) & !0xfff
    }
}

/// 0x180007d90: reads in chunks; a short chunk ends the read; the total
/// comes back (also after a too-small buffer).
pub extern "system" fn XCrdStorageReadBlobEx(adapter: *const Adapter, path: *const u16, mut offset: u64, mut buffer: *mut u8, len: *mut u32) -> HRESULT {
    result((|| {
        // SAFETY: the caller's length (read without a check when there is a
        // buffer, as the library's).
        check(buffer.is_null() || unsafe { *len } != 0, "Can't specify buffer with zero length\n")?;
        check(!adapter.is_null(), "Invalid Adapter Handle.\n")?;
        check(!path.is_null(), "Invalid Input Path.\n")?;
        check(!buffer.is_null() || !len.is_null(), "BufferLength must not be NULL if Buffer is NULL\n")?;
        // SAFETY: non-null by now.
        let mut left = unsafe { *len };
        if left == 0 {
            return Ok(transfer(adapter, path, 0, buffer, len, READ, 0));
        }
        // SAFETY: a NUL-terminated path.
        let path_bytes = (unsafe { wide(path) }.len() * 2 + 2) as u32;
        let mut total = 0u32;
        loop {
            let want = chunk(path_bytes, left);
            let mut got = want;
            let hr = transfer(adapter, path, offset, buffer, &mut got, READ, 0);
            if hr < 0 {
                if hr == too_small() {
                    // SAFETY: as above.
                    unsafe { *len = total };
                }
                return Ok(hr);
            }
            left = left.wrapping_sub(want);
            total = total.wrapping_add(got);
            offset = offset.wrapping_add(u64::from(got));
            // SAFETY: within the caller's buffer.
            buffer = unsafe { buffer.add(got as usize) };
            if got != want || left == 0 {
                // SAFETY: as above.
                unsafe { *len = total };
                return Ok(hr);
            }
        }
    })())
}

/// 0x180007d70: from offset 0.
pub extern "system" fn XCrdStorageReadBlob(adapter: *const Adapter, path: *const u16, buffer: *mut u8, len: *mut u32) -> HRESULT {
    XCrdStorageReadBlobEx(adapter, path, 0, buffer, len)
}

/// 0x180008120: writes in chunks; flag 1 (truncate) goes with the last
/// one, flag 2 with every one.
pub extern "system" fn XCrdStorageWriteBlobEx(adapter: *const Adapter, path: *const u16, mut offset: u64, mut buffer: *const u8, len: u32, flags: u32) -> HRESULT {
    result((|| {
        check(!buffer.is_null() && len != 0, "Can't specify buffer with zero length\n")?;
        check(!adapter.is_null(), "Invalid Adapter Handle.\n")?;
        check(!path.is_null(), "Invalid Input Path.\n")?;
        check(flags & !3 == 0, "Invalid Write Blob Flags\n")?;
        // SAFETY: a NUL-terminated path.
        let path_bytes = (unsafe { wide(path) }.len() * 2 + 2) as u32;
        let mut sent = flags & 2;
        let mut left = len;
        loop {
            let mut n = chunk(path_bytes, left);
            if path_bytes.wrapping_add(left) as usize <= MAX_BODY && flags & 1 != 0 {
                sent |= 1;
            }
            let hr = transfer(adapter, path, offset, buffer.cast_mut(), &mut n, WRITE, sent);
            if hr < 0 {
                return Ok(hr);
            }
            // SAFETY: within the caller's buffer.
            buffer = unsafe { buffer.add(n as usize) };
            offset = offset.wrapping_add(u64::from(n));
            left = left.wrapping_sub(n);
            if left == 0 {
                return Ok(hr);
            }
        }
    })())
}

/// 0x1800080f0: from offset 0, truncating.
pub extern "system" fn XCrdStorageWriteBlob(adapter: *const Adapter, path: *const u16, buffer: *const u8, len: u32) -> HRESULT {
    XCrdStorageWriteBlobEx(adapter, path, 0, buffer, len, 1)
}

/// 0x180007d40: an external device's address as the driver's form: the
/// size (8) and the value's bytes, most significant first.
pub extern "system" fn XCrdStorageQueryExternalDeviceAddress(out: *mut [u8; 8], value: u32) -> HRESULT {
    let mut a = [0u8; 8];
    a[..4].copy_from_slice(&8u32.to_le_bytes());
    a[4..].copy_from_slice(&value.to_be_bytes());
    // SAFETY: the caller's (not checked).
    unsafe { *out = a };
    0
}

pub(crate) fn exports() -> Vec<crate::HleExport> {
    use crate::hle_export;
    vec![
        hle_export!(XCrdStorageReadBlobEx),
        hle_export!(XCrdStorageReadBlob),
        hle_export!(XCrdStorageWriteBlobEx),
        hle_export!(XCrdStorageWriteBlob),
        hle_export!(XCrdStorageQueryExternalDeviceAddress),
    ]
}
