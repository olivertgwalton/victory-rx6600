//! The system calls the XCRD client makes, through one table: the host's
//! (or, for paths, the layer's own) functions, or a test's recorder. The
//! console library imports the same functions; the reference tests patch
//! its imports to the same recorder, so both sides see one fake driver.
#![allow(clippy::type_complexity)]

use std::ffi::c_void;
use std::sync::RwLock;

use crate::modules::files::host;

pub type Handle = *mut c_void;

/// `IO_STATUS_BLOCK`.
#[repr(C)]
#[derive(Default)]
pub struct IoStatus {
    pub status: usize,
    pub information: usize,
}

#[derive(Clone, Copy)]
pub struct Sys {
    pub create_file_w: unsafe extern "system" fn(*const u16, u32, u32, *const c_void, u32, u32, Handle) -> Handle,
    pub nt_device_io_control_file: unsafe extern "system" fn(
        Handle,
        Handle,
        *const c_void,
        *const c_void,
        *mut IoStatus,
        u32,
        *const c_void,
        u32,
        *mut c_void,
        u32,
    ) -> i32,
    pub device_io_control:
        unsafe extern "system" fn(Handle, u32, *const c_void, u32, *mut c_void, u32, *mut u32, *mut c_void) -> i32,
    pub nt_wait_for_single_object: unsafe extern "system" fn(Handle, u8, *const i64) -> i32,
    pub create_event_w: unsafe extern "system" fn(*const c_void, i32, i32, *const u16) -> Handle,
    pub close_handle: unsafe extern "system" fn(Handle) -> i32,
    pub sleep: unsafe extern "system" fn(u32),
    pub get_tick_count64: unsafe extern "system" fn() -> u64,
    /// The library's message output (`_putws`, a line to standard output).
    pub putws: unsafe extern "C" fn(*const u16) -> i32,
}

unsafe extern "system" fn create_file_w(
    path: *const u16,
    access: u32,
    share: u32,
    security: *const c_void,
    disposition: u32,
    flags: u32,
    template: Handle,
) -> Handle {
    // The title's imports bind CreateFileW to the layer's, so paths on its
    // drives resolve as they would for the console library.
    crate::modules::files::CreateFileW(path, access, share, security, disposition, flags, template)
}

unsafe extern "system" fn nt_device_io_control_file(
    file: Handle,
    event: Handle,
    apc: *const c_void,
    context: *const c_void,
    status: *mut IoStatus,
    code: u32,
    input: *const c_void,
    input_len: u32,
    output: *mut c_void,
    output_len: u32,
) -> i32 {
    host!("ntdll.dll", "NtDeviceIoControlFile": extern "system" fn(
        Handle, Handle, *const c_void, *const c_void, *mut IoStatus, u32, *const c_void, u32, *mut c_void, u32,
    ) -> i32)(file, event, apc, context, status, code, input, input_len, output, output_len)
}

unsafe extern "system" fn device_io_control(
    device: Handle,
    code: u32,
    input: *const c_void,
    input_len: u32,
    output: *mut c_void,
    output_len: u32,
    returned: *mut u32,
    overlapped: *mut c_void,
) -> i32 {
    host!("kernel32.dll", "DeviceIoControl": extern "system" fn(
        Handle, u32, *const c_void, u32, *mut c_void, u32, *mut u32, *mut c_void,
    ) -> i32)(device, code, input, input_len, output, output_len, returned, overlapped)
}

unsafe extern "system" fn nt_wait_for_single_object(handle: Handle, alertable: u8, timeout: *const i64) -> i32 {
    host!("ntdll.dll", "NtWaitForSingleObject": extern "system" fn(Handle, u8, *const i64) -> i32)(
        handle, alertable, timeout,
    )
}

unsafe extern "system" fn create_event_w(security: *const c_void, manual: i32, initial: i32, name: *const u16) -> Handle {
    // SAFETY: as the caller passes them.
    unsafe { windows_sys::Win32::System::Threading::CreateEventW(security.cast(), manual, initial, name) }
}

unsafe extern "system" fn close_handle(handle: Handle) -> i32 {
    // SAFETY: as the caller passes it.
    unsafe { windows_sys::Win32::Foundation::CloseHandle(handle) }
}

unsafe extern "system" fn sleep(ms: u32) {
    // SAFETY: plain.
    unsafe { windows_sys::Win32::System::Threading::Sleep(ms) }
}

unsafe extern "system" fn get_tick_count64() -> u64 {
    // SAFETY: plain.
    unsafe { windows_sys::Win32::System::SystemInformation::GetTickCount64() }
}

unsafe extern "C" fn putws(text: *const u16) -> i32 {
    // SAFETY: a NUL-terminated line.
    let line = unsafe { super::wide_str(text) };
    println!("{line}");
    0
}

pub const HOST: Sys = Sys {
    create_file_w,
    nt_device_io_control_file,
    device_io_control,
    nt_wait_for_single_object,
    create_event_w,
    close_handle,
    sleep,
    get_tick_count64,
    putws,
};

static SYS: RwLock<Sys> = RwLock::new(HOST);

pub fn sys() -> Sys {
    *SYS.read().unwrap_or_else(|e| e.into_inner())
}

#[cfg(test)]
pub fn set(sys: Sys) {
    *SYS.write().unwrap_or_else(|e| e.into_inner()) = sys;
}

/// `RtlNtStatusToDosErrorNoTeb`: a pure table lookup, never replaced.
pub fn dos_error(status: i32) -> u32 {
    host!("ntdll.dll", "RtlNtStatusToDosErrorNoTeb": extern "system" fn(i32) -> u32)(status)
}

/// `GetLastError` as a host error.
pub fn last_error() -> u32 {
    // SAFETY: plain.
    unsafe { windows_sys::Win32::Foundation::GetLastError() }
}
