//! XAS paths: content in Azure blob storage, named as
//! `[XAS:]<ip>/<account>/<container>/<name><sas>,SN=<host>` with options.
//! Addresses are `XCRDAPI.dll`'s.

use super::srb::log;
use super::{Adapter, E_INVALIDARG, E_OUTOFMEMORY, HRESULT, heap_alloc, units, wide};

fn s(p: *const u16) -> Vec<u16> {
    // SAFETY: a NUL-terminated string.
    unsafe { wide(p) }.to_vec()
}

/// `wcscat_s` into a buffer of `cap` characters: appends when it fits with
/// its NUL; otherwise the CRT empties the buffer.
fn cat(buf: &mut Vec<u16>, cap: usize, more: &[u16]) {
    if buf.len() + more.len() < cap {
        buf.extend_from_slice(more);
    } else {
        buf.clear();
    }
}

/// Lone `%` signs (not followed by another) in a SAS token.
fn lone_percents(t: &[u16]) -> usize {
    (0..t.len()).filter(|&i| t[i] == u16::from(b'%') && t.get(i + 1) != Some(&u16::from(b'%'))).count()
}

/// 0x18000b124. Flag 2 makes a placeholder path (account, container, SAS
/// and address replaced, only the VCSA name needed); flags 1 and 4 add
/// ",PRST=1" and ",NRM=1". Kind 0 leaves "%ws" for the name and doubles the
/// SAS token's lone `%` signs, so the path can be formatted later; kind 1
/// takes the name as given.
#[allow(clippy::too_many_arguments)]
fn build(
    out: *mut *mut u16,
    adapter: *const Adapter,
    ip: *const u16,
    host: *const u16,
    account: *const u16,
    container: *const u16,
    name: *const u16,
    sas: *const u16,
    kind: u32,
    flags: u64,
    vcsa: *const u16,
    port: u16,
) -> HRESULT {
    if adapter.is_null() {
        log(1, "Invalid Adapter Handle.\n");
        return E_INVALIDARG;
    }
    if out.is_null() || flags & !7 != 0 {
        return E_INVALIDARG;
    }
    let (ip, account, container, sas) = if flags & 2 == 0 {
        if ip.is_null() || account.is_null() || container.is_null() || sas.is_null() {
            return E_INVALIDARG;
        }
        (s(ip), s(account), s(container), s(sas))
    } else {
        if vcsa.is_null() {
            return E_INVALIDARG;
        }
        (units("0.0.0.0"), units("invalidACCT"), units("invalidCONT"), units("?sig=invalid"))
    };
    let name = match kind {
        1 if !name.is_null() => s(name),
        0 => units("%ws"),
        _ => return E_INVALIDARG,
    };
    let host = if host.is_null() { units("blob.core.windows.net") } else { s(host) };
    let mut size = ((host.len() + container.len() + account.len() + ip.len() + sas.len() + name.len()) * 2 + 0x1e) as u32;
    if flags & 1 != 0 {
        size += 0x10;
    }
    if flags & 4 != 0 {
        size += 0xe;
    }
    let vcsa = (!vcsa.is_null()).then(|| s(vcsa));
    if let Some(v) = &vcsa {
        size += (v.len() * 2 + 0xc) as u32;
    }
    let port_text = units(&port.to_string());
    if vcsa.is_some() && port != 0 {
        size += 0x18;
    }
    let extra = if kind == 0 { lone_percents(&sas) } else { 0 };
    let bytes = (size + (extra as u32 + 1) * 2) as usize;
    let buffer = heap_alloc(bytes).cast::<u16>();
    if buffer.is_null() {
        return E_OUTOFMEMORY;
    }
    let cap = bytes / 2;
    // StringCchPrintf("%ws%ws/%ws/%ws/", "[XAS:]", ip, account, container).
    let mut text = units("[XAS:]");
    for (part, slash) in [(&ip, true), (&account, true), (&container, true)] {
        text.extend_from_slice(part);
        if slash {
            text.push(u16::from(b'/'));
        }
    }
    if text.len() >= cap {
        text.truncate(cap - 1);
    }
    cat(&mut text, cap, &name);
    let token: Vec<u16> = if kind == 0 {
        let mut t = Vec::new();
        for (i, &c) in sas.iter().enumerate() {
            t.push(c);
            if c == u16::from(b'%') && sas.get(i + 1) != Some(&u16::from(b'%')) {
                t.push(c);
            }
        }
        t
    } else {
        sas
    };
    cat(&mut text, cap, &token);
    cat(&mut text, cap, &units(",SN="));
    cat(&mut text, cap, &host);
    if flags & 1 != 0 {
        cat(&mut text, cap, &units(",PRST=1"));
    }
    if flags & 4 != 0 {
        cat(&mut text, cap, &units(",NRM=1"));
    }
    if let Some(v) = &vcsa {
        for part in [units(","), units("VCSA"), units("="), v.clone()] {
            cat(&mut text, cap, &part);
        }
        if port != 0 {
            for part in [units(","), units("VCSP"), units("="), port_text] {
                cat(&mut text, cap, &part);
            }
        }
    }
    // SAFETY: a block of `cap` characters; the text and its NUL fit.
    unsafe {
        buffer.write_bytes(0, cap);
        std::ptr::copy_nonoverlapping(text.as_ptr(), buffer, text.len());
        *out = buffer;
    }
    0
}

/// 0x18000b030: no VCSA name or port; the last argument must be 0.
pub extern "system" fn XCrdXasBuildPath(
    out: *mut *mut u16,
    adapter: *const Adapter,
    ip: *const u16,
    host: *const u16,
    account: *const u16,
    container: *const u16,
    name: *const u16,
    sas: *const u16,
    kind: u32,
    flags: u64,
    reserved: u64,
) -> HRESULT {
    if reserved != 0 {
        return E_INVALIDARG;
    }
    build(out, adapter, ip, host, account, container, name, sas, kind, flags, std::ptr::null(), 0)
}

/// 0x18000b0b0.
pub extern "system" fn XCrdXasBuildPathEx(
    out: *mut *mut u16,
    adapter: *const Adapter,
    ip: *const u16,
    host: *const u16,
    account: *const u16,
    container: *const u16,
    name: *const u16,
    sas: *const u16,
    kind: u32,
    flags: u64,
    vcsa: *const u16,
    port: u16,
) -> HRESULT {
    build(out, adapter, ip, host, account, container, name, sas, kind, flags, vcsa, port)
}

pub(crate) fn exports() -> Vec<crate::HleExport> {
    use crate::hle_export;
    vec![hle_export!(XCrdXasBuildPath), hle_export!(XCrdXasBuildPathEx)]
}
