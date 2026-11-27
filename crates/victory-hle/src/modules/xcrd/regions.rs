//! Streaming regions: information by id or offset, specifiers, and the
//! region queue. The list functions split a request into chunks that fit
//! the driver's 1 MiB limit, halving the chunk until it does, as the
//! library does. Addresses are `XCRDAPI.dll`'s.

use super::srb::{HEADER, MAX_BODY, Payload, check, copy_string, send};
use super::{Adapter, E_INVALIDARG, HRESULT, handle_of, wide};

fn result(r: Result<HRESULT, HRESULT>) -> HRESULT {
    r.unwrap_or_else(|hr| hr)
}

fn text<'a>(path: *const u16) -> &'a [u16] {
    // SAFETY: a NUL-terminated path.
    unsafe { wide(path) }
}

fn rd32(b: &[u8], at: usize) -> u32 {
    u32::from_le_bytes(b[at..at + 4].try_into().unwrap())
}

/// Copies `n` bytes of `from` at `src` to `to` at `dst`.
fn mv(to: &mut [u8], dst: usize, from: &[u8], src: usize, n: usize) {
    to[dst..dst + n].copy_from_slice(&from[src..src + n]);
}

/// 0x180008840: the region holding an offset of a content's file (kind 0
/// or 1); 0x20 bytes back.
pub extern "system" fn XCrdStreamingQueryRegionIdByOffset(out: *mut [u8; 0x20], adapter: *const Adapter, path: *const u16, kind: u32, offset: u64) -> HRESULT {
    result((|| {
        check(!adapter.is_null(), "Invalid Adapter Handle\n")?;
        check(!path.is_null(), "Invalid XCRD Path\n")?;
        let path = text(path);
        check(!path.is_empty(), "Invalid XCRD Path - 2\n")?;
        check(kind <= 1, "Invalid XCRD Type\n")?;
        let mut p = Payload::new(0x7800_0048, path.len() * 2 + 0x5a);
        p.u64(0x10, offset);
        p.u32(8, kind);
        p.u32(0, 0x58);
        p.u32(4, (path.len() * 2 + 2) as u32);
        let status = copy_string(p.body(), 0x58, path.len() * 2 + 4, path);
        if status < 0 {
            return Ok(status);
        }
        let hr = send(handle_of(adapter), &mut p);
        if hr >= 0 {
            let b = &p.bytes[HEADER..];
            // SAFETY: the caller's (not checked).
            let out = unsafe { &mut *out };
            let mine = rd32(out, 4);
            out[4..8].copy_from_slice(&((mine & !0x7f) | (rd32(b, 0x24) & 0x7f)).to_le_bytes());
            mv(out, 8, b, 0x28, 8);
            mv(out, 0x10, b, 0x30, 8);
            mv(out, 0, b, 0x20, 4);
            mv(out, 0x18, b, 0x38, 8);
        }
        Ok(hr)
    })())
}

/// 0x18000eac4: one chunk of region information: `count` region ids in,
/// a 0x38-byte entry each back, kept as 0x30-byte entries. By id (flag 0:
/// `source` is the id) or by path (flag 1).
fn region_info_chunk(out: *mut u8, adapter: *const Adapter, source: *const u16, flags: u32, count: u32, regions: *const u32) -> HRESULT {
    result((|| {
        check(!adapter.is_null(), "Adapter Handle should be non zero\n")?;
        check(flags & !1 == 0, "Invalid Query Region Flags\n")?;
        let by_path = flags & 1 != 0;
        check(!by_path || !source.is_null(), "Must Specify Source Path\n")?;
        let n = count as usize;
        let path = if by_path { text(source) } else { &[] };
        let mut len = n * 0x3c + 0x18;
        if by_path {
            len += path.len() * 2 + 2;
        }
        let mut p = Payload::new(0x7800_0040, len);
        let ids;
        if by_path {
            let status = copy_string(p.body(), 0x18, path.len() * 2 + 2, path);
            if status < 0 {
                return Ok(status);
            }
            p.u32(0x10, 0x18);
            p.u32(0x14, (path.len() * 2 + 2) as u32);
            p.u64(0, u64::MAX);
            ids = 0x18 + path.len() * 2 + 2;
        } else {
            p.u64(0, source as u64);
            ids = 0x18;
        }
        p.u32(0xc, flags);
        p.u32(8, count);
        // SAFETY: the caller's ids (not checked).
        p.put(ids, unsafe { std::slice::from_raw_parts(regions.cast::<u8>(), n * 4) });
        let hr = send(handle_of(adapter), &mut p);
        if hr < 0 || n == 0 {
            return Ok(hr);
        }
        let b = &p.bytes[HEADER..];
        let entries = ids + n * 4;
        // SAFETY: the caller's entries (not checked).
        let out = unsafe { std::slice::from_raw_parts_mut(out, n * 0x30) };
        for i in 0..n {
            let (o, e) = (i * 0x30, entries + i * 0x38);
            out[o + 4..o + 8].copy_from_slice(&(rd32(b, e + 4) & 0x7f).to_le_bytes());
            mv(out, o, b, e, 4);
            mv(out, o + 0x20, b, e + 0x20, 4);
            mv(out, o + 0x10, b, e + 0x10, 8);
            mv(out, o + 8, b, e + 8, 8);
            mv(out, o + 0x18, b, e + 0x18, 8);
        }
        Ok(hr)
    })())
}

/// The chunked loop of 0x180008a60 and 0x180008b50: `per` entries a
/// chunk, until all are done or one fails.
fn region_info_all(out: *mut u8, adapter: *const Adapter, source: *const u16, flags: u32, count: u32, regions: *const u32, per: u32) -> HRESULT {
    let (mut left, mut take, mut done) = (count, per, 0usize);
    let mut hr = 0;
    while left != 0 {
        if hr < 0 {
            return hr;
        }
        // SAFETY: within the caller's arrays of `count`.
        let (o, r) = unsafe { (out.add(done * 0x30), regions.add(done)) };
        hr = region_info_chunk(o, adapter, source, flags, take, r);
        if hr >= 0 {
            left -= take;
            done += per as usize;
            if left < per {
                take = left;
            }
        }
    }
    hr
}

/// 0x180008a60: by instance id.
pub extern "system" fn XCrdStreamingQueryRegionInformation(out: *mut u8, adapter: *const Adapter, id: u64, count: u32, regions: *const u32) -> HRESULT {
    result((|| {
        check(count != 0, "Region Count should not be zero\n")?;
        check(!adapter.is_null(), "Invalid Adapter Handle\n")?;
        check(!out.is_null() && !regions.is_null(), "RegionInfoList or RegionList must not be NULL\n")?;
        let mut per = count;
        while (per.wrapping_mul(0x3c).wrapping_add(0x18)) as usize > MAX_BODY {
            per >>= 1;
        }
        Ok(region_info_all(out, adapter, id as *const u16, 0, count, regions, per))
    })())
}

/// 0x180008b50: by the content's path.
pub extern "system" fn XCrdStreamingQueryRegionInformationByPath(out: *mut u8, adapter: *const Adapter, path: *const u16, count: u32, regions: *const u32) -> HRESULT {
    result((|| {
        check(count != 0, "Region Count should not be zero\n")?;
        check(!adapter.is_null(), "Invalid Adapter Handle\n")?;
        check(!out.is_null() && !regions.is_null(), "RegionInfoList or RegionList must not be NULL\n")?;
        // The path's length counts (the library takes it as unchecked).
        let len = text(path).len() as u32;
        let mut per = count;
        while (len.wrapping_add(per.wrapping_mul(0x1e)).wrapping_mul(2).wrapping_add(0x1a)) as usize > MAX_BODY {
            per >>= 1;
        }
        Ok(region_info_all(out, adapter, path, 1, count, regions, per))
    })())
}

/// 0x18000ed28: one chunk of region specifiers (0x188 bytes each: an id
/// and two strings of 0x80 and 0x100 bytes), from `start`.
fn specifiers_chunk(specs: *mut u8, written: *mut u32, adapter: *const Adapter, source: *const u16, flags: u32, start: u32, count: u32) -> HRESULT {
    result((|| {
        check(!adapter.is_null(), "Adapter Handle should be non zero\n")?;
        check(flags & !1 == 0, "Invalid Query Region Specifier Flags\n")?;
        let by_path = flags & 1 != 0;
        check(!by_path || !source.is_null(), "Must Specify Source Path\n")?;
        check(!written.is_null(), "Must Specify RegionSpecifiersWritten\n")?;
        check(!specs.is_null() || count == 0, "Must Specify Specifiers or have 0 SpecifierCount\n")?;
        let n = count as usize;
        let path = if by_path { text(source) } else { &[] };
        let mut len = n * 0x188 + 0x20;
        if by_path {
            len += path.len() * 2 + 2;
        }
        let mut p = Payload::new(0x7800_00a8, len);
        p.u32(0x18, start);
        p.u32(0x14, count);
        let mut entries = 0;
        if by_path {
            p.u32(0x1c, 1);
            let status = copy_string(p.body(), 0x20, path.len() * 2 + 2, path);
            if status < 0 {
                return Ok(status);
            }
            p.u32(8, 0x20);
            p.u32(0xc, (path.len() * 2 + 2) as u32);
            p.u64(0, u64::MAX);
            if count != 0 {
                entries = 0x20 + path.len() * 2 + 2;
                p.u32(0x10, entries as u32);
            }
        } else {
            p.u64(0, source as u64);
            if count != 0 {
                p.u32(0x10, 0x20);
                entries = 0x20;
            }
        }
        let hr = send(handle_of(adapter), &mut p);
        if hr < 0 {
            return Ok(hr);
        }
        let b = p.bytes[HEADER..].to_vec();
        let got = rd32(&b, 0x14);
        // SAFETY: checked non-null.
        unsafe { *written = got };
        for i in 0..(got.min(count) as usize) {
            // SAFETY: the caller's specifiers, `count` of them.
            let spec = unsafe { std::slice::from_raw_parts_mut(specs.add(i * 0x188), 0x188) };
            let e = entries + i * 0x188;
            spec[4..8].fill(0);
            mv(spec, 0, &b, e, 4);
            let units = |at: usize, n: usize| -> Vec<u16> {
                b[at..(at + n).min(b.len())].chunks(2).map(|c| u16::from_le_bytes([c[0], *c.get(1).unwrap_or(&0)])).collect()
            };
            let status = copy_string(spec, 8, 0x80, &units(e + 8, 0x80));
            if status < 0 {
                return Ok(status);
            }
            let status = copy_string(spec, 0x88, 0x100, &units(e + 0x88, 0x100));
            if status < 0 {
                return Ok(status);
            }
        }
        Ok(hr)
    })())
}

/// The chunked loop of 0x180008c70 and 0x180008dd0.
fn specifiers_all(specs: *mut u8, written: *mut u32, adapter: *const Adapter, source: *const u16, flags: u32, count: u32, per: u32) -> HRESULT {
    // SAFETY: checked non-null by the callers.
    unsafe { *written = 0 };
    if count == 0 {
        return if specs.is_null() { specifiers_chunk(std::ptr::null_mut(), written, adapter, source, flags, 0, 0) } else { 0 };
    }
    let (mut left, mut take, mut start, mut done) = (count, per, 0u32, 0usize);
    let mut hr = 0;
    while left != 0 {
        if hr < 0 {
            return hr;
        }
        let mut got = 0;
        // SAFETY: within the caller's array of `count`.
        hr = specifiers_chunk(unsafe { specs.add(done * 0x188) }, &mut got, adapter, source, flags, start, take);
        if hr >= 0 {
            left -= take;
            start += per;
            // SAFETY: as above.
            unsafe { *written += got };
            done += per as usize;
            if left < per {
                take = left;
            }
        }
    }
    hr
}

fn specifier_checks(specs: *mut u8, written: *mut u32, adapter: *const Adapter, source: *const u16, count: u32, source_message: &str) -> Result<(), HRESULT> {
    check(!written.is_null(), "SpecifiersWritten should not be NULL\n")?;
    check(!adapter.is_null(), "Invalid Adapter Handle\n")?;
    check(!source.is_null(), source_message)?;
    if count == 0 {
        check(specs.is_null(), "Specifiers should be NULL with a zero specifier count\n")
    } else {
        check(!specs.is_null(), "Specifiers should not be NULL\n")
    }
}

/// 0x180008c70: by stream handle.
pub extern "system" fn XCrdStreamingQueryRegionSpecifiers(specs: *mut u8, written: *mut u32, adapter: *const Adapter, stream: *const u16, count: u32) -> HRESULT {
    result((|| {
        specifier_checks(specs, written, adapter, stream, count, "Invalid Stream Handle\n")?;
        let mut per = count;
        if count != 0 {
            while (per.wrapping_mul(0x188).wrapping_add(0x20)) as usize > MAX_BODY {
                per >>= 1;
            }
        }
        Ok(specifiers_all(specs, written, adapter, stream, 0, count, per))
    })())
}

/// 0x180008dd0: by path; the first size check counts the path, the
/// halving that follows does not (as the library's).
pub extern "system" fn XCrdStreamingQueryRegionSpecifiersByPath(specs: *mut u8, written: *mut u32, adapter: *const Adapter, path: *const u16, count: u32) -> HRESULT {
    result((|| {
        specifier_checks(specs, written, adapter, path, count, "Invalid CRD Path\n")?;
        let len = text(path).len() as u32;
        let mut per = count;
        let mut size = len.wrapping_add(count.wrapping_mul(0xc4)).wrapping_mul(2).wrapping_add(0x22);
        while size as usize > MAX_BODY {
            per >>= 1;
            size = per.wrapping_mul(0x188).wrapping_add(0x20);
        }
        Ok(specifiers_all(specs, written, adapter, path, 1, count, per))
    })())
}

/// 0x18000ef94: one chunk of the region queue: up to `count` ids from
/// `start` of the queue of `kind`; more than asked for is
/// ERROR_INSUFFICIENT_BUFFER.
fn queue_chunk(out: *mut u32, adapter: *const Adapter, source: *const u16, flags: u32, start: u32, count: u32, written: *mut u32, kind: u32) -> HRESULT {
    result((|| {
        check(!adapter.is_null(), "Adapter Handle should be non zero\n")?;
        check(flags & !1 == 0, "Invalid Query Region Flags\n")?;
        let by_path = flags & 1 != 0;
        check(!by_path || !source.is_null(), "Must Specify Source Path\n")?;
        let path = if by_path { text(source) } else { &[] };
        let mut len = count as usize * 4 + 0x28;
        if by_path {
            len += path.len() * 2 + 2;
        }
        let mut p = Payload::new(0x7800_0080, len);
        let ids;
        if by_path {
            // A failed copy still sends (the library goes on to the send).
            let status = copy_string(p.body(), 0x28, path.len() * 2 + 2, path);
            if status >= 0 {
                p.u32(0x10, 0x28);
                p.u32(0x14, (path.len() * 2 + 2) as u32);
                p.u64(0, u64::MAX);
            }
            ids = 0x28 + path.len() * 2 + 2;
            if status < 0 {
                return Ok(send(handle_of(adapter), &mut p));
            }
        } else {
            p.u64(0, source as u64);
            p.u64(0x10, 0);
            ids = 0x28;
        }
        p.u32(0xc, flags);
        p.u32(8, count);
        p.u32(0x1c, kind);
        p.u32(0x18, start);
        let hr = send(handle_of(adapter), &mut p);
        if hr < 0 {
            return Ok(hr);
        }
        let b = &p.bytes[HEADER..];
        let got = rd32(b, 8);
        if got > count {
            return Ok(0x8007_007a_u32 as i32);
        }
        for i in 0..got as usize {
            // SAFETY: the caller's array of at least `count`.
            unsafe { *out.add(i) = rd32(b, ids + i * 4) };
        }
        // SAFETY: the caller's (not checked).
        unsafe { *written = got };
        Ok(hr)
    })())
}

fn queue_all(out: *mut u32, written: *mut u32, adapter: *const Adapter, source: *const u16, flags: u32, count: u32, kind: u32) -> HRESULT {
    let mut per = count;
    while (per.wrapping_mul(4).wrapping_add(0x28)) as usize > MAX_BODY {
        per >>= 1;
    }
    let (mut left, mut take, mut start, mut done, mut total) = (count, per, 0u32, 0usize, 0u32);
    let mut hr = 0;
    while left != 0 {
        if hr < 0 {
            return hr;
        }
        let mut got = 0;
        // SAFETY: within the caller's array of `count`.
        hr = queue_chunk(unsafe { out.add(done) }, adapter, source, flags, start, take, &mut got, kind);
        if hr >= 0 {
            if got == 0 {
                break;
            }
            total += got;
            left -= take;
            start += per;
            if left < per {
                take = left;
            }
            done += per as usize;
        }
    }
    if hr >= 0 {
        // SAFETY: the caller's (not checked).
        unsafe { *written = total };
    }
    hr
}

/// 0x180009060: by queue id; queue kinds 0-3.
pub extern "system" fn XCrdStreamingQueueQueryRegionList(out: *mut u32, written: *mut u32, adapter: *const Adapter, id: u64, count: u32, kind: u32) -> HRESULT {
    result((|| {
        check(count != 0, "Region Count should not be zero\n")?;
        check(!adapter.is_null(), "Invalid Adapter Handle\n")?;
        check(!out.is_null(), " RegionList must not be NULL\n")?;
        check(kind <= 3, " Invalid Queue Type Value\n")?;
        Ok(queue_all(out, written, adapter, id as *const u16, 0, count, kind))
    })())
}

/// 0x1800091a0: by path.
pub extern "system" fn XCrdStreamingQueueQueryRegionListByPath(out: *mut u32, written: *mut u32, adapter: *const Adapter, path: *const u16, count: u32, kind: u32) -> HRESULT {
    result((|| {
        check(count != 0, "Region Count should not be zero\n")?;
        check(!adapter.is_null(), "Invalid Adapter Handle\n")?;
        check(!path.is_null(), "Invalid XCrdPath\n")?;
        check(!out.is_null(), " RegionList must not be NULL\n")?;
        check(kind <= 3, " Invalid Queue Type Value\n")?;
        Ok(queue_all(out, written, adapter, path, 1, count, kind))
    })())
}

#[allow(dead_code)]
const _UNUSED: HRESULT = E_INVALIDARG;

pub(crate) fn exports() -> Vec<crate::HleExport> {
    use crate::hle_export;
    vec![
        hle_export!(XCrdStreamingQueryRegionIdByOffset),
        hle_export!(XCrdStreamingQueryRegionInformation),
        hle_export!(XCrdStreamingQueryRegionInformationByPath),
        hle_export!(XCrdStreamingQueryRegionSpecifiers),
        hle_export!(XCrdStreamingQueryRegionSpecifiersByPath),
        hle_export!(XCrdStreamingQueueQueryRegionList),
        hle_export!(XCrdStreamingQueueQueryRegionListByPath),
    ]
}
