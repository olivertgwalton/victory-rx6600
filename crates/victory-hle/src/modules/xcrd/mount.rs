//! Mounting a CRD as a disk and taking it away again: the driver attaches
//! the content and names its SCSI address; the library waits for the disk
//! and its first partition to appear. Unmounting dismounts the disk's
//! volumes (found through the mount point manager) before the driver
//! detaches it. Addresses are `XCRDAPI.dll`'s.

use super::disks::{Address, Device, INVALID, close, content_address, device, device_io_control, disk_number, open, open_wide};
use super::srb::{HEADER, Payload, copy_string, log, send};
use super::sys::{self, Handle, sys};
use super::{Adapter, E_INVALIDARG, HRESULT, handle_of, heap_alloc, heap_free, hresult_from_win32, wide};

/// ERROR_WRITE_PROTECT, as an HRESULT: no point waiting.
const WRITE_PROTECT: HRESULT = 0x8007_0013_u32 as i32;
/// Tolerated by the unmounts: the volume was already gone.
const NO_VOLUME: HRESULT = 0x8007_01b1_u32 as i32;
const E_INVALID: HRESULT = E_INVALIDARG;

fn last_hresult() -> HRESULT {
    hresult_from_win32(sys::last_error())
}

fn show(name: &[u16]) -> String {
    String::from_utf16_lossy(name)
}

fn sleep(ms: u32) {
    // SAFETY: plain.
    unsafe { (sys().sleep)(ms) };
}

/// 0x180003980: errors not worth waiting out.
fn fatal(hr: HRESULT) -> bool {
    matches!(hr as u32, 0x8007_01e7 | 0x8007_0570 | 0x8007_0571 | 0x8007_045d)
}

/// 0x18000f180: asks the driver to detach the content at this address
/// (control 8: target, path, port... in reverse).
fn detach(adapter: *const Adapter, address: &Address) -> HRESULT {
    let mut p = Payload::new(0x7800_0008, 0x10);
    p.put(0, &[address[7], address[6], address[5]]);
    send(handle_of(adapter), &mut p)
}

/// 0x18000f26c: whether the volume lies on `disk`
/// (IOCTL_VOLUME_GET_VOLUME_DISK_EXTENTS): `(0, true)` when it does,
/// E_INVALIDARG when it does not or the query fails other than for more
/// room.
fn on_disk(h: Handle, disk: u32) -> (HRESULT, bool) {
    let u32_at = |b: &[u8], at: usize| u32::from_le_bytes(b[at..at + 4].try_into().unwrap());
    let mut one = [0u8; 0x20];
    if device_io_control(h, 0x56_0000, &[], &mut one) {
        return if u32_at(&one, 8) == disk { (0, true) } else { (E_INVALID, false) };
    }
    if sys::last_error() != 0xea {
        return (E_INVALID, false);
    }
    let count = u32_at(&one, 0);
    let size = count.wrapping_mul(24).wrapping_add(8) as usize;
    let block = heap_alloc(size);
    if block.is_null() {
        log(1, "Failed to allocate memory for volume extents\n");
        return (0x8007_0008_u32 as i32, false);
    }
    // SAFETY: a block of `size` bytes.
    let all = unsafe { std::slice::from_raw_parts_mut(block, size) };
    let result = if device_io_control(h, 0x56_0000, &[], all) {
        // The library reads as many extents as the answer says.
        let n = u32_at(all, 0) as usize;
        if (0..n).take_while(|i| 8 + 24 * i + 4 <= size).any(|i| u32_at(all, 8 + 24 * i) == disk) { (0, true) } else { (E_INVALID, false) }
    } else {
        let hr = last_hresult();
        log(1, &format!("Failed to query volume extents - {:x}\n", hr as u32));
        (hr, false)
    };
    heap_free(block.cast());
    result
}

/// 0x18000d694: locks the volume (five tries, 50 ms apart, going on
/// regardless) and dismounts it.
fn lock_and_dismount(h: Handle) -> HRESULT {
    let mut hr = 0;
    let mut locked = false;
    for _ in 0..5 {
        if device_io_control(h, 0x9_0018, &[], &mut []) {
            locked = true;
            break;
        }
        hr = last_hresult();
        sleep(0x32);
    }
    if !locked && hr < 0 {
        log(2, &format!("XiDismountVolume - Failed to lock volume - {:x}. Proceeding.\n", hr as u32));
    }
    if device_io_control(h, 0x9_0020, &[], &mut []) { 0 } else { last_hresult() }
}

/// A `MOUNTMGR_MOUNT_POINT` field.
fn field(points: &[u8], at: usize, size: usize) -> usize {
    let mut v = [0u8; 4];
    v[..size].copy_from_slice(&points[at..at + size]);
    u32::from_le_bytes(v) as usize
}

fn chars(points: &[u8], offset: usize, bytes: usize) -> Vec<u16> {
    points[offset..offset + bytes].chunks(2).map(|c| u16::from_le_bytes([c[0], c.get(1).copied().unwrap_or(0)])).collect()
}

/// `MOUNTMGR_IS_VOLUME_NAME`, and whether it is the NT form
/// (`\??\Volume{...}`, no trailing separator).
fn volume_name(name: &[u16]) -> (bool, bool) {
    let at = |i: usize| name.get(i).copied().unwrap_or(0);
    let c = |ch: char| ch as u16;
    let len = name.len() * 2;
    let shaped = (len == 0x60 || (len == 0x62 && at(0x30) == c('\\')))
        && at(0) == c('\\')
        && (at(1) == c('?') || at(1) == c('\\'))
        && at(2) == c('?')
        && at(3) == c('\\')
        && name.get(4..11) == Some(&"Volume{".encode_utf16().collect::<Vec<_>>()[..])
        && at(0x13) == c('-')
        && at(0x18) == c('-')
        && at(0x1d) == c('-')
        && at(0x22) == c('-')
        && at(0x2f) == c('}');
    (shaped, shaped && len == 0x60 && at(1) == c('?'))
}

/// `RtlEqualUnicodeString`, case ignored.
fn same_device(a: &[u16], b: &[u16]) -> bool {
    #[repr(C)]
    struct Unicode {
        length: u16,
        maximum: u16,
        buffer: *const u16,
    }
    let u = |s: &[u16]| Unicode { length: (s.len() * 2) as u16, maximum: (s.len() * 2) as u16, buffer: s.as_ptr() };
    let (a, b) = (u(a), u(b));
    crate::modules::files::host!("ntdll.dll", "RtlEqualUnicodeString": extern "system" fn(*const Unicode, *const Unicode, u8) -> u8)(&a, &b, 1) != 0
}

/// 0x18000d930: the next volume among the mount points, as
/// `\\?\Volume{...}\`. Entries are used up as they are looked at; other
/// names for the same device are dropped. ERROR_NO_MORE_FILES at the end.
fn next_volume(points: &mut [u8], out: &mut Vec<u16>) -> Result<(), u32> {
    let count = field(points, 4, 4);
    let entry = |i: usize| 8 + 24 * i;
    let mut found = None;
    for i in 0..count {
        let e = entry(i);
        let offset = field(points, e, 4);
        if offset == 0 {
            continue;
        }
        let name = chars(points, offset, field(points, e + 4, 2));
        if volume_name(&name).1 {
            found = Some(i);
            break;
        }
        points[e..e + 4].fill(0);
    }
    let Some(i) = found else {
        return Err(0x12);
    };
    let e = entry(i);
    let device = chars(points, field(points, e + 0x10, 4), field(points, e + 0x14, 2));
    for j in i + 1..count {
        let f = entry(j);
        let offset = field(points, f, 4);
        if offset == 0 {
            continue;
        }
        let name = chars(points, offset, field(points, f + 4, 2));
        let keep = volume_name(&name).1 && !same_device(&chars(points, field(points, f + 0x10, 4), field(points, f + 0x14, 2)), &device);
        if !keep {
            points[f..f + 4].fill(0);
        }
    }
    let len = field(points, e + 4, 2);
    if len + 4 > 0x208 {
        return Err(0xce);
    }
    let mut name = chars(points, field(points, e, 4), len);
    name[1] = u16::from(b'\\');
    name.push(u16::from(b'\\'));
    *out = name;
    points[e..e + 4].fill(0);
    Ok(())
}

/// 0x18000d77c: the mount point manager's list (IOCTL_MOUNTMGR_QUERY_POINTS
/// for everything, grown until it fits) and its first volume; a Win32
/// error otherwise.
fn first_volume() -> Result<(Vec<u8>, Vec<u16>), u32> {
    let manager = open("\\\\.\\MountPointManager", 0, 0x80);
    if manager == INVALID {
        return Err(sys::last_error());
    }
    let mut points = vec![0u8; 0x20];
    let ok = loop {
        if device_io_control(manager, 0x6d_0008, &[0; 0x18], &mut points) {
            break true;
        }
        if sys::last_error() != 0xea {
            break false;
        }
        let size = u32::from_le_bytes(points[..4].try_into().unwrap()) as usize;
        points = vec![0u8; size];
    };
    let error = sys::last_error();
    close(manager);
    if !ok {
        return Err(error);
    }
    let mut name = Vec::new();
    next_volume(&mut points, &mut name)?;
    Ok((points, name))
}

/// 0x18000d428: dismounts every volume on `disk`. A volume that opens only
/// for reading and is on the disk cannot be dismounted: S_FALSE.
fn dismount_disk(disk: u32) -> HRESULT {
    let (mut points, mut name) = match first_volume() {
        Ok(v) => v,
        Err(e) => return hresult_from_win32(e),
    };
    let mut hr = 0;
    loop {
        if name.last() == Some(&u16::from(b'\\')) {
            name.pop();
        }
        let h = open_wide(&name, 0xc000_0000, 0x80);
        if h == INVALID {
            let reader = open_wide(&name, 0x8000_0000, 0x80);
            if reader != INVALID {
                if on_disk(reader, disk).0 >= 0 {
                    log(1, &format!("Failed to dismount {} on disk {disk:x}\n", show(&name)));
                    close(reader);
                    return 1;
                }
                close(reader);
            }
            let e = last_hresult();
            log(1, &format!("Failed to open volume {} - {:x}\n", show(&name), e as u32));
        } else {
            let (r, here) = on_disk(h, disk);
            if r >= 0 && here {
                let e = lock_and_dismount(h);
                log(1, &format!("Dismounting disk {} - {:x}\n", disk as i32, e as u32));
                if e < 0 {
                    log(1, &format!("Failed to dismount {} - {:x}\n", show(&name), e as u32));
                    hr = e;
                } else {
                    log(1, &format!("Successfully dismounted volume {}\n", show(&name)));
                }
            }
            close(h);
        }
        if next_volume(&mut points, &mut name).is_err() {
            break;
        }
    }
    hr
}

/// 0x18000d350: dismounts a mounted device's volume (opened for reading if
/// not for writing; with neither, the lock and dismount go to an invalid
/// handle).
fn dismount_device(device: &Device) -> HRESULT {
    // SAFETY: the record's own name.
    let name = unsafe { wide(device.name) };
    let mut h = open_wide(name, 0xc000_0000, 0x80);
    if h == INVALID {
        h = open_wide(name, 0x8000_0000, 0x80);
        if h == INVALID {
            return lock_and_dismount(h);
        }
    }
    let (r, here) = on_disk(h, device.disk);
    if r < 0 {
        close(h);
        return r;
    }
    if !here {
        log(1, &format!("Failed to dismount {} on disk {:x}\n", show(name), device.disk));
    }
    let hr = lock_and_dismount(h);
    close(h);
    hr
}

/// 0x18000cadc: attaches the content (control 4) and waits for its disk:
/// the drive with its address (1600 tries 100 ms apart), then the first
/// partition (800 tries, at most a minute). Giving up detaches it again.
fn mount(out: *mut *mut Device, adapter: *const Adapter, path: *const u16, flags: u32, kind: Option<u32>) -> HRESULT {
    // SAFETY: a NUL-terminated path (the library does not check it).
    let path = unsafe { wide(path) };
    if path.is_empty() {
        return E_INVALIDARG;
    }
    let chars = path.len() + 1;
    let mut p = Payload::new(0x7800_0004, chars * 2 + 0x24);
    let mut mode = 0u32;
    if flags & 1 != 0 {
        mode |= 1;
        if flags & 0x4000 != 0 {
            mode |= 0x8000;
        }
    }
    for (flag, bit) in [(4, 8), (0x20, 0x40), (0x40, 0x100), (0x80, 0x200), (0x400, 0x800), (0x1000, 0x2000), (0x800, 0x1000), (0x2000, 0x4000)] {
        if flags & flag != 0 {
            mode |= bit;
        }
    }
    if let Some(kind) = kind {
        mode |= 0x80;
        p.u32(0x1c, kind);
    }
    p.u32(0x18, mode);
    let status = copy_string(p.body(), 0x24, chars * 2, path);
    if status < 0 {
        return status;
    }
    p.u32(0, 0x24);
    p.u32(4, (path.len() * 2) as u32);
    let hr = send(handle_of(adapter), &mut p);
    if hr < 0 {
        return hr;
    }
    let b = &p.bytes[HEADER..];
    // SAFETY: an adapter of the caller's.
    let port = unsafe { (*adapter).port } as u8;
    let address: Address = [8, 0, 0, 0, port, b[0x22], b[0x21], b[0x20]];
    sleep(0x19);
    let mut hr = hr;
    let mut tries = 0;
    let disk = loop {
        match disk_number(&address) {
            Ok(d) => break Some(d),
            Err(e) => {
                hr = e;
                if e == WRITE_PROTECT || fatal(e) {
                    break None;
                }
                sleep(100);
                tries += 1;
                if tries >= 0x640 {
                    break None;
                }
            }
        }
    };
    if let Some(disk) = disk {
        // SAFETY: plain.
        let start = unsafe { (sys().get_tick_count64)() };
        let mut tries = 0;
        loop {
            match device(disk, &address) {
                Ok(d) => {
                    // SAFETY: the caller's out (not checked).
                    unsafe { *out = d };
                    return 0;
                }
                Err(e) => {
                    hr = e;
                    if e == WRITE_PROTECT || fatal(e) {
                        break;
                    }
                    sleep(100);
                    tries += 1;
                    // SAFETY: plain.
                    let elapsed = unsafe { (sys().get_tick_count64)() }.wrapping_sub(start);
                    if tries >= 0x320 || elapsed >= 60_000 {
                        break;
                    }
                }
            }
        }
    }
    detach(adapter, &address);
    hr
}

/// 0x180003ae0.
pub extern "system" fn XCrdMount(out: *mut *mut Device, adapter: *const Adapter, path: *const u16, flags: u32) -> HRESULT {
    mount(out, adapter, path, flags, None)
}

/// 0x180003b00: as `XCrdMount`, naming the content's type.
pub extern "system" fn XCrdMountContentType(out: *mut *mut Device, adapter: *const Adapter, path: *const u16, kind: u32, flags: u32) -> HRESULT {
    mount(out, adapter, path, flags, Some(kind))
}

/// 0x180009f60: dismounts the device's volume and detaches it, freeing the
/// record when both work; `stage` says where it stopped (1 no adapter, 2 no
/// device, 3 dismounting, 4 detaching, 0 done).
pub extern "system" fn XCrdUnmountEx(adapter: *const Adapter, device: *mut Device, stage: *mut u32) -> HRESULT {
    let (hr, at) = if adapter.is_null() {
        (E_INVALIDARG, 1)
    } else if device.is_null() {
        (E_INVALIDARG, 2)
    } else {
        // SAFETY: a device record of the caller's.
        let d = unsafe { &*device };
        let hr = dismount_device(d);
        if hr < 0 && hr != NO_VOLUME {
            (hr, 3)
        } else {
            let hr = detach(adapter, &d.address);
            if hr < 0 {
                (hr, 4)
            } else {
                heap_free(device.cast());
                (hr, 0)
            }
        }
    };
    if !stage.is_null() {
        // SAFETY: the caller's.
        unsafe { *stage = at };
    }
    hr
}

/// 0x180009ed0.
pub extern "system" fn XCrdUnmount(adapter: *const Adapter, device: *mut Device) -> HRESULT {
    XCrdUnmountEx(adapter, device, std::ptr::null_mut())
}

/// 0x180009ee0: unmounts the content at a path: its disk's volumes, then
/// the driver's copy.
pub extern "system" fn XCrdUnmountByPath(adapter: *const Adapter, path: *const u16) -> HRESULT {
    if adapter.is_null() || path.is_null() {
        return E_INVALIDARG;
    }
    let mut address = [0u8; 8];
    let hr = content_address(adapter, path, &mut address);
    if hr < 0 {
        return hr;
    }
    let disk = match disk_number(&address) {
        Ok(d) => d,
        Err(hr) => return hr,
    };
    let hr = dismount_disk(disk);
    if hr >= 0 || hr == NO_VOLUME { detach(adapter, &address) } else { hr }
}

pub(crate) fn exports() -> Vec<crate::HleExport> {
    use crate::hle_export;
    vec![hle_export!(XCrdMount), hle_export!(XCrdMountContentType), hle_export!(XCrdUnmount), hle_export!(XCrdUnmountEx), hle_export!(XCrdUnmountByPath)]
}
