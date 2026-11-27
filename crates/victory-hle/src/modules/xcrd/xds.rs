//! XDS volume mappings: a primary volume and an optional overlay,
//! exchanged with the driver directly (control codes 0x780001c0/c4, no
//! SRB block). Addresses are `XCRDAPI.dll`'s.

use super::srb::{hresult, log};
use super::sys::{IoStatus, sys};
use super::{Adapter, E_INVALIDARG, HRESULT, handle_of};
use crate::modules::files::host;

fn rd(b: &[u8], at: usize) -> u32 {
    u32::from_le_bytes(b[at..at + 4].try_into().unwrap())
}

fn ioctl(adapter: *const Adapter, code: u32, buf: &mut [u8]) -> i32 {
    let mut io = IoStatus::default();
    let p = buf.as_mut_ptr().cast();
    let n = buf.len() as u32;
    // SAFETY: our buffer; the adapter's handle.
    let status = unsafe { (sys().nt_device_io_control_file)(handle_of(adapter), std::ptr::null_mut(), std::ptr::null(), std::ptr::null(), &mut io, code, p, n, p, n) };
    if status < 0 { status } else { io.status as i32 }
}

/// 0x18000b670: the mapping: the primary volume's name, and the overlay's
/// when asked for; lengths in bytes, in and out.
pub extern "system" fn XCrdXdsGetVolumeMapping(adapter: *const Adapter, primary: *mut u8, primary_len: *mut u32, overlay: *mut u8, overlay_len: *mut u32) -> HRESULT {
    if adapter.is_null() {
        log(1, "Invalid Adapter Handle\n");
        return E_INVALIDARG;
    }
    // SAFETY: the caller's lengths, checked non-null.
    if primary.is_null() || primary_len.is_null() || unsafe { *primary_len } == 0 {
        log(1, "PrimaryVolume must be specified\n");
        return E_INVALIDARG;
    }
    // SAFETY: as above.
    let plen = unsafe { *primary_len };
    let mut size = plen.wrapping_add(0xd);
    if !overlay.is_null() {
        // SAFETY: as above.
        if overlay_len.is_null() || unsafe { *overlay_len } == 0 {
            log(1, "OverlayVolumeLength must be specified\n");
            return E_INVALIDARG;
        }
        // SAFETY: as above.
        size = unsafe { *overlay_len }.wrapping_add(0xd).wrapping_add(size);
    }
    let mut buf = vec![0u8; size as usize];
    let mut status = ioctl(adapter, 0x7800_01c4, &mut buf);
    // SAFETY (below): the caller's buffers of their lengths.
    unsafe {
        if status < 0 {
            if status == 0x8000_0005_u32 as i32 {
                *primary_len = rd(&buf, 0);
            }
        } else {
            let n = rd(&buf, 8);
            if n > *primary_len {
                status = 0xc000_0023_u32 as i32;
            } else {
                *primary_len = n;
                if n != 0 {
                    std::ptr::copy_nonoverlapping(buf[0xc..].as_ptr(), primary, n as usize);
                }
                if !overlay.is_null() {
                    let next = rd(&buf, 0) as usize;
                    if next == 0 {
                        *overlay_len = 0;
                    } else {
                        let m = rd(&buf, next + 8);
                        if m > *overlay_len {
                            status = 0xc000_0023_u32 as i32;
                        } else {
                            *overlay_len = m;
                            if m != 0 {
                                std::ptr::copy_nonoverlapping(buf[next + 0xc..].as_ptr(), overlay, m as usize);
                            }
                        }
                    }
                }
            }
        }
    }
    hresult(status)
}

/// `UNICODE_STRING`.
#[repr(C)]
#[derive(Default)]
struct Ustr {
    len: u16,
    max: u16,
    buf: usize,
}

/// `RtlDosPathNameToNtPathName_U` (the host's; its result unchecked, as
/// the library's), less a trailing backslash.
fn nt_path(path: *const u16) -> Ustr {
    let mut u = Ustr::default();
    host!("ntdll.dll", "RtlDosPathNameToNtPathName_U": extern "system" fn(*const u16, *mut Ustr, *mut u8, *mut u8) -> u8)(path, &mut u, std::ptr::null_mut(), std::ptr::null_mut());
    if u.len > 2 {
        // SAFETY: the string just made.
        let last = unsafe { *(u.buf as *const u16).add(usize::from(u.len / 2) - 1) };
        if last == u16::from(b'\\') {
            u.len -= 2;
            u.max -= 2;
        }
    }
    u
}

fn free(u: &Ustr) {
    if u.buf != 0 {
        use windows_sys::Win32::System::Memory::{GetProcessHeap, HeapFree};
        // SAFETY: the process-heap string ntdll made.
        unsafe { HeapFree(GetProcessHeap(), 0, u.buf as *mut _) };
    }
}

/// 0x18000b830: records of `{next, 1, room, name}` (the room is the NT
/// name's maximum length).
pub extern "system" fn XCrdXdsSetVolumeMapping(adapter: *const Adapter, primary: *const u16, overlay: *const u16) -> HRESULT {
    if adapter.is_null() {
        log(1, "Invalid Adapter Handle\n");
        return E_INVALIDARG;
    }
    if primary.is_null() {
        log(1, "PrimaryVolume must be specified\n");
        return E_INVALIDARG;
    }
    let a = nt_path(primary);
    let b = if overlay.is_null() { Ustr::default() } else { nt_path(overlay) };
    let mut size = u32::from(a.max) + 0xd;
    if !overlay.is_null() {
        size += u32::from(b.max) + 0xd;
    }
    let mut buf = vec![0u8; size as usize];
    let put = |buf: &mut [u8], at: usize, u: &Ustr| {
        buf[at + 4..at + 8].copy_from_slice(&1u32.to_le_bytes());
        buf[at + 8..at + 12].copy_from_slice(&u32::from(u.max).to_le_bytes());
        if u.buf != 0 {
            // SAFETY: the string's `len` bytes.
            let s = unsafe { std::slice::from_raw_parts(u.buf as *const u8, usize::from(u.len)) };
            buf[at + 0xc..at + 0xc + s.len()].copy_from_slice(s);
        }
    };
    put(&mut buf, 0, &a);
    if !overlay.is_null() {
        let next = usize::from(a.max) + 0xd;
        buf[0..4].copy_from_slice(&(next as u32).to_le_bytes());
        put(&mut buf, next, &b);
    }
    let status = ioctl(adapter, 0x7800_01c0, &mut buf);
    free(&a);
    free(&b);
    hresult(status)
}

pub(crate) fn exports() -> Vec<crate::HleExport> {
    use crate::hle_export;
    vec![hle_export!(XCrdXdsGetVolumeMapping), hle_export!(XCrdXdsSetVolumeMapping)]
}
