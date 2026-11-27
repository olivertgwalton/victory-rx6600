//! The exports that send one fixed-size body: their checks, what goes in
//! the body, and what comes back out. Addresses are `XCRDAPI.dll`'s.

use super::srb::{Payload, check, send};
use super::{Adapter, HRESULT, handle_of};

/// Builds a body of `len` bytes for `code`, fills it, sends it on the
/// adapter, and on success hands it to `done`.
fn exchange(adapter: *const Adapter, code: u32, len: usize, fill: impl FnOnce(&mut Payload), done: impl FnOnce(&Payload)) -> HRESULT {
    let mut payload = Payload::new(code, len);
    fill(&mut payload);
    let hr = send(handle_of(adapter), &mut payload);
    if hr >= 0 {
        done(&payload);
    }
    hr
}

fn body(p: &Payload) -> &[u8] {
    &p.bytes[super::srb::HEADER..]
}

fn result(r: Result<HRESULT, HRESULT>) -> HRESULT {
    r.unwrap_or_else(|hr| hr)
}

/// 0x180003d00: takes or drops an optical-drive power reference; the
/// count comes back at +0 whether or not the driver succeeded.
pub extern "system" fn XCrdOddPmAcquireReleaseReference(count: *mut u32, adapter: *const Adapter, acquire: u8) -> HRESULT {
    result((|| {
        check(!adapter.is_null(), "Invalid Adapter Handle\n")?;
        let mut payload = Payload::new(0x7800_0098, 8);
        payload.put(4, &[acquire]);
        let hr = send(handle_of(adapter), &mut payload);
        if !count.is_null() {
            // SAFETY: checked non-null.
            unsafe { *count = payload.read_u32(0) };
        }
        Ok(hr)
    })())
}

/// 0x180003a00: whether a sideloaded key (a 16-byte id) is present.
pub extern "system" fn XCrdIsSideloadedKeyPresent(adapter: *const Adapter, key: *const [u8; 16], present: *mut u8) -> HRESULT {
    result((|| {
        check(!adapter.is_null(), "Invalid adapter handle.\n")?;
        check(!present.is_null(), "Invalid IsPresent.\n")?;
        // SAFETY: the caller's key (not checked, as the console's).
        let key = unsafe { *key };
        Ok(exchange(adapter, 0x7800_0180, 0x14, |p| p.put(0, &key), |p| {
            // SAFETY: checked non-null.
            unsafe { *present = body(p)[0x10] };
        }))
    })())
}

/// 0x180004440: the attestation blob for a 16-byte id, 0x80 bytes; on
/// success +0 becomes 1 and +0x14..+0x80 the driver's.
pub extern "system" fn XCrdQueryAttestationBlob(adapter: *const Adapter, id: *const [u8; 16], blob: *mut [u8; 0x80]) -> HRESULT {
    result((|| {
        check(!adapter.is_null(), "Invalid Adapter Handle\n")?;
        // SAFETY: the caller's id (not checked).
        let id = unsafe { *id };
        Ok(exchange(adapter, 0x7800_009c, 0x80, |p| {
            p.put(0, &[1]);
            p.put(4, &id);
        }, |p| {
            // SAFETY: the caller's blob (not checked).
            let blob = unsafe { &mut *blob };
            blob[0] = 1;
            blob[0x14..].copy_from_slice(&body(p)[0x14..0x80]);
        }))
    })())
}

/// 0x180004f00: write statistics; only type 0 is filled in (0x20 bytes
/// from +8); types 1 and up are refused, negative ones sent and not read.
pub extern "system" fn XCrdQueryWriteStats(adapter: *const Adapter, kind: i32, stats: *mut [u8; 0x20]) -> HRESULT {
    result((|| {
        check(!adapter.is_null(), "Invalid Adapter Handle\n")?;
        check(!stats.is_null(), "Invalid Write Stats Information\n")?;
        check(kind < 1, "Invalid Write Stats Type\n")?;
        Ok(exchange(adapter, 0x7800_0168, 0x28, |_| {}, |p| {
            if kind == 0 {
                // SAFETY: checked non-null.
                unsafe { (*stats).copy_from_slice(&body(p)[8..0x28]) };
            }
        }))
    })())
}

/// 0x180005eb0.
pub extern "system" fn XCrdRemapReload(adapter: *const Adapter) -> HRESULT {
    result((|| {
        check(!adapter.is_null(), "Invalid Adapter Handle.\n")?;
        Ok(exchange(adapter, 0x7800_00f0, 0x20, |_| {}, |_| {}))
    })())
}

/// 0x180006bd0: two bytes.
pub extern "system" fn XCrdSetTelemetry(adapter: *const Adapter, a: u8, b: u8) -> HRESULT {
    result((|| {
        check(!adapter.is_null(), "Invalid Adapter Handle\n")?;
        Ok(exchange(adapter, 0x7800_01c8, 2, |p| p.put(0, &[a, b]), |_| {}))
    })())
}

/// 0x180006c90: bit 0 only.
pub extern "system" fn XCrdSetTitleQuirks(adapter: *const Adapter, flags: u32) -> HRESULT {
    result((|| {
        check(!adapter.is_null(), "Invalid Adapter Handle\n")?;
        check(flags & !1 == 0, "Invalid Flags\n")?;
        Ok(exchange(adapter, 0x7800_01b4, 4, |p| p.u32(0, flags & 1), |_| {}))
    })())
}

/// 0x1800083f0: the active streaming instance id (+0 of a 0x60 body).
pub extern "system" fn XCrdStreamingQueryActiveInstanceId(adapter: *const Adapter, id: *mut u64) -> HRESULT {
    result((|| {
        check(!adapter.is_null(), "Invalid Adapter Handle\n")?;
        Ok(exchange(adapter, 0x7800_003c, 0x60, |_| {}, |p| {
            // SAFETY: the caller's (not checked).
            unsafe { *id = u64::from_le_bytes(body(p)[..8].try_into().unwrap()) };
        }))
    })())
}

/// 0x180009890: stops a streaming instance; the reserved argument must be 0.
pub extern "system" fn XCrdStreamingStop(adapter: *const Adapter, id: u64, reserved: u64) -> HRESULT {
    result((|| {
        check(!adapter.is_null(), "Invalid Adapter Handle\n")?;
        check(reserved == 0, "Reserved field must be zero\n")?;
        Ok(exchange(adapter, 0x7800_0034, 0x58, |p| p.u64(0, id), |_| {}))
    })())
}

/// 0x180009960: one byte (suspend or resume).
pub extern "system" fn XCrdSuspendResumeIoBalancer(adapter: *const Adapter, suspend: u8) -> HRESULT {
    result((|| {
        check(!adapter.is_null(), "Invalid Adapter Handle.\n")?;
        Ok(exchange(adapter, 0x7800_00a4, 1, |p| p.put(0, &[suspend]), |_| {}))
    })())
}

/// 0x18000ba60: an XPF operation 0-2; both reserved arguments must be 0
/// (checked before the operation).
pub extern "system" fn XCrdXpfOp(adapter: *const Adapter, op: u32, reserved: usize, reserved2: usize) -> HRESULT {
    result((|| {
        check(!adapter.is_null(), "Invalid Adapter Handle\n")?;
        check(reserved == 0 && reserved2 == 0, "Reserved values must be zero\n")?;
        check(op <= 2, "Invalid OP value\n")?;
        Ok(exchange(adapter, 0x7800_00b4, 0x88, |p| p.u32(0, op), |_| {}))
    })())
}

/// 0x180002d30: derives an MSIXVC2 key: up to 0x20 bytes of input, a
/// value, up to 0x40 bytes of context, up to 0x20 bytes out (from +0x70).
/// Bad arguments fail with no message.
pub extern "system" fn XCrdDeriveMsixvc2Key(
    adapter: *const Adapter,
    input: *const u8,
    input_len: u32,
    value: u32,
    context: *const u8,
    context_len: u32,
    out: *mut u8,
    out_len: u32,
) -> HRESULT {
    if adapter.is_null() || input.is_null() || input_len > 0x20 || out.is_null() || out_len > 0x20 || context.is_null() || context_len > 0x40 {
        return super::E_INVALIDARG;
    }
    // SAFETY: the caller's buffers of the lengths given (checked above).
    let (input, context) = unsafe {
        (std::slice::from_raw_parts(input, input_len as usize), std::slice::from_raw_parts(context, context_len as usize))
    };
    exchange(adapter, 0x7800_01dc, 0x90, |p| {
        p.u32(0, input_len);
        p.put(4, input);
        p.u32(0x24, value);
        p.u32(0x28, context_len);
        p.put(0x2c, context);
        p.u32(0x6c, out_len);
    }, |p| {
        // SAFETY: the caller's buffer of `out_len` bytes.
        unsafe { std::ptr::copy_nonoverlapping(body(p)[0x70..].as_ptr(), out, out_len as usize) };
    })
}

/// 0x180002fe0: encrypts whole pages in place with a 32-byte key and a
/// 16-byte IV, at a page-aligned offset. Bad arguments fail with no
/// message.
pub extern "system" fn XCrdEncryptBuffer(adapter: *const Adapter, buffer: *mut u8, len: u32, offset: u64, key: *const [u8; 32], iv: *const [u8; 16]) -> HRESULT {
    if adapter.is_null() || buffer.is_null() || len == 0 || len & 0xfff != 0 || offset & 0xfff != 0 || key.is_null() || iv.is_null() || len > 0xffff_ffbf {
        return super::E_INVALIDARG;
    }
    // SAFETY: the caller's buffer of `len` bytes, key and IV.
    let (data, key, iv) = unsafe { (std::slice::from_raw_parts(buffer, len as usize), *key, *iv) };
    exchange(adapter, 0x7800_01e0, len as usize + 0x40, |p| {
        p.u64(0, offset);
        p.u32(8, len >> 12);
        p.put(0xc, &key);
        p.put(0x2c, &iv);
        p.u32(0x3c, 0x40);
        p.put(0x40, data);
    }, |p| {
        // The data is read back from the offset the driver left at +0x3c
        // (as the console's; an offset past the body is the driver's error,
        // where the console's would read past its block: nothing copied).
        let at = p.read_u32(0x3c) as usize;
        if let Some(data) = body(p).get(at..at + len as usize) {
            // SAFETY: as above.
            unsafe { std::ptr::copy_nonoverlapping(data.as_ptr(), buffer, len as usize) };
        }
    })
}

/// 0x180008f50: queues regions (32-bit ids) for a streaming instance.
pub extern "system" fn XCrdStreamingQueueInsertRegion(adapter: *const Adapter, id: u64, count: u32, regions: *const u32, flags: u32) -> HRESULT {
    result((|| {
        check(!adapter.is_null(), "Adapter Handle should be non zero\n")?;
        check(count != 0, "Region count can't be zero\n")?;
        check(flags == 0, "Flags should be zero for now\n")?;
        // SAFETY: the caller's regions (not checked).
        let regions = unsafe { std::slice::from_raw_parts(regions.cast::<u8>(), count as usize * 4) };
        Ok(exchange(adapter, 0x7800_0044, (count.wrapping_mul(4).wrapping_add(0x38)) as usize, |p| {
            p.u64(0, id);
            p.u32(0x10, count);
            p.u32(0x14, 0x38);
            p.put(0x38, regions);
        }, |_| {}))
    })())
}

pub(crate) fn exports() -> Vec<crate::HleExport> {
    use crate::hle_export;
    vec![
        hle_export!(XCrdOddPmAcquireReleaseReference),
        hle_export!(XCrdIsSideloadedKeyPresent),
        hle_export!(XCrdQueryAttestationBlob),
        hle_export!(XCrdQueryWriteStats),
        hle_export!(XCrdRemapReload),
        hle_export!(XCrdSetTelemetry),
        hle_export!(XCrdSetTitleQuirks),
        hle_export!(XCrdStreamingQueryActiveInstanceId),
        hle_export!(XCrdStreamingStop),
        hle_export!(XCrdSuspendResumeIoBalancer),
        hle_export!(XCrdXpfOp),
        hle_export!(XCrdDeriveMsixvc2Key),
        hle_export!(XCrdEncryptBuffer),
        hle_export!(XCrdStreamingQueueInsertRegion),
    ]
}
