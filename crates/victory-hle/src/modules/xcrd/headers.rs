//! XVD headers and UAB data, read locally (no driver). Addresses are
//! `XCRDAPI.dll`'s.

use super::srb::log;
use super::{E_INVALIDARG, HRESULT, NOT_SUPPORTED};

/// "msft-xvd", an XVD header's first 8 bytes.
const XVD_MAGIC: u64 = 0x6476_782d_7466_736d;
/// "MS-XUAB ", at +0x200 of UAB data.
const UAB_MAGIC: u64 = 0x2042_4155_582d_534d;
/// `HRESULT_FROM_WIN32(ERROR_FILE_CORRUPT)`.
const CORRUPT: HRESULT = 0x8007_0570_u32 as i32;

fn u32_at(p: *const u8, at: usize) -> u32 {
    // SAFETY: the caller's header (the library reads these fields unchecked).
    unsafe { p.add(at).cast::<u32>().read_unaligned() }
}
fn u64_at(p: *const u8, at: usize) -> u64 {
    // SAFETY: as above.
    unsafe { p.add(at).cast::<u64>().read_unaligned() }
}
fn u8_at(p: *const u8, at: usize) -> u8 {
    // SAFETY: as above.
    unsafe { *p.add(at) }
}

fn fail(message: &str, hr: HRESULT) -> HRESULT {
    log(1, message);
    hr
}

/// 0x180003330: the console mode of a streaming-roamable XVD (flag 0x200),
/// from the byte at +0x276: 0x40, 0x60, 0x64 or 0x70.
pub extern "system" fn XCrdGetStreamingRoamableXvdConsoleMode(header: *const u8, mode: *mut u16) -> HRESULT {
    if header.is_null() {
        return fail("Invalid Header.\n", E_INVALIDARG);
    }
    if mode.is_null() {
        return fail("Invalid Pointer.\n", E_INVALIDARG);
    }
    if u64_at(header, 0) != XVD_MAGIC {
        return fail("Unknown header.\n", CORRUPT);
    }
    if u32_at(header, 8) & 0x200 == 0 {
        return fail("Only supported for Streaming Roamable Xvd.\n", NOT_SUPPORTED);
    }
    let value = match u8_at(header, 0x276) {
        0 => 0x40,
        1 => 0x60,
        2 => 0x64,
        3 => 0x70,
        _ => return fail("Unsupported console mode.\n", CORRUPT),
    };
    // SAFETY: checked non-null.
    unsafe { *mode = value };
    0
}

/// Bitmap lengths of UAB data (bytes): metadata first, then data, each a
/// bit per 0xaa000 bytes, rounded up.
fn uab_lengths(uab: *const u8) -> (u64, u64) {
    let (total, meta) = (u64_at(uab, 0x2a0), u64_at(uab, 0x2c0));
    let data = total.wrapping_sub(meta).wrapping_add(0xa_9fff) / 0xa_a000;
    let md = meta.wrapping_add(0xa_9fff) / 0xa_a000;
    ((md + 7) >> 3, (data + 7) >> 3)
}

/// 0x1800033f0: where the UAB data's bitmaps are and how long; `alternate`
/// picks the copy at +0x210 rather than +0x208.
pub extern "system" fn XCrdGetUabBitmapOffset(
    uab: *const u8,
    size: u64,
    alternate: u8,
    md_bitmap: *mut *const u8,
    md_len: *mut u32,
    data_bitmap: *mut *const u8,
    data_len: *mut u32,
) -> HRESULT {
    for (bad, message) in [
        (uab.is_null(), "Invalid UabData.\n"),
        (md_bitmap.is_null(), "Invalid MdBitmap.\n"),
        (md_len.is_null(), "Invalid MdBitmapLength.\n"),
        (data_bitmap.is_null(), "Invalid DataBitmap.\n"),
        (data_len.is_null(), "Invalid DataBitmapLength.\n"),
    ] {
        if bad {
            return fail(message, E_INVALIDARG);
        }
    }
    if size < 0x1000 {
        return CORRUPT;
    }
    let (md, data) = uab_lengths(uab);
    let base = u64_at(uab, if alternate != 0 { 0x210 } else { 0x208 }) as usize;
    // SAFETY: checked non-null; pointers into the caller's data, as the
    // library computes them.
    unsafe {
        *md_len = md as u32;
        *data_len = data as u32;
        *md_bitmap = uab.wrapping_add(base);
        *data_bitmap = uab.wrapping_add((md as u32 as usize).wrapping_add(base));
    }
    0
}

/// 0x18000a970: checks UAB data against an XVD's basic information.
/// `strict` requires the data to be exactly the doubled layout.
pub extern "system" fn XCrdValidateUab(uab: *const u8, size: u64, strict: u8, info: *const u8) -> HRESULT {
    if uab.is_null() {
        return fail("Invalid UabData.\n", E_INVALIDARG);
    }
    if info.is_null() {
        return fail("Invalid XvdBasicInfo.\n", E_INVALIDARG);
    }
    if size < 0x1000 {
        return E_INVALIDARG;
    }
    if u64_at(uab, 0x200) != UAB_MAGIC || u64::from(u32_at(uab, 0x284)) > size {
        return CORRUPT;
    }
    if u64_at(info, 0x78) != u64_at(uab, 0x2a0) {
        return E_INVALIDARG;
    }
    // SAFETY: the caller's data and information (unchecked sizes).
    let same = |a: usize, b: usize| unsafe { std::slice::from_raw_parts(uab.add(a), 16) == std::slice::from_raw_parts(info.add(b), 16) };
    if !same(0x220, 0x30) || !same(0x230, 0x30) || !same(0x240, 0x40) {
        return E_INVALIDARG;
    }
    if u64_at(uab, 0x208) != 0x1000 {
        return CORRUPT;
    }
    let (md, data) = uab_lengths(uab);
    let mut pages = (data.wrapping_add(0xfff).wrapping_add(md) as u32) & 0xffff_f000;
    let doubled = (((md.wrapping_add(0xfff).wrapping_add(data)) & !0xfff).wrapping_mul(2).wrapping_add(0x1000)) as u32 as u64;
    if strict != 0 && size != doubled {
        return CORRUPT;
    }
    if u32_at(uab, 0x218) != pages {
        return CORRUPT;
    }
    if size == doubled {
        let second = u32_at(uab, 0x21c);
        let at = u64_at(uab, 0x210);
        if second == 0 || at == 0 || at != u64::from(u32_at(uab, 0x208).wrapping_add(pages)) || second != pages {
            return CORRUPT;
        }
        pages = pages.wrapping_add(pages.wrapping_add(0x1000));
    } else {
        pages = pages.wrapping_add(0x1000);
    }
    if u32_at(uab, 0x284) == pages { 0 } else { CORRUPT }
}

/// 0x1800102f4: blocks needed at a level of a 170-way tree for `x`.
fn level(x: u32, level: u32) -> u32 {
    match level {
        0 => x.wrapping_add(0xa9) / 0xaa,
        1 => x.wrapping_add(0x70e3) / 0x70e4,
        2 => x.wrapping_add(0x4a_f767) / 0x4a_f768,
        3 => x.wrapping_add(0x31c8_4b0f) / 0x31c8_4b10,
        _ => 0,
    }
}

/// XVC types (`+0x84`) this applies to.
const XVC_TYPES: u64 = 0x100_0701_8042;

/// 0x180003540: the byte offset of an XVC's data: header, embedded and
/// user data pages, the hash tree unless the XVD is unhashed (flag 4).
pub extern "system" fn XCrdGetXvcOffsetFromHeader(header: *const u8, offset: *mut u64) -> HRESULT {
    if header.is_null() {
        return fail("Invalid Header.\n", E_INVALIDARG);
    }
    if offset.is_null() {
        return fail("Invalid Offset.\n", E_INVALIDARG);
    }
    if u64_at(header, 0) != XVD_MAGIC {
        return fail("Unknown header.\n", E_INVALIDARG);
    }
    let kind = u32_at(header, 0x84);
    if kind > 0x28 || XVC_TYPES & (1 << kind) == 0 {
        return fail("Not a Xvc.\n", E_INVALIDARG);
    }
    let pages = |v: u64| v.wrapping_add(0xfff) >> 12;
    let a = pages(u64::from(u32_at(header, 0x8c)));
    let b = pages(u64_at(header, 0x18));
    let c = pages(u64::from(u32_at(header, 0x90)));
    let d = pages(u64::from(u32_at(header, 0x88)));
    let e = u64::from(u8_at(header, 0x270));
    let data = (c as u32).wrapping_add((b as u32).wrapping_add(a as u32));
    let extra = if u32_at(header, 0x80) == 1 { pages(u64::from(u32_at(header, 0x94))) as u32 } else { 0 };
    let mut tree = 0u32;
    if u32_at(header, 8) & 4 == 0 {
        let blocks = extra.wrapping_add(data);
        tree = level(blocks, 0);
        if blocks.wrapping_add(0xa9) >= 0x154 {
            let mut l = 1;
            loop {
                let n = level(blocks, l);
                tree = tree.wrapping_add(n);
                l += 1;
                if n <= 1 {
                    break;
                }
            }
        }
    }
    let total = a.wrapping_add(d).wrapping_add(e + 3).wrapping_add(u64::from(tree));
    // SAFETY: checked non-null.
    unsafe { *offset = total << 12 };
    0
}

pub(crate) fn exports() -> Vec<crate::HleExport> {
    use crate::hle_export;
    vec![
        hle_export!(XCrdGetStreamingRoamableXvdConsoleMode),
        hle_export!(XCrdGetUabBitmapOffset),
        hle_export!(XCrdValidateUab),
        hle_export!(XCrdGetXvcOffsetFromHeader),
    ]
}
