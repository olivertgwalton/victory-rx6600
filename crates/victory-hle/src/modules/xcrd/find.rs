//! Enumerations: the blobs of a store (the driver's, or files on the O:
//! drive for kind 9) and the storage devices. Each keeps an adapter of its
//! own. Addresses are `XCRDAPI.dll`'s.

use super::srb::{HEADER, Payload, copy_string, send};
use super::sys::{self, Handle, sys};
use super::{Adapter, DOMAINS, E_INVALIDARG, HRESULT, XCrdCloseAdapter, XCrdOpenAdapter, handle_of, heap_alloc, heap_free, hresult_from_win32, units, wide};

const E_POINTER: HRESULT = 0x8000_4003_u32 as i32;
const E_OUTOFMEMORY: HRESULT = 0x8007_000e_u32 as i32;
/// ERROR_NOT_ENOUGH_MEMORY, as the blob records' allocations say it.
const NO_MEMORY: HRESULT = 0x8007_0008_u32 as i32;
const BACKSLASH: u16 = b'\\' as u16;

fn u32_at(b: &[u8], at: usize) -> u32 {
    u32::from_le_bytes(b[at..at + 4].try_into().unwrap())
}

fn u64_at(b: &[u8], at: usize) -> u64 {
    u64::from_le_bytes(b[at..at + 8].try_into().unwrap())
}

/// The characters at `at` up to a NUL (or the end).
fn string_at(b: &[u8], at: usize) -> Vec<u16> {
    b[at..].chunks_exact(2).map(|c| u16::from_le_bytes([c[0], c[1]])).take_while(|&c| c != 0).collect()
}

// Storage devices.

/// The search: its adapter, the driver's cursor and the current device.
#[repr(C)]
pub struct DeviceFind {
    adapter: *mut Adapter,
    cursor: u32,
    id: u32,
}

/// 0x180010248: one step (0 first, 1 next, 2 end; control 0x8c).
fn device_step(h: &mut DeviceFind, op: u32) -> HRESULT {
    let mut p = Payload::new(0x7800_008c, 0xc);
    p.u32(0, op);
    p.u32(8, h.cursor);
    p.u32(4, h.id);
    let hr = send(handle_of(h.adapter), &mut p);
    if hr >= 0 {
        h.id = p.read_u32(4);
        h.cursor = p.read_u32(8);
    }
    hr
}

fn drop_device_find(h: *mut DeviceFind) {
    // SAFETY: a search of ours.
    let adapter = unsafe { (*h).adapter };
    if !adapter.is_null() {
        XCrdCloseAdapter(adapter);
    }
    // SAFETY: made by Box::into_raw.
    drop(unsafe { Box::from_raw(h) });
}

/// 0x180010130.
pub extern "system" fn XCrdFindFirstStorageDevice(out: *mut *mut DeviceFind, id: *mut u32) -> HRESULT {
    if out.is_null() || id.is_null() {
        return E_POINTER;
    }
    // SAFETY: the caller's.
    unsafe { *out = std::ptr::null_mut() };
    let h = Box::into_raw(Box::new(DeviceFind { adapter: std::ptr::null_mut(), cursor: 0, id: 0 }));
    // SAFETY: ours.
    let mut hr = XCrdOpenAdapter(unsafe { &mut (*h).adapter });
    if hr >= 0 {
        // SAFETY: ours.
        hr = device_step(unsafe { &mut *h }, 0);
    }
    if hr < 0 {
        drop_device_find(h);
        return hr;
    }
    // SAFETY: the caller's outs, ours.
    unsafe {
        *id = (*h).id;
        *out = h;
    }
    hr
}

/// 0x180010200.
pub extern "system" fn XCrdFindNextStorageDevice(h: *mut DeviceFind, id: *mut u32) -> HRESULT {
    if h.is_null() {
        return E_INVALIDARG;
    }
    if id.is_null() {
        return E_POINTER;
    }
    // SAFETY: a search of ours.
    let h = unsafe { &mut *h };
    let hr = device_step(h, 1);
    if hr >= 0 {
        // SAFETY: the caller's.
        unsafe { *id = h.id };
    }
    hr
}

/// 0x1800100e0.
pub extern "system" fn XCrdFindCloseStorageDevice(h: *mut DeviceFind) -> HRESULT {
    if h.is_null() {
        return E_INVALIDARG;
    }
    // SAFETY: a search of ours.
    device_step(unsafe { &mut *h }, 2);
    drop_device_find(h);
    0
}

// Blobs.

/// A blob search. `mem` stands for the library's block at its own offsets
/// (0x28 on): the `WIN32_FIND_DATAW` of an O: search, or the driver's
/// cursor (0x28), name (0x48), size (0x250), time (0x258) and attributes
/// (0x370).
pub struct BlobFind {
    adapter: *mut Adapter,
    find: Handle,
    count: u32,
    flags: u32,
    record: *mut u8,
    mem: Box<[u8; 0x400]>,
    kind: u32,
    local: bool,
    /// The directory, `cap` bytes (a NUL-terminated string).
    path: Vec<u8>,
}

/// 0x18000fb7c: a step of the driver's enumeration (control 0x84): 0
/// first (its cursor kept), 1 next, 2 end.
fn blob_step(h: &mut BlobFind, op: u32, path: Option<&[u16]>) -> HRESULT {
    let mut p = Payload::new(0x7800_0084, 0x3d0);
    p.u32(0x10, h.kind);
    p.u32(0x18, op);
    p.put(0, &h.mem[0x28..0x38]);
    if let Some(path) = path {
        let status = copy_string(p.body(), 0x20, 0x208, path);
        if status < 0 {
            return status;
        }
    }
    let hr = send(handle_of(h.adapter), &mut p);
    if hr < 0 {
        return hr;
    }
    let body = &p.bytes[HEADER..];
    let name: Vec<u16> = body[0x20..].chunks_exact(2).map(|c| u16::from_le_bytes([c[0], c[1]])).collect();
    let status = copy_string(&mut h.mem[..], 0x48, 0x208, &name);
    if status < 0 {
        return status;
    }
    h.mem[0x370..0x374].copy_from_slice(&body[0x348..0x34c]);
    h.mem[0x258..0x260].copy_from_slice(&body[0x230..0x238]);
    h.mem[0x250..0x258].copy_from_slice(&body[0x228..0x230]);
    if op == 0 {
        h.mem[0x28..0x38].copy_from_slice(&body[0..0x10]);
    }
    status
}

/// 0x18000fec8: the current entry's name, time, size and attributes.
fn current(h: &BlobFind) -> (Vec<u16>, u64, u64, u32) {
    let m = &h.mem[..];
    if h.local {
        let size = (u64::from(u32_at(m, 0x44)) << 32) | u64::from(u32_at(m, 0x48));
        (string_at(m, 0x54), u64_at(m, 0x2c), size, u32_at(m, 0x28))
    } else {
        (string_at(m, 0x48), u64_at(m, 0x258), u64_at(m, 0x250), u32_at(m, 0x370))
    }
}

/// 0x18000fe7c.
fn free_record(record: *mut u8) {
    // SAFETY: a record of ours: the header block's pointer at 0x28.
    let extra = unsafe { record.add(0x28).cast::<*mut u8>().read_unaligned() };
    if !extra.is_null() {
        heap_free(extra.cast());
    }
    heap_free(record.cast());
}

/// 0x18000ff2c: an XVD file's header (0xe00 bytes from 0x200).
fn read_header(path: &[u16], out: &mut [u8]) -> HRESULT {
    let s = sys();
    let mut name = path.to_vec();
    name.push(0);
    // SAFETY: a NUL-terminated path; read, shared both ways, existing.
    let h = unsafe { (s.create_file_w)(name.as_ptr(), 0x8000_0000, 3, std::ptr::null(), 3, 0, std::ptr::null_mut()) };
    if h as isize == -1 {
        return hresult_from_win32(sys::last_error());
    }
    let mut read = 0u32;
    // SAFETY: our handle and buffer.
    let ok = unsafe { (s.set_file_pointer_ex)(h, 0x200, std::ptr::null_mut(), 0) != 0 && (s.read_file)(h, out.as_mut_ptr().cast(), 0xe00, &mut read, std::ptr::null_mut()) != 0 };
    let hr = if !ok {
        hresult_from_win32(sys::last_error())
    } else if read < 0xe00 {
        E_INVALIDARG
    } else {
        0
    };
    // SAFETY: ours.
    unsafe { (s.close_handle)(h) };
    hr
}

/// 0x18000fffc: a plausible XVD header.
fn valid_header(h: &[u8]) -> bool {
    let size = u64_at(h, 0x18);
    &h[..8] == b"msft-xvd"
        && u32_at(h, 0x18) & 0xfff == 0
        && size.wrapping_sub(0x10_0000) <= 0xff_fff0_0000
        && u32_at(h, 0x80) <= 1
        && u16::from_le_bytes([h[0xa], h[0xb]]) == 0
        && (u32_at(h, 0x90) == 0 || {
            let kind = u32_at(h, 0x84) as i32;
            (0..=0x28).contains(&kind) && 0x100_0701_8042u64 >> kind & 1 != 0
        })
        && u32_at(h, 0xc) == 3
        && u32_at(h, 0x88) & 0xfff == 0
}

/// 0x18000fca0: with flag 1 and a file of a header's size or more, the
/// blob's basic information (`{0xe08, valid, info...}`): the driver's
/// (class 3), or read from the O: file and rearranged in place.
fn attach_info(adapter: *const Adapter, flags: u32, record: &mut [u8], kind: u32) -> HRESULT {
    if flags & 1 == 0 || (u64_at(record, 0x30) as i64) < 0xe00 {
        return 0;
    }
    let block = heap_alloc(0xe08);
    if block.is_null() {
        return NO_MEMORY;
    }
    // SAFETY: a new block of 0xe08 bytes.
    let x = unsafe { std::slice::from_raw_parts_mut(block, 0xe08) };
    x[8..].fill(0);
    x[0..4].copy_from_slice(&0xe08u32.to_le_bytes());
    x[4..8].fill(0);
    let hr;
    if kind != 9 {
        let path = string_at(record, 0x10 + u32_at(record, 0x20) as usize);
        let mut name = path.clone();
        name.push(0);
        hr = super::info::XCrdQueryXvdInformation(adapter, name.as_ptr(), 3, x[8..].as_mut_ptr(), 0x80);
        record[0x48..0x4c].copy_from_slice(&hr.to_le_bytes());
        if hr < 0 {
            heap_free(block.cast());
            return 0;
        }
        x[4] = 1;
    } else {
        let path = string_at(record, 0x10 + u32_at(record, 0x18) as usize);
        hr = read_header(&path, &mut x[8..]);
        record[0x48..0x4c].copy_from_slice(&hr.to_le_bytes());
        if hr < 0 {
            heap_free(block.cast());
            return hr;
        }
        if valid_header(&x[8..]) {
            x[4] = 1;
            // The library's moves, in its order (they overlap the header).
            let mv = |x: &mut [u8], to: usize, from: usize, n: usize| {
                let v = x[from..from + n].to_vec();
                x[to..to + n].copy_from_slice(&v);
            };
            let h = 8;
            mv(x, 0x10, h + 0x84, 4);
            mv(x, 0x18, h + 0x10, 8);
            mv(x, 0x20, h + 0x18, 8);
            mv(x, 0x14, h + 0x8, 4);
            mv(x, h, h + 0xc, 2);
            mv(x, 0xa, h + 0x271, 1);
            mv(x, 0x58, h + 0x1bc, 8);
            mv(x, 0x60, h + 0x18c, 16);
            mv(x, 0xc, h + 0x80, 4);
            mv(x, 0x70, h + 0x19c, 16);
            mv(x, 0x48, h + 0x1ac, 16);
            mv(x, 0x38, h + 0x30, 16);
            mv(x, 0x28, h + 0x20, 16);
        }
    }
    let f = u32_at(record, 0x40) | 1;
    record[0x40..0x44].copy_from_slice(&f.to_le_bytes());
    record[0x28..0x30].copy_from_slice(&(block as usize).to_le_bytes());
    hr
}

/// 0x18000f8b0: the record of the current entry: `{size, search, name
/// offset/bytes, path offset (0x90)/bytes, CRD path offset/bytes, info,
/// size, time, flags, kind, info status}` then the strings: the path
/// (directory and name) and its CRD path (`[XOD:]\O:...` for kind 9, the
/// kind's prefix otherwise).
fn build_record(h: &mut BlobFind, name: &[u16], size: u64, time: u64, attributes: u32) -> HRESULT {
    if !h.record.is_null() {
        free_record(h.record);
        h.record = std::ptr::null_mut();
    }
    let dir = string_at(&h.path, 0);
    let total = ((dir.len() + name.len()) as u32).wrapping_mul(4).wrapping_add(0xc8) as usize;
    let block = heap_alloc(total);
    if block.is_null() {
        return NO_MEMORY;
    }
    // SAFETY: a new block of `total` bytes.
    let r = unsafe { std::slice::from_raw_parts_mut(block, total) };
    r.fill(0);
    let fail = |status: HRESULT| {
        heap_free(block.cast());
        status
    };
    let status = copy_string(r, 0xa0, total - 0xa0, name);
    if status < 0 {
        return fail(status);
    }
    r[8..16].copy_from_slice(&(h as *const BlobFind as usize).to_le_bytes());
    r[0x38..0x40].copy_from_slice(&time.to_le_bytes());
    r[0x30..0x38].copy_from_slice(&size.to_le_bytes());
    r[0..4].copy_from_slice(&(total as u32).to_le_bytes());
    r[0x44..0x48].copy_from_slice(&h.kind.to_le_bytes());
    r[0x18..0x1c].copy_from_slice(&0x90u32.to_le_bytes());
    let full: Vec<u16> = dir.iter().copied().chain([BACKSLASH]).chain(name.iter().copied()).collect();
    let status = copy_string(r, 0xa0, total - 0xa0, &full);
    if status < 0 {
        return fail(status);
    }
    let written = string_at(r, 0xa0);
    let n = written.len();
    r[0x1c..0x20].copy_from_slice(&((n * 2) as u32).to_le_bytes());
    let remaining = ((total - 0xa0) as u32).wrapping_sub((n * 2) as u32).wrapping_sub(2) as usize;
    let leaf = written.iter().rposition(|&c| c == BACKSLASH).map_or(0xa0, |i| 0xa0 + 2 * (i + 1));
    r[0x10..0x14].copy_from_slice(&((leaf - 0x10) as u32).to_le_bytes());
    let leaf_len = string_at(r, leaf).len();
    r[0x14..0x18].copy_from_slice(&((leaf_len * 2) as u32).to_le_bytes());
    let at = 0xa0 + n * 2 + 2;
    let crd: Vec<u16> = if h.kind == 9 {
        let rest = dir.iter().position(|&c| c == BACKSLASH).map_or(&[][..], |i| &dir[i..]);
        [units("[XOD:]"), vec![BACKSLASH], units("O:"), rest.to_vec(), vec![BACKSLASH], name.to_vec()].concat()
    } else {
        let (first, d) = if dir.first() == Some(&BACKSLASH) { (dir.get(1).copied(), &dir[1..]) } else { (dir.first().copied(), &dir[..]) };
        let sep = if first.unwrap_or(0) == 0 || d.contains(&BACKSLASH) { vec![] } else { vec![BACKSLASH] };
        let prefix = DOMAINS.get(h.kind as usize).copied().unwrap_or("");
        [units(prefix), vec![BACKSLASH], d.to_vec(), sep, name.to_vec()].concat()
    };
    let status = copy_string(r, at, remaining, &crd);
    if status < 0 {
        return fail(status);
    }
    r[0x20..0x24].copy_from_slice(&((at - 0x10) as u32).to_le_bytes());
    let crd_len = string_at(r, at).len();
    r[0x24..0x28].copy_from_slice(&((crd_len * 2) as u32).to_le_bytes());
    let mut f = 0u32;
    if attributes & 0x10 != 0 {
        f |= 2;
    }
    if h.kind == 9 {
        f |= 4;
    }
    r[0x40..0x44].copy_from_slice(&f.to_le_bytes());
    let mut hr = status;
    if attributes & 0x10 == 0 {
        hr = attach_info(h.adapter, h.flags, r, h.kind);
        if hr < 0 {
            return fail(hr);
        }
    }
    h.record = block;
    hr
}

/// A directory less one leading separator.
fn skip(d: &[u16]) -> &[u16] {
    if d.first() == Some(&BACKSLASH) { &d[1..] } else { d }
}

fn drop_blob_find(h: Box<BlobFind>) {
    if !h.adapter.is_null() {
        XCrdCloseAdapter(h.adapter);
    }
}

/// 0x18000f4a0: the first blob of a store (kind 9: files on the O: drive,
/// which must be a CD drive; others, the driver's). `path` is a directory
/// in it; flag 1 adds each file's basic information. The record comes back
/// past its first 0x10 bytes.
pub extern "system" fn XCrdFindFirstBlob(out: *mut *mut BlobFind, record: *mut *mut u8, kind: u32, path: *const u16, flags: u32) -> HRESULT {
    if flags & !1 != 0 {
        return E_INVALIDARG;
    }
    let local = kind == 9;
    if local {
        let root = [u16::from(b'O'), u16::from(b':'), 0];
        // SAFETY: a NUL-terminated root.
        if unsafe { (sys().get_drive_type_w)(root.as_ptr()) } != 5 {
            return 0x8007_0490_u32 as i32;
        }
    }
    // SAFETY: a NUL-terminated directory, if any.
    let dir = (!path.is_null()).then(|| unsafe { wide(path) });
    let size = (dir.map_or(0, |d| d.len() * 2) as u32).wrapping_add(0x412) as usize;
    let mut h = Box::new(BlobFind {
        adapter: std::ptr::null_mut(),
        find: std::ptr::null_mut(),
        count: 0,
        flags: 0,
        record: std::ptr::null_mut(),
        mem: Box::new([0; 0x400]),
        kind: 0,
        local,
        path: vec![0; size - 0x400],
    });
    let hr = XCrdOpenAdapter(&mut h.adapter);
    if hr < 0 {
        return hr;
    }
    let cap = h.path.len();
    let given = dir.filter(|d| !d.is_empty());
    let text: Vec<u16> = match (local, given) {
        (true, Some(d)) => [units("O:"), vec![BACKSLASH], skip(d).to_vec()].concat(),
        (true, None) => units("O:\\*.*"),
        (false, Some(d)) => [vec![BACKSLASH], skip(d).to_vec(), vec![BACKSLASH]].concat(),
        (false, None) => units("\\"),
    };
    let status = copy_string(&mut h.path, 0, cap, &text);
    if status < 0 {
        drop_blob_find(h);
        return status;
    }
    h.flags = flags;
    h.kind = kind;
    let hr = if local {
        let mut pattern = string_at(&h.path, 0);
        pattern.push(0);
        // SAFETY: a NUL-terminated pattern and our WIN32_FIND_DATAW.
        h.find = unsafe { (sys().find_first_file_w)(pattern.as_ptr(), h.mem[0x28..].as_mut_ptr().cast()) };
        if h.find as isize == -1 { hresult_from_win32(sys::last_error()) } else { 0 }
    } else {
        blob_step(&mut h, 0, if local { None } else { given })
    };
    if hr < 0 {
        drop_blob_find(h);
        return hr;
    }
    let (name, time, size, attributes) = current(&h);
    let hr = match string_at(&h.path, 0).iter().rposition(|&c| c == BACKSLASH) {
        Some(i) => {
            h.path[2 * i..2 * i + 2].fill(0);
            build_record(&mut h, &name, size, time, attributes)
        }
        None => E_INVALIDARG,
    };
    if hr < 0 {
        if h.local {
            if !h.find.is_null() {
                // SAFETY: our search.
                unsafe { (sys().find_close)(h.find) };
            }
        } else {
            blob_step(&mut h, 2, None);
        }
        drop_blob_find(h);
        return hr;
    }
    let raw = Box::into_raw(h);
    // SAFETY: the caller's outs (not checked); ours.
    unsafe {
        *out = raw;
        *record = (*raw).record.add(0x10);
    }
    hr
}

/// 0x18000f7c0.
pub extern "system" fn XCrdFindNextBlob(h: *mut BlobFind, record: *mut *mut u8) -> HRESULT {
    // SAFETY: a search of ours (not checked).
    let h = unsafe { &mut *h };
    h.count += 1;
    let hr = if h.local {
        // SAFETY: our search and its data.
        if unsafe { (sys().find_next_file_w)(h.find, h.mem[0x28..].as_mut_ptr().cast()) } != 0 { 0 } else { hresult_from_win32(sys::last_error()) }
    } else {
        blob_step(h, 1, None)
    };
    if hr < 0 {
        return hr;
    }
    let (name, time, size, attributes) = current(h);
    let hr = build_record(h, &name, size, time, attributes);
    if hr >= 0 {
        // SAFETY: the caller's out.
        unsafe { *record = h.record.add(0x10) };
    }
    hr
}

/// 0x18000f420.
pub extern "system" fn XCrdFindCloseBlob(h: *mut BlobFind) -> HRESULT {
    // SAFETY: made by Box::into_raw.
    let mut h = unsafe { Box::from_raw(h) };
    if !h.record.is_null() {
        free_record(h.record);
        h.record = std::ptr::null_mut();
    }
    if h.local {
        if !h.find.is_null() {
            // SAFETY: our search.
            unsafe { (sys().find_close)(h.find) };
            h.find = std::ptr::null_mut();
        }
    } else {
        blob_step(&mut h, 2, None);
    }
    XCrdCloseAdapter(h.adapter);
    0
}

pub(crate) fn exports() -> Vec<crate::HleExport> {
    use crate::hle_export;
    vec![
        hle_export!(XCrdFindFirstStorageDevice),
        hle_export!(XCrdFindNextStorageDevice),
        hle_export!(XCrdFindCloseStorageDevice),
        hle_export!(XCrdFindFirstBlob),
        hle_export!(XCrdFindNextBlob),
        hle_export!(XCrdFindCloseBlob),
    ]
}
