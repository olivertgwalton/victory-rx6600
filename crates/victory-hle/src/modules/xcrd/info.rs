//! Information about XVDs, XRsc entries and CRDs, by class: each class has
//! a minimum size, a class number of the driver's, and a layout of the
//! driver's answer in the caller's structure. The layouts were read from
//! the library and checked against it with answers whose bytes name their
//! own offsets (tests `learn_*`). Addresses are `XCRDAPI.dll`'s.

use super::srb::{HEADER, Payload, check, copy_string, send};
use super::{Adapter, E_INVALIDARG, HRESULT, handle_of, wide};

fn result(r: Result<HRESULT, HRESULT>) -> HRESULT {
    r.unwrap_or_else(|hr| hr)
}

fn text<'a>(path: *const u16) -> &'a [u16] {
    // SAFETY: a NUL-terminated path.
    unsafe { wide(path) }
}

/// Copies runs `(to, from, len)` of the answer into the caller's buffer.
fn place(out: *mut u8, body: &[u8], runs: &[(usize, usize, usize)]) {
    for &(to, from, n) in runs {
        // SAFETY: within the caller's structure of the class's size.
        unsafe { std::ptr::copy_nonoverlapping(body[from..from + n].as_ptr(), out.add(to), n) };
    }
}

/// `XCrdQueryXvdInformation`'s classes: minimum size, the driver's class,
/// the answer's layout (classes 1 and 0x13 are variable, see below).
fn xvd_class(class: i32) -> Option<(u32, u32, &'static [(usize, usize, usize)])> {
    const HEAD: &[(usize, usize, usize)] = &[(0, 0x10, 3), (4, 0x14, 0x7c)];
    Some(match class {
        0 => (0x80, 0, HEAD),
        1 => (0x402, 1, &[]),
        2 => (8, 3, &[(0, 0x10, 8)]),
        3 => (0x80, 5, HEAD),
        4 => (8, 6, &[(0, 0x10, 8)]),
        5 => (0x18, 7, &[(0, 0x10, 0x18)]),
        6 => (1, 9, &[(0, 0x10, 1)]),
        7 => (0x168, 0xa, &[(0, 0x10, 0x48), (0x4c, 0x5c, 0x24)]),
        8 => (0x158, 0xb, &[(0, 0x10, 0x64)]),
        9 => (1, 0xc, &[(0, 0x10, 1)]),
        0xa => (0, 0, &[]),
        0xb => (0x150, 0xe, &[(0, 0x10, 0x3c), (0x40, 0x50, 0x14), (0x58, 0x68, 0x5c)]),
        0xc => (1, 0xf, &[(0, 0x10, 1)]),
        0xd => (0x1f8, 0x12, &[(0, 0x10, 0x1f8)]),
        0xe => (0x1f8, 0x13, &[(0, 0x10, 0x1f8)]),
        0xf => (0x1f8, 0x14, &[(0, 0x10, 0x1f8)]),
        0x10 => (0x1f8, 0x15, &[(0, 0x10, 0x1f8)]),
        0x11 => (1, 0x16, &[(0, 0x10, 1)]),
        0x12 => (8, 0x17, &[(0, 0x10, 8)]),
        0x13 => (0x20a, 0x1a, &[]),
        0x14 => (0x40, 0x1b, &[(0, 0x10, 0x18)]),
        c if c < 0 => (0, 0, &[]),
        _ => return None,
    })
}

/// 0x180005250.
pub extern "system" fn XCrdQueryXvdInformation(adapter: *const Adapter, path: *const u16, class: i32, out: *mut u8, size: u32) -> HRESULT {
    result((|| {
        check(!adapter.is_null(), "Invalid Adapter Handle\n")?;
        check(!path.is_null(), "Path must be specified\n")?;
        check(!out.is_null(), "XvdInfo must be supplied\n")?;
        let Some((min, driver, runs)) = xvd_class(class) else {
            return Err(fail("Invalid InfoClass supplied\n"));
        };
        check(size >= min, "Info Buffer too small\n")?;
        let path = text(path);
        let mut p = Payload::new(0x7800_0050, path.len() * 2 + 0x41a);
        p.u32(0, 0x418);
        p.u32(4, (path.len() * 2) as u32);
        let status = copy_string(p.body(), 0x418, path.len() * 2 + 2, path);
        p.u32(0xc, driver);
        if status < 0 {
            return Ok(status);
        }
        let hr = send(handle_of(adapter), &mut p);
        if hr < 0 {
            return Ok(hr);
        }
        let body = &p.bytes[HEADER..];
        match class {
            1 => {
                // A count, then that many 16-byte entries.
                let n = usize::from(u16::from_le_bytes([body[0x10], body[0x11]]));
                let k = (n * 16).min(body.len() - 0x12);
                place(out, body, &[(0, 0x10, 2), (2, 0x12, k)]);
            }
            0x13 => {
                // A byte, then a string and its NUL.
                let chars = body[0x12..].chunks(2).take_while(|c| c != &[0, 0]).count();
                place(out, body, &[(0, 0x10, 1), (2, 0x12, chars * 2 + 2)]);
            }
            _ => place(out, body, runs),
        }
        Ok(hr)
    })())
}

fn fail(message: &str) -> HRESULT {
    super::srb::log(1, message);
    E_INVALIDARG
}

/// 0x180006f70: classes 3, 4 and 9 take no structure.
pub extern "system" fn XCrdSetXvdInformation(adapter: *const Adapter, path: *const u16, class: i32, info: *const u8, size: u32) -> HRESULT {
    result((|| {
        check(!adapter.is_null(), "Invalid Adapter Handle\n")?;
        check(!path.is_null(), "Path must be specified\n")?;
        if info.is_null() {
            check(matches!(class, 3 | 4 | 9), "XvdInfo must be supplied\n")?;
        } else {
            check(class < 0xc, "Invalid InfoClass supplied\n")?;
        }
        let min = match class {
            0 | 0xa | 0xb => 8,
            1 | 2 => 0x10,
            5..=7 => 4,
            8 => 0x54,
            3 | 4 | 9 => 0,
            _ => return Err(E_INVALIDARG),
        };
        if !(info.is_null() && matches!(class, 3 | 4 | 9)) {
            check(size >= min, "Info buffer too small\n")?;
        }
        let path = text(path);
        let extra = if class == 8 { 0x50 } else { 0 };
        let mut p = Payload::new(0x7800_0054, extra + path.len() * 2 + 0x112);
        p.u32(0, (extra + 0x110) as u32);
        p.u32(4, (path.len() * 2) as u32);
        let mut status = copy_string(p.body(), extra + 0x110, path.len() * 2 + 2, path);
        // SAFETY (the reads below): the caller's structure of `size` bytes.
        let at = |n: usize| unsafe { std::slice::from_raw_parts(info, n) }.to_vec();
        let driver = match class {
            0 => 0,
            1 => 1,
            2 => 2,
            3 => 3,
            4 => 4,
            5 => 5,
            6 => 6,
            7 => 7,
            8 => 8,
            9 => 9,
            0xa => 0xa,
            _ => 0x11,
        };
        p.u32(8, driver);
        match class {
            0 | 0xa | 0xb => p.put(0x10, &at(8)),
            1 | 2 => p.put(0x10, &at(16)),
            5..=7 => p.put(0x10, &at(4)),
            4 => {
                if info.is_null() {
                    p.put(0x10, &[0, 0]);
                } else {
                    let s: Vec<u16> = at(0x100).chunks(2).map(|c| u16::from_le_bytes([c[0], c[1]])).collect();
                    status = copy_string(p.body(), 0x10, 0x100, &s);
                }
            }
            8 => {
                // A word and a byte, then the extra room's size (0x50) and
                // a name in that room and two bytes more.
                let head = at(3);
                p.put(0x10, &head);
                p.put(0x13, &[0x50]);
                // SAFETY: as above.
                let name: Vec<u16> = (0..0x29).map(|i| unsafe { info.add(4 + 2 * i).cast::<u16>().read_unaligned() }).collect();
                status = copy_string(p.body(), 0x14, 0x52, &name);
            }
            _ => {}
        }
        if status < 0 {
            return Ok(status);
        }
        Ok(send(handle_of(adapter), &mut p))
    })())
}

/// The content the cloud feature bits belong to.
const ANY_XVC: &str = "[XUC:]\\ANYXVC";

/// 0x180001c80: the cloud feature bits (XVD class 0x12, 8 bytes).
pub extern "system" fn XCloudQueryFeatureBits(adapter: *const Adapter, bits: *mut u64) -> HRESULT {
    let path: Vec<u16> = ANY_XVC.encode_utf16().chain([0]).collect();
    XCrdQueryXvdInformation(adapter, path.as_ptr(), 0x12, bits.cast(), 8)
}

/// 0x180001cb0: sets them (XVD class 0xa).
pub extern "system" fn XCloudSetFeatureBits(adapter: *const Adapter, bits: *const u64) -> HRESULT {
    let path: Vec<u16> = ANY_XVC.encode_utf16().chain([0]).collect();
    // SAFETY: the caller's bits (read first, as the library's).
    let value = unsafe { *bits };
    XCrdSetXvdInformation(adapter, path.as_ptr(), 0xa, (&raw const value).cast(), 8)
}

pub(crate) fn exports() -> Vec<crate::HleExport> {
    use crate::hle_export;
    vec![
        hle_export!(XCrdQueryXvdInformation),
        hle_export!(XCrdSetXvdInformation),
        hle_export!(XCloudQueryFeatureBits),
        hle_export!(XCloudSetFeatureBits),
    ]
}
