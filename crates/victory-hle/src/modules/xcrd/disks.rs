//! From a mounted CRD to its disk: the driver gives the content's SCSI
//! address, the library looks for the physical drive with that address,
//! and opens its first partition. Addresses are `XCRDAPI.dll`'s.

use std::ffi::c_void;

use super::srb::{HEADER, Payload, copy_string, hresult, send};
use super::sys::{self, Handle, sys};
use super::{Adapter, E_INVALIDARG, E_OUTOFMEMORY, HRESULT, handle_of, heap_alloc, heap_free, hresult_from_win32, units, wide};

const INVALID: Handle = -1isize as Handle;

/// `SCSI_ADDRESS`: length 8, port, path, target, lun.
pub type Address = [u8; 8];

/// 0x18000e87c: the content's SCSI address (control code 0x78000078); the
/// port is the adapter's.
pub(crate) fn content_address(adapter: *const Adapter, path: *const u16, out: &mut Address) -> HRESULT {
    // SAFETY: a NUL-terminated path.
    let path = unsafe { wide(path) };
    let total = path.len() * 2 + 0x22;
    let mut p = Payload::new(0x7800_0078, total);
    p.u32(0, total as u32);
    p.u32(4, 0x20);
    p.u32(8, (path.len() * 2) as u32);
    let status = copy_string(p.body(), 0x20, path.len() * 2 + 2, path);
    if status < 0 {
        return status;
    }
    let hr = send(handle_of(adapter), &mut p);
    if hr < 0 {
        return hr;
    }
    let b = &p.bytes[HEADER..];
    // SAFETY: an adapter of the caller's.
    let port = unsafe { (*adapter).port } as u8;
    *out = [8, 0, 0, 0, port, b[0x1a], b[0x19], b[0x18]];
    0
}

fn device_io_control(h: Handle, code: u32, input: &[u8], output: &mut [u8]) -> bool {
    let mut returned = 0u32;
    let (ip, il) = if input.is_empty() { (std::ptr::null(), 0) } else { (input.as_ptr().cast(), input.len() as u32) };
    let (op, ol) = if output.is_empty() { (std::ptr::null_mut(), 0) } else { (output.as_mut_ptr().cast(), output.len() as u32) };
    // SAFETY: our buffers.
    unsafe { (sys().device_io_control)(h, code, ip, il, op, ol, &mut returned, std::ptr::null_mut()) != 0 }
}

fn open(name: &str, access: u32, flags: u32) -> Handle {
    let mut w = units(name);
    w.push(0);
    // SAFETY: a NUL-terminated name.
    unsafe { (sys().create_file_w)(w.as_ptr(), access, 3, std::ptr::null(), 3, flags, std::ptr::null_mut()) }
}

fn close(h: Handle) {
    // SAFETY: a handle we opened.
    unsafe { (sys().close_handle)(h) };
}

/// 0x18000dc0c: the physical drive (0-127) with this SCSI address, not an
/// SD or USB one; ERROR_NOT_FOUND without.
pub(crate) fn disk_number(address: &Address) -> Result<u32, HRESULT> {
    for i in 0..0x80u32 {
        let h = open(&format!("\\\\.\\PhysicalDrive{i}"), 0xc000_0000, 0);
        if h == INVALID {
            let _ = sys::last_error();
            continue;
        }
        let mut descriptor = [0u8; 0x28];
        if device_io_control(h, 0x2d_1400, &[0; 12], &mut descriptor) {
            let bus = u32::from_le_bytes(descriptor[0x1c..0x20].try_into().unwrap());
            if bus == 0xc || bus == 7 {
                close(h);
                continue;
            }
        } else {
            let _ = sys::last_error();
        }
        let mut found = [0u8; 8];
        if device_io_control(h, 0x4_1018, &[], &mut found) && found[4..8] == address[4..8] {
            close(h);
            return Ok(i);
        }
        close(h);
    }
    Err(0x8007_0490_u32 as i32)
}

/// The library's device record: its name (a pointer to the name after it),
/// the SCSI address, the disk number, the first word the volume query
/// returns, then the name.
#[repr(C)]
pub struct Device {
    pub name: *const u16,
    pub address: Address,
    pub disk: u32,
    pub volume: u32,
}

/// 0x18000c56c: the disk's first partition: open, not write-protected or
/// writable, its volume query (0x56001c), and openable as a directory.
pub(crate) fn device(disk: u32, address: &Address) -> Result<*mut Device, HRESULT> {
    if disk > 9999 {
        return Err(E_INVALIDARG);
    }
    let name = format!("\\\\?\\GLOBALROOT\\Device\\Harddisk{disk}\\Partition1");
    let h = open(&name, 0x80, 0);
    if h == INVALID {
        return Err(hresult_from_win32(sys::last_error()));
    }
    let result = (|| {
        if !device_io_control(h, 0x7_0024, &[], &mut []) {
            let e = sys::last_error();
            if e != 0x13 {
                return Err(hresult_from_win32(e));
            }
        }
        let mut volume = [0u8; 0x14];
        if !device_io_control(h, 0x56_001c, &[], &mut volume) {
            return Err(hresult_from_win32(sys::last_error()));
        }
        let w = units(&name);
        let size = w.len() * 2 + 0x1a;
        let record = heap_alloc(size);
        if record.is_null() {
            return Err(E_OUTOFMEMORY);
        }
        // SAFETY: a new block of `size` bytes.
        unsafe {
            record.write_bytes(0, size);
            let text = std::slice::from_raw_parts_mut(record.add(0x18), size - 0x18);
            let status = copy_string(text, 0, size - 0x18, &w);
            if status < 0 {
                heap_free(record.cast());
                return Err(hresult(status));
            }
            let d = record.cast::<Device>();
            (*d).address = *address;
            (*d).disk = disk;
            (*d).name = record.add(0x18).cast();
            (*d).volume = u32::from_le_bytes(volume[..4].try_into().unwrap());
        }
        let root = open(&format!("{name}\\"), 0x8000_0000, 0x200_0000);
        if root == INVALID {
            let hr = hresult_from_win32(sys::last_error());
            if hr < 0 {
                heap_free(record.cast());
                return Err(hr);
            }
        } else {
            close(root);
        }
        Ok(record.cast::<Device>())
    })();
    close(h);
    result
}

/// 0x180004d90: a copy of the device's name, on the process heap.
pub extern "system" fn XCrdQueryDevicePath(out: *mut *mut u16, device: *const Device) -> HRESULT {
    if device.is_null() {
        return E_INVALIDARG;
    }
    // SAFETY: a device record.
    let name = unsafe { wide((*device).name) };
    let bytes = name.len() * 2 + 2;
    let buffer = heap_alloc(bytes);
    // SAFETY: the caller's out.
    unsafe { *out = buffer.cast() };
    if buffer.is_null() {
        return E_OUTOFMEMORY;
    }
    // SAFETY: a block of `bytes`.
    let status = copy_string(unsafe { std::slice::from_raw_parts_mut(buffer, bytes) }, 0, bytes, name);
    if status < 0 {
        heap_free(buffer.cast());
        // SAFETY: as above.
        unsafe { *out = std::ptr::null_mut() };
    }
    status
}

/// 0x180004e40: the device path of a mounted CRD.
pub extern "system" fn XCrdQueryDevicePathByPath(adapter: *const Adapter, path: *const u16, out: *mut *mut u16) -> HRESULT {
    if path.is_null() {
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
    match device(disk, &address) {
        Ok(d) => {
            let hr = XCrdQueryDevicePath(out, d);
            heap_free(d.cast::<c_void>());
            hr
        }
        Err(hr) => hr,
    }
}

pub(crate) fn exports() -> Vec<crate::HleExport> {
    use crate::hle_export;
    vec![hle_export!(XCrdQueryDevicePath), hle_export!(XCrdQueryDevicePathByPath)]
}
