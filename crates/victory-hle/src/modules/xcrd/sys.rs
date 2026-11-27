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
    /// WNF: the library subscribes to three state names.
    pub rtl_query_wnf_state_data: unsafe extern "system" fn(*mut u32, u64, *const std::ffi::c_void, *mut std::ffi::c_void, *const std::ffi::c_void) -> i32,
    pub rtl_subscribe_wnf: unsafe extern "system" fn(*mut *mut std::ffi::c_void, u64, u32, *const std::ffi::c_void, *mut std::ffi::c_void, *const std::ffi::c_void, u32, u32) -> i32,
    pub rtl_unsubscribe_wnf: unsafe extern "system" fn(*mut std::ffi::c_void) -> i32,
    /// `NtQueryVolumeInformationFile` (handle, status block, buffer, length, class).
    pub nt_query_volume_information_file: unsafe extern "system" fn(Handle, *mut IoStatus, *mut c_void, u32, u32) -> i32,
    /// Files, for the O: drive's blobs.
    pub get_drive_type_w: unsafe extern "system" fn(*const u16) -> u32,
    pub find_first_file_w: unsafe extern "system" fn(*const u16, *mut c_void) -> Handle,
    pub find_next_file_w: unsafe extern "system" fn(Handle, *mut c_void) -> i32,
    pub find_close: unsafe extern "system" fn(Handle) -> i32,
    pub set_file_pointer_ex: unsafe extern "system" fn(Handle, i64, *mut i64, u32) -> i32,
    pub read_file: unsafe extern "system" fn(Handle, *mut c_void, u32, *mut u32, *mut c_void) -> i32,
    /// The transfer pipe.
    pub create_named_pipe_w: unsafe extern "system" fn(*const u16, u32, u32, u32, u32, u32, u32, *const c_void) -> Handle,
    pub connect_named_pipe: unsafe extern "system" fn(Handle, *mut c_void) -> i32,
    pub disconnect_named_pipe: unsafe extern "system" fn(Handle) -> i32,
    pub write_file: unsafe extern "system" fn(Handle, *const c_void, u32, *mut u32, *mut c_void) -> i32,
    pub get_overlapped_result: unsafe extern "system" fn(Handle, *mut c_void, *mut u32, i32) -> i32,
    pub cancel_io_ex: unsafe extern "system" fn(Handle, *mut c_void) -> i32,
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

unsafe extern "system" fn rtl_query_wnf_state_data(stamp: *mut u32, name: u64, handler: *const std::ffi::c_void, context: *mut std::ffi::c_void, kind: *const std::ffi::c_void) -> i32 {
    host!("ntdll.dll", "RtlQueryWnfStateData": extern "system" fn(*mut u32, u64, *const std::ffi::c_void, *mut std::ffi::c_void, *const std::ffi::c_void) -> i32)(stamp, name, handler, context, kind)
}

unsafe extern "system" fn rtl_subscribe_wnf(out: *mut *mut std::ffi::c_void, name: u64, stamp: u32, handler: *const std::ffi::c_void, context: *mut std::ffi::c_void, kind: *const std::ffi::c_void, group: u32, unknown: u32) -> i32 {
    host!("ntdll.dll", "RtlSubscribeWnfStateChangeNotification": extern "system" fn(*mut *mut std::ffi::c_void, u64, u32, *const std::ffi::c_void, *mut std::ffi::c_void, *const std::ffi::c_void, u32, u32) -> i32)(out, name, stamp, handler, context, kind, group, unknown)
}

unsafe extern "system" fn rtl_unsubscribe_wnf(subscription: *mut std::ffi::c_void) -> i32 {
    host!("ntdll.dll", "RtlUnsubscribeWnfNotificationWaitForCompletion": extern "system" fn(*mut std::ffi::c_void) -> i32)(subscription)
}

unsafe extern "system" fn nt_query_volume_information_file(file: Handle, io: *mut IoStatus, buffer: *mut c_void, length: u32, class: u32) -> i32 {
    host!("ntdll.dll", "NtQueryVolumeInformationFile": extern "system" fn(Handle, *mut IoStatus, *mut c_void, u32, u32) -> i32)(file, io, buffer, length, class)
}

unsafe extern "system" fn get_drive_type_w(root: *const u16) -> u32 {
    host!("kernel32.dll", "GetDriveTypeW": extern "system" fn(*const u16) -> u32)(root)
}

unsafe extern "system" fn find_first_file_w(pattern: *const u16, data: *mut c_void) -> Handle {
    host!("kernel32.dll", "FindFirstFileW": extern "system" fn(*const u16, *mut c_void) -> Handle)(pattern, data)
}

unsafe extern "system" fn find_next_file_w(find: Handle, data: *mut c_void) -> i32 {
    host!("kernel32.dll", "FindNextFileW": extern "system" fn(Handle, *mut c_void) -> i32)(find, data)
}

unsafe extern "system" fn find_close(find: Handle) -> i32 {
    host!("kernel32.dll", "FindClose": extern "system" fn(Handle) -> i32)(find)
}

unsafe extern "system" fn set_file_pointer_ex(file: Handle, distance: i64, position: *mut i64, method: u32) -> i32 {
    host!("kernel32.dll", "SetFilePointerEx": extern "system" fn(Handle, i64, *mut i64, u32) -> i32)(file, distance, position, method)
}

unsafe extern "system" fn read_file(file: Handle, buffer: *mut c_void, length: u32, read: *mut u32, overlapped: *mut c_void) -> i32 {
    host!("kernel32.dll", "ReadFile": extern "system" fn(Handle, *mut c_void, u32, *mut u32, *mut c_void) -> i32)(file, buffer, length, read, overlapped)
}

unsafe extern "system" fn create_named_pipe_w(name: *const u16, open: u32, mode: u32, instances: u32, out: u32, inn: u32, timeout: u32, security: *const c_void) -> Handle {
    host!("kernel32.dll", "CreateNamedPipeW": extern "system" fn(*const u16, u32, u32, u32, u32, u32, u32, *const c_void) -> Handle)(name, open, mode, instances, out, inn, timeout, security)
}

unsafe extern "system" fn connect_named_pipe(pipe: Handle, overlapped: *mut c_void) -> i32 {
    host!("kernel32.dll", "ConnectNamedPipe": extern "system" fn(Handle, *mut c_void) -> i32)(pipe, overlapped)
}

unsafe extern "system" fn disconnect_named_pipe(pipe: Handle) -> i32 {
    host!("kernel32.dll", "DisconnectNamedPipe": extern "system" fn(Handle) -> i32)(pipe)
}

unsafe extern "system" fn write_file(file: Handle, buffer: *const c_void, length: u32, written: *mut u32, overlapped: *mut c_void) -> i32 {
    host!("kernel32.dll", "WriteFile": extern "system" fn(Handle, *const c_void, u32, *mut u32, *mut c_void) -> i32)(file, buffer, length, written, overlapped)
}

unsafe extern "system" fn get_overlapped_result(file: Handle, overlapped: *mut c_void, bytes: *mut u32, wait: i32) -> i32 {
    host!("kernel32.dll", "GetOverlappedResult": extern "system" fn(Handle, *mut c_void, *mut u32, i32) -> i32)(file, overlapped, bytes, wait)
}

unsafe extern "system" fn cancel_io_ex(file: Handle, overlapped: *mut c_void) -> i32 {
    host!("kernel32.dll", "CancelIoEx": extern "system" fn(Handle, *mut c_void) -> i32)(file, overlapped)
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
    rtl_query_wnf_state_data,
    rtl_subscribe_wnf,
    rtl_unsubscribe_wnf,
    nt_query_volume_information_file,
    get_drive_type_w,
    find_first_file_w,
    find_next_file_w,
    find_close,
    set_file_pointer_ex,
    read_file,
    create_named_pipe_w,
    connect_named_pipe,
    disconnect_named_pipe,
    write_file,
    get_overlapped_result,
    cancel_io_ex,
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
