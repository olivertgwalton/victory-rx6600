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

/// 0x180004250: hands the driver a content's metadata and data bitmaps
/// and a 16-byte id; the number of blocks it has comes back (+0x10).
pub extern "system" fn XCrdPopulateSparseCacheBlocks(
    adapter: *const Adapter,
    path: *const u16,
    md: *const u8,
    md_len: u32,
    data: *const u8,
    data_len: u32,
    id: *const [u8; 16],
    downloaded: *mut u32,
) -> HRESULT {
    result((|| {
        check(!adapter.is_null(), "Invalid Adapter Handle.\n")?;
        check(!path.is_null(), "Invalid XcrdPath.\n")?;
        check(!md.is_null(), "Invalid MdBitmap.\n")?;
        check(!data.is_null(), "Invalid DataBitmap.\n")?;
        check(!downloaded.is_null(), "Invalid DownloadedBlocks.\n")?;
        // SAFETY: a NUL-terminated path.
        let units = unsafe { wide(path) };
        if units.is_empty() {
            return Err(super::E_INVALIDARG);
        }
        let size = u64::from(md_len) + 0x32 + units.len() as u64 * 2 + u64::from(data_len);
        if size > u64::from(u32::MAX) {
            return Err(0x8007_007a_u32 as i32);
        }
        let mut p = Payload::new(0x7800_0184, size as usize);
        let bytes = units.len() * 2;
        let md_at = bytes + 0x32;
        p.u32(0x2c, size as u32);
        p.u32(0x24, 0x30);
        p.u32(0x28, bytes as u32);
        p.u32(0x18, md_at as u32);
        p.u32(0x14, md_len);
        p.u32(0x20, md_at as u32 + md_len);
        p.u32(0x1c, data_len);
        // SAFETY: the caller's id (not checked) and bitmaps of their lengths.
        unsafe {
            p.put(0, &*id);
            let status = copy_string(p.body(), 0x30, bytes + 2, units);
            if status < 0 {
                return Ok(status);
            }
            p.put(md_at, std::slice::from_raw_parts(md, md_len as usize));
            p.put(md_at + md_len as usize, std::slice::from_raw_parts(data, data_len as usize));
        }
        let hr = send(handle_of(adapter), &mut p);
        if hr >= 0 {
            // SAFETY: checked non-null.
            unsafe { *downloaded = p.read_u32(0x10) };
        }
        Ok(hr)
    })())
}

/// 0x180005a70: an XVD's block allocation table, into a buffer rounded up
/// to pages; its description (0x34 bytes) comes back too, also after a
/// too-small buffer.
pub extern "system" fn XCrdReadBatInfoXVD(adapter: *const Adapter, path: *const u16, buffer: *mut u8, len: *mut u32, info: *mut [u8; 0x34]) -> HRESULT {
    result((|| {
        // SAFETY: the caller's length (read without a check when there is a
        // buffer, as the library's).
        check(buffer.is_null() || unsafe { *len } != 0, "Can't specify buffer with zero length\n")?;
        check(!adapter.is_null(), "Invalid Adapter Handle.\n")?;
        check(!path.is_null(), "Invalid Input Path.\n")?;
        if buffer.is_null() {
            check(!len.is_null(), "BufferLength must not be NULL if Buffer is NULL\n")?;
            // SAFETY: checked non-null.
            check(unsafe { *len } == 0, "BufferLength must not be zero if Buffer is NULL\n")?;
        }
        // SAFETY: a NUL-terminated path; the length is non-null by now.
        let (units, n) = unsafe { (wide(path), *len) };
        let pages = n.wrapping_add(0xfff) & 0xffff_f000;
        let mut size = (units.len() * 2 + 0x4a) as u32;
        if n != 0 {
            size = size.wrapping_add(pages);
        }
        let mut p = Payload::new(0x7800_002c, size as usize);
        p.u32(0x20, pages);
        p.u32(0x18, 0x48);
        p.u32(0x1c, (units.len() * 2) as u32);
        let status = copy_string(p.body(), 0x48, units.len() * 2 + 2, units);
        if status < 0 {
            return Ok(status);
        }
        let hr = send(handle_of(adapter), &mut p);
        if hr < 0 && hr != too_small() {
            return Ok(hr);
        }
        let b = p.bytes[HEADER..].to_vec();
        let rd = |at: usize, k: usize| b[at..at + k].to_vec();
        if hr >= 0 && n != 0 {
            // From the path's last character on (0x180005bf8: (len + 0x24) * 2).
            let at = units.len() * 2 + 0x48;
            let k = (u32::from_le_bytes(b[0x20..0x24].try_into().unwrap()) as usize).min(b.len().saturating_sub(at));
            // SAFETY: the caller's buffer (the driver keeps to its pages).
            unsafe { std::ptr::copy_nonoverlapping(b[at..].as_ptr(), buffer, k) };
        }
        // SAFETY: the caller's description (not checked) and length.
        unsafe {
            let o = &mut *info;
            for (to, from, k) in [(0, 8, 8), (8, 0, 8), (0x10, 0x14, 4), (0x14, 0x10, 4), (0x18, 0x28, 8), (0x24, 0x34, 4), (0x20, 0x30, 4), (0x28, 0x38, 8), (0x30, 0x40, 4)] {
                o[to..to + k].copy_from_slice(&rd(from, k));
            }
            *len = u32::from_le_bytes(b[0x20..0x24].try_into().unwrap());
        }
        Ok(hr)
    })())
}

/// 0x180005c90: reads an XVD's user data from a page-aligned offset, in
/// chunks; each chunk's count is what the driver read (a chunk of 0 would
/// repeat for good, as the library's).
pub extern "system" fn XCrdReadUserDataXVD(adapter: *const Adapter, path: *const u16, mut offset: u64, mut buffer: *mut u8, len: *mut u32) -> HRESULT {
    const USER_DATA: u32 = 0x7800_0028;
    result((|| {
        // SAFETY: as in XCrdStorageReadBlobEx.
        check(buffer.is_null() || unsafe { *len } != 0, "Can't specify buffer with zero length\n")?;
        check(!adapter.is_null(), "Invalid Adapter Handle.\n")?;
        check(!path.is_null(), "Invalid Input Path.\n")?;
        check(!buffer.is_null() || !len.is_null(), "BufferLength must not be NULL if Buffer is NULL\n")?;
        check(offset & 0xfff == 0, "Offset has to be page aligned\n")?;
        // SAFETY: non-null by now.
        let mut left = unsafe { *len };
        if left == 0 {
            return Ok(transfer(adapter, path, offset, buffer, len, USER_DATA, 0));
        }
        // SAFETY: a NUL-terminated path.
        let path_bytes = (unsafe { wide(path) }.len() * 2 + 2) as u32;
        let mut total = 0u32;
        loop {
            let mut got = chunk(path_bytes, left);
            let hr = transfer(adapter, path, offset, buffer, &mut got, USER_DATA, 0);
            if hr < 0 {
                if hr == too_small() {
                    // SAFETY: as above.
                    unsafe { *len = total };
                }
                return Ok(hr);
            }
            total = total.wrapping_add(got);
            // SAFETY: within the caller's buffer.
            buffer = unsafe { buffer.add(got as usize) };
            offset = offset.wrapping_add(u64::from(got));
            left = left.wrapping_sub(got);
            if left == 0 {
                // SAFETY: as above.
                unsafe { *len = total };
                return Ok(hr);
            }
        }
    })())
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
        hle_export!(XCrdPopulateSparseCacheBlocks),
        hle_export!(XCrdReadBatInfoXVD),
        hle_export!(XCrdReadUserDataXVD),
    ]
}
