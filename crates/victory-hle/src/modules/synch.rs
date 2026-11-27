//! Parameter checks the console's KernelBase and ntdll make on events,
//! semaphores, condition variables, critical sections and local memory
//! and locale names (`api-ms-win-core-synch`, `-heap`, `-localization`)
//! that Wine does not:
//! there the call succeeds. Everything past the checks is the host's.
//! RVAs are of the console's `KernelBase.dll` and `ntdll.dll`.
#![allow(non_snake_case)]

use std::ffi::c_void;

use crate::modules::files::host;
use crate::{HleModule, hle_export};

/// `STATUS_INVALID_PARAMETER_n`, as `BaseSetLastNTError` reports it.
const ERROR_INVALID_PARAMETER: u32 = 87;

fn refuse<T>(nothing: T) -> T {
    // SAFETY: plain call.
    unsafe { windows_sys::Win32::Foundation::SetLastError(ERROR_INVALID_PARAMETER) };
    nothing
}

/// `CreateEventExW` (+0x6e1c0): flags other than manual reset (1) and
/// initially set (2) are `STATUS_INVALID_PARAMETER_3`.
pub extern "system" fn CreateEventExW(
    security: *mut c_void,
    name: *const u16,
    flags: u32,
    access: u32,
) -> *mut c_void {
    if flags & !3 != 0 {
        return refuse(std::ptr::null_mut());
    }
    host!("kernel32.dll", "CreateEventExW": extern "system" fn(*mut c_void, *const u16, u32, u32) -> *mut c_void)(
        security, name, flags, access,
    )
}

/// `CreateSemaphoreExW` (+0x6c4c0): the flags are reserved and must be 0
/// (`STATUS_INVALID_PARAMETER_5`, checked first).
pub extern "system" fn CreateSemaphoreExW(
    security: *mut c_void,
    initial: i32,
    maximum: i32,
    name: *const u16,
    flags: u32,
    access: u32,
) -> *mut c_void {
    if flags != 0 {
        return refuse(std::ptr::null_mut());
    }
    host!("kernel32.dll", "CreateSemaphoreExW": extern "system" fn(
        *mut c_void, i32, i32, *const u16, u32, u32,
    ) -> *mut c_void)(security, initial, maximum, name, flags, access)
}

/// `SleepConditionVariableSRW` (+0x41d40 → ntdll +0xabdd0): flags other
/// than shared (1) fail at once (Wine waits and times out).
pub extern "system" fn SleepConditionVariableSRW(
    variable: *mut c_void,
    lock: *mut c_void,
    timeout: u32,
    flags: u32,
) -> i32 {
    if flags & !1 != 0 {
        return refuse(0);
    }
    host!("kernel32.dll", "SleepConditionVariableSRW": extern "system" fn(*mut c_void, *mut c_void, u32, u32) -> i32)(
        variable, lock, timeout, flags,
    )
}

/// `InitializeCriticalSectionEx` (+0xc8c90 → ntdll +0x66b80): reserved
/// flags (`0xe000_0000`), "no debug info" with "static init"
/// (`0x1100_0000` together) and a spin count above 24 bits fail.
pub extern "system" fn InitializeCriticalSectionEx(
    section: *mut c_void,
    spin: u32,
    flags: u32,
) -> i32 {
    if flags & 0xe000_0000 != 0 || flags & 0x1100_0000 == 0x1100_0000 || spin & 0xff00_0000 != 0 {
        return refuse(0);
    }
    host!("kernel32.dll", "InitializeCriticalSectionEx": extern "system" fn(*mut c_void, u32, u32) -> i32)(
        section, spin, flags,
    )
}

/// `LocalAlloc` (+0x53340): only `LMEM_MOVEABLE` (2), `LMEM_NOCOMPACT`
/// (0x10), `LMEM_NODISCARD` (0x20), `LMEM_ZEROINIT` (0x40) and the discard
/// bits (0xf00) are flags (mask `0xffff_f08d` refused).
pub extern "system" fn LocalAlloc(flags: u32, bytes: usize) -> *mut c_void {
    if flags & 0xffff_f08d != 0 {
        return refuse(std::ptr::null_mut());
    }
    host!("kernel32.dll", "LocalAlloc": extern "system" fn(u32, usize) -> *mut c_void)(flags, bytes)
}

/// `LocaleNameToLCID` (+0xcbf90): the only flag is "allow neutral names"
/// (0x0800_0000); any other is refused (Wine ignores them).
pub extern "system" fn LocaleNameToLCID(name: *const u16, flags: u32) -> u32 {
    if flags & !0x0800_0000 != 0 {
        return refuse(0);
    }
    host!("kernel32.dll", "LocaleNameToLCID": extern "system" fn(*const u16, u32) -> u32)(
        name, flags,
    )
}

pub(crate) fn module() -> HleModule {
    HleModule {
        name: "synch",
        dlls: &[
            "kernel32.dll",
            "kernelbase.dll",
            "api-ms-win-core-synch-*",
            "api-ms-win-core-heap-*",
            "api-ms-win-core-localization-*",
        ],
        exports: vec![
            hle_export!(CreateEventExW),
            hle_export!(CreateSemaphoreExW),
            hle_export!(SleepConditionVariableSRW),
            hle_export!(InitializeCriticalSectionEx),
            hle_export!(LocalAlloc),
            hle_export!(LocaleNameToLCID),
        ],
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn last_error() -> u32 {
        // SAFETY: plain call.
        unsafe { windows_sys::Win32::Foundation::GetLastError() }
    }

    fn close(handle: *mut c_void) {
        // SAFETY: our handle.
        unsafe { windows_sys::Win32::Foundation::CloseHandle(handle) };
    }

    /// KernelBase +0x6e1c0 / +0x6c4c0: reserved flags refused (Wine made
    /// both objects); the valid ones still work.
    #[test]
    fn events_and_semaphores_refuse_reserved_flags() {
        let null = std::ptr::null_mut();
        assert!(CreateEventExW(null, std::ptr::null(), 4, 0x1f_0003).is_null());
        assert_eq!(last_error(), ERROR_INVALID_PARAMETER);
        let e = CreateEventExW(null, std::ptr::null(), 3, 0x1f_0003);
        assert!(!e.is_null());
        close(e);
        assert!(CreateSemaphoreExW(null, 0, 1, std::ptr::null(), 1, 0x1f_0003).is_null());
        assert_eq!(last_error(), ERROR_INVALID_PARAMETER);
        let s = CreateSemaphoreExW(null, 0, 1, std::ptr::null(), 0, 0x1f_0003);
        assert!(!s.is_null());
        close(s);
    }

    /// ntdll +0xabdd0: flags beyond shared fail without waiting (Wine:
    /// ERROR_TIMEOUT after the wait).
    #[test]
    fn condition_waits_refuse_unknown_flags() {
        let (mut variable, mut lock) = (0usize, 0usize);
        let (v, l) = ((&raw mut variable).cast(), (&raw mut lock).cast());
        assert_eq!(SleepConditionVariableSRW(v, l, 0, 2), 0);
        assert_eq!(last_error(), ERROR_INVALID_PARAMETER);
        assert_eq!(SleepConditionVariableSRW(v, l, 0, 0), 0);
        assert_eq!(last_error(), 1460, "a real wait times out");
    }

    /// ntdll +0x66b80: reserved flags, conflicting flags, a 25-bit spin.
    #[test]
    fn critical_sections_refuse_reserved_flags_and_spins() {
        let mut section = [0u64; 5];
        let p = section.as_mut_ptr().cast();
        for (spin, flags) in [(0, 0x2000_0000), (0, 0x1100_0000), (0x0100_0000, 0)] {
            assert_eq!(
                InitializeCriticalSectionEx(p, spin, flags),
                0,
                "{spin:#x} {flags:#x}"
            );
            assert_eq!(last_error(), ERROR_INVALID_PARAMETER);
        }
        assert_eq!(InitializeCriticalSectionEx(p, 4000, 0x1000_0000), 1);
    }

    /// KernelBase +0x53340: flags outside the LMEM set refused (Wine
    /// allocated); fixed and zeroed memory still comes back.
    #[test]
    fn local_memory_refuses_unknown_flags() {
        for flags in [0x1, 0x8, 0x80, 0x1000] {
            assert!(LocalAlloc(flags, 16).is_null(), "{flags:#x}");
            assert_eq!(last_error(), ERROR_INVALID_PARAMETER);
        }
        let p = LocalAlloc(0x40, 16);
        assert!(!p.is_null());
        // SAFETY: 16 zeroed bytes, ours.
        unsafe {
            assert_eq!(*p.cast::<[u8; 16]>(), [0; 16]);
            windows_sys::Win32::Foundation::LocalFree(p);
        }
    }

    /// KernelBase +0xcbf90: unknown flags fail; neutral names come back as
    /// such only when allowed.
    #[test]
    fn locale_names_refuse_unknown_flags() {
        let gb: Vec<u16> = "en-GB".encode_utf16().chain(Some(0)).collect();
        assert_eq!(LocaleNameToLCID(gb.as_ptr(), 1), 0);
        assert_eq!(last_error(), ERROR_INVALID_PARAMETER);
        assert_eq!(LocaleNameToLCID(gb.as_ptr(), 0), 0x809);
        let en: Vec<u16> = "en".encode_utf16().chain(Some(0)).collect();
        assert_eq!(LocaleNameToLCID(en.as_ptr(), 0x0800_0000), 0x9);
    }
}
