//! Thread creation and placement as the console's KernelBase does it
//! (`api-ms-win-core-processthreads`): its checks on thread attribute
//! lists and creation flags, and ideal processors, which Wine does not
//! keep (`SetThreadIdealProcessorEx` fails there with
//! `ERROR_CALL_NOT_IMPLEMENTED`).
//!
//! The list layout is the console's, and Wine's too, so the host's
//! `CreateRemoteThreadEx` reads the lists made here: a header (mask of
//! attributes present, capacity, count, extended-flags entry) and 24-byte
//! entries `{attribute, size, value}`. RVAs below are of the console's
//! `KernelBase.dll`.
#![allow(non_snake_case)]
#![allow(clippy::not_unsafe_ptr_arg_deref)]
#![allow(clippy::too_many_arguments)]

use std::collections::HashMap;
use std::ffi::c_void;
use std::sync::{Mutex, MutexGuard};

use crate::modules::files::host;
use crate::{HleModule, hle_export};

/// The list's header; entries follow it.
#[repr(C)]
struct Header {
    /// Bit `attribute & 31` of each attribute added.
    present: u32,
    capacity: u32,
    count: u32,
    _reserved: u32,
    /// The `PROC_THREAD_ATTRIBUTE_EXTENDED_FLAGS` entry, once there is one.
    extended: *mut Entry,
}

#[repr(C)]
struct Entry {
    attribute: usize,
    size: usize,
    value: *mut c_void,
}

const HEADER: usize = size_of::<Header>();
const ENTRY: usize = size_of::<Entry>();
const _: () = assert!(HEADER == 24 && ENTRY == 24);

/// Most entries a list can have (`InitializeProcThreadAttributeList` +0xffbe0).
const MAX_ATTRIBUTES: u32 = 31;

const THREAD: usize = 0x1_0000;
const INPUT: usize = 0x2_0000;
const ADDITIVE: usize = 0x4_0000;
const GROUP_AFFINITY: usize = 0x3_0003;
const IDEAL_PROCESSOR: usize = 0x3_0005;
const ENABLE_OPTIONAL_XSTATE_FEATURES: usize = 0x3_001b;
const EXTENDED_FLAGS: usize = 0x6_0001;
const REPLACE_VALUE: u32 = 1;

const CREATE_SUSPENDED: u32 = 0x4;
const STACK_SIZE_PARAM_IS_A_RESERVATION: u32 = 0x1_0000;

// Win32 errors of the statuses the console sets (`BaseSetLastNTError`).
const ERROR_INVALID_HANDLE: u32 = 6;
const ERROR_BAD_LENGTH: u32 = 24; // STATUS_INFO_LENGTH_MISMATCH
const ERROR_GEN_FAILURE: u32 = 31; // STATUS_UNSUCCESSFUL
const ERROR_NOT_SUPPORTED: u32 = 50; // STATUS_NOT_SUPPORTED
const ERROR_INVALID_PARAMETER: u32 = 87; // STATUS_INVALID_PARAMETER(_n)
const ERROR_INSUFFICIENT_BUFFER: u32 = 122;
const ERROR_ALREADY_EXISTS: u32 = 183; // STATUS_OBJECT_NAME_EXISTS

fn set_last_error(code: u32) {
    // SAFETY: plain call.
    unsafe { windows_sys::Win32::Foundation::SetLastError(code) }
}

/// `InitializeProcThreadAttributeList` (+0xffbe0): flags must be zero and
/// at most 31 attributes; a missing or small buffer gets the size needed
/// and `ERROR_INSUFFICIENT_BUFFER`.
pub extern "system" fn InitializeProcThreadAttributeList(
    list: *mut u8,
    count: u32,
    flags: u32,
    size: *mut usize,
) -> i32 {
    match initialize(list, count, flags, size) {
        Ok(()) => 1,
        Err(code) => {
            set_last_error(code);
            0
        }
    }
}

fn initialize(list: *mut u8, count: u32, flags: u32, size: *mut usize) -> Result<(), u32> {
    if flags != 0 || count > MAX_ATTRIBUTES {
        return Err(ERROR_INVALID_PARAMETER);
    }
    // The console reads it unchecked; a null one fails here instead.
    if size.is_null() {
        return Err(ERROR_INVALID_PARAMETER);
    }
    let needed = (count as usize + 1) * ENTRY;
    // SAFETY: the title passes its size and, if not null, a list that big.
    unsafe {
        let room = if list.is_null() { 0 } else { size.read() };
        size.write(needed);
        if list.is_null() || room < needed {
            return Err(ERROR_INSUFFICIENT_BUFFER);
        }
        let header = list.cast::<Header>();
        (*header).present = 0;
        (*header).capacity = count;
        (*header).count = 0;
        (*header).extended = std::ptr::null_mut();
    }
    Ok(())
}

/// `DeleteProcThreadAttributeList` (+0x124b20, a bare `ret`): nothing to free.
pub extern "system" fn DeleteProcThreadAttributeList(_list: *mut u8) {}

/// `UpdateProcThreadAttribute` (+0xf6ee0).
pub extern "system" fn UpdateProcThreadAttribute(
    list: *mut u8,
    flags: u32,
    attribute: usize,
    value: *mut c_void,
    size: usize,
    previous: *mut c_void,
    return_size: *mut usize,
) -> i32 {
    // SAFETY: the title passes a list it initialised and values of `size`.
    match unsafe {
        update(
            list.cast(),
            flags,
            attribute,
            value,
            size,
            previous,
            return_size,
        )
    } {
        Ok(()) => 1,
        Err(code) => {
            set_last_error(code);
            0
        }
    }
}

/// The size an attribute's value must have, or why it cannot be added.
fn check_size(attribute: usize, value: *const c_void, size: usize) -> Result<(), u32> {
    let exactly = |n| {
        if size == n {
            Ok(())
        } else {
            Err(ERROR_BAD_LENGTH)
        }
    };
    match attribute {
        // Parent process, job object, and other handles or pointers.
        0x2_0000
        | 0x2_000a
        | 0x2_000c
        | 0x2_0010
        | 0x2_0015
        | 0x2_0016
        | ENABLE_OPTIONAL_XSTATE_FEATURES => exactly(8),
        // Handle and job lists: whole pointers, at least one.
        0x2_0002 | 0x2_000d => {
            if size != 0 && size.is_multiple_of(8) {
                Ok(())
            } else {
                Err(ERROR_BAD_LENGTH)
            }
        }
        0x2_0004 | 0x2_0019 => exactly(2),
        // Mitigation policies: one, two or four quadwords' worth, or a dword.
        0x2_0007 | 0x2_0018 => match size {
            4 | 8 | 16 => Ok(()),
            _ => Err(ERROR_BAD_LENGTH),
        },
        // Package names and such: a non-empty UTF-16 string below 64 KiB.
        0x2_0008 | 0x2_0014 => {
            if size != 0 && size.is_multiple_of(2) && size < 0xffff {
                Ok(())
            } else {
                Err(ERROR_INVALID_PARAMETER)
            }
        }
        // Security capabilities: 24 bytes whose reserved dword is zero.
        0x2_0009 => {
            // SAFETY: 24 bytes, as checked first.
            if size == 0x18
                && unsafe { value.cast::<u8>().add(0x14).cast::<u32>().read_unaligned() } == 0
            {
                Ok(())
            } else {
                Err(ERROR_INVALID_PARAMETER)
            }
        }
        0x2_000b | 0x2_000e | 0x2_000f | 0x2_0012 | 0x2_001a | 0x2_001c | IDEAL_PROCESSOR => {
            exactly(4)
        }
        0x2_0011 => exactly(0x20c),
        0x2_0013 => exactly(0x114),
        0x2_0017 => exactly(0x38),
        0x2_001d => {
            if size >= 1 {
                Ok(())
            } else {
                Err(ERROR_BAD_LENGTH)
            }
        }
        GROUP_AFFINITY => exactly(0x10),
        EXTENDED_FLAGS => exactly(4),
        _ => Err(ERROR_NOT_SUPPORTED),
    }
}

/// See [`UpdateProcThreadAttribute`]; the order of the checks is the console's.
///
/// # Safety
/// `list` is an initialised list; `value` holds `size` bytes.
unsafe fn update(
    list: *mut Header,
    flags: u32,
    attribute: usize,
    value: *mut c_void,
    size: usize,
    previous: *mut c_void,
    return_size: *mut usize,
) -> Result<(), u32> {
    if flags & !REPLACE_VALUE != 0 {
        return Err(ERROR_INVALID_PARAMETER);
    }
    // `shl esi, cl`: a 32-bit shift by the attribute number's low five bits.
    let bit = 1u32 << (attribute & 31);
    // SAFETY: an initialised list, per the caller.
    let header = unsafe { &mut *list };
    if attribute & ADDITIVE == 0 {
        if header.count == header.capacity {
            return Err(ERROR_GEN_FAILURE);
        }
        if header.present & bit != 0 {
            return Err(ERROR_ALREADY_EXISTS);
        }
        if !previous.is_null() || flags & REPLACE_VALUE != 0 {
            return Err(ERROR_INVALID_PARAMETER);
        }
    }
    if attribute & INPUT != 0 && !return_size.is_null() {
        return Err(ERROR_INVALID_PARAMETER);
    }
    check_size(attribute, value, size)?;
    // SAFETY: entries follow the header, `capacity` of them.
    let entries = unsafe { list.cast::<u8>().add(HEADER).cast::<Entry>() };
    let (entry, add, stored) = if attribute == EXTENDED_FLAGS {
        // The flags live in the entry itself and accumulate unless replaced.
        let (entry, add, before) = if header.extended.is_null() {
            // SAFETY: a free entry, below.
            (unsafe { entries.add(header.count as usize) }, true, 0)
        } else {
            // SAFETY: the entry this list keeps for them.
            (header.extended, false, unsafe { (*header.extended).value }
                as u32)
        };
        // SAFETY: four bytes, as checked.
        let asked = unsafe { value.cast::<u32>().read_unaligned() };
        if asked & !7 != 0 {
            return Err(ERROR_INVALID_PARAMETER);
        }
        let flags_now = if flags & REPLACE_VALUE != 0 {
            asked
        } else {
            asked | before
        };
        if !previous.is_null() {
            // SAFETY: the title passes room for the flags before.
            unsafe { previous.cast::<u32>().write_unaligned(before) };
        }
        if add {
            header.extended = entry;
        }
        (entry, add, flags_now as usize as *mut c_void)
    } else {
        // SAFETY: as above; count < capacity unless additive.
        (unsafe { entries.add(header.count as usize) }, true, value)
    };
    // The console writes past a full list here (an additive attribute skips
    // the count check); this fails instead.
    if add && header.count >= header.capacity {
        return Err(ERROR_GEN_FAILURE);
    }
    // SAFETY: an entry of the list.
    unsafe {
        (*entry).value = stored;
        if add {
            (*entry).attribute = attribute;
            (*entry).size = size;
        }
    }
    if add {
        header.count += 1;
        header.present |= bit;
    }
    Ok(())
}

/// What a thread's attribute list asks for at its creation.
#[derive(Debug, Default, PartialEq, Eq)]
struct Placement {
    affinity: Option<[u8; 16]>,
    ideal: Option<u32>,
}

/// The checks the console's attribute translation (+0xce29c) makes for a
/// new thread: a sane header, each attribute present once and meant for
/// threads (group affinity, ideal processor, optional xstate features).
///
/// # Safety
/// `list` is null or a list the title made.
unsafe fn thread_attributes(list: *const Header) -> Result<Placement, u32> {
    let mut placement = Placement::default();
    if list.is_null() {
        return Ok(placement);
    }
    // SAFETY: per the caller.
    let header = unsafe { &*list };
    if header.count > header.capacity || header.count > MAX_ATTRIBUTES {
        return Err(ERROR_INVALID_PARAMETER);
    }
    let mut seen = 0u32;
    for i in 0..header.count as usize {
        // SAFETY: `count` entries follow the header.
        let entry = unsafe { &*list.cast::<u8>().add(HEADER + i * ENTRY).cast::<Entry>() };
        let bit = 1u32 << (entry.attribute & 31);
        if header.present & bit == 0 || seen & bit != 0 || entry.attribute & THREAD == 0 {
            return Err(ERROR_INVALID_PARAMETER);
        }
        seen |= bit;
        match (entry.attribute, entry.size) {
            (GROUP_AFFINITY, 0x10) => {
                // SAFETY: 16 bytes, as checked.
                placement.affinity =
                    Some(unsafe { entry.value.cast::<[u8; 16]>().read_unaligned() });
            }
            // SAFETY: 4 bytes, as checked.
            (IDEAL_PROCESSOR, 4) => {
                placement.ideal = Some(unsafe { entry.value.cast::<u32>().read_unaligned() })
            }
            // Not a thing this host has; the console only passes it on.
            (ENABLE_OPTIONAL_XSTATE_FEATURES, 8) => {}
            _ => return Err(ERROR_INVALID_PARAMETER),
        }
    }
    Ok(placement)
}

/// `CreateRemoteThreadEx` (+0xcda70): only `CREATE_SUSPENDED` and
/// `STACK_SIZE_PARAM_IS_A_RESERVATION`, and the attribute checks above. The
/// thread starts suspended here; its group affinity and ideal processor are
/// set before it runs, as the console's kernel sets them at creation.
pub extern "system" fn CreateRemoteThreadEx(
    process: *mut c_void,
    security: *mut c_void,
    stack: usize,
    start: *mut c_void,
    parameter: *mut c_void,
    flags: u32,
    list: *mut u8,
    id: *mut u32,
) -> *mut c_void {
    if flags & !(CREATE_SUSPENDED | STACK_SIZE_PARAM_IS_A_RESERVATION) != 0 {
        set_last_error(ERROR_INVALID_PARAMETER);
        return std::ptr::null_mut();
    }
    // SAFETY: the title passes a list it made, or null.
    let placement = match unsafe { thread_attributes(list.cast()) } {
        Ok(p) => p,
        Err(code) => {
            set_last_error(code);
            return std::ptr::null_mut();
        }
    };
    if let Some(ideal) = placement.ideal
        && !valid_ideal(ideal)
    {
        set_last_error(ERROR_INVALID_PARAMETER);
        return std::ptr::null_mut();
    }
    let create = host!("kernel32.dll", "CreateRemoteThreadEx": extern "system" fn(
        *mut c_void, *mut c_void, usize, *mut c_void, *mut c_void, u32, *mut u8, *mut u32,
    ) -> *mut c_void);
    let mut tid = 0u32;
    let thread = create(
        process,
        security,
        stack,
        start,
        parameter,
        flags | CREATE_SUSPENDED,
        std::ptr::null_mut(),
        &mut tid,
    );
    if thread.is_null() {
        return thread;
    }
    if let Some(affinity) = placement.affinity
        && let Err(code) = host_set_group_affinity(thread, &affinity)
    {
        // The console's kernel fails the creation; nothing of the thread ran.
        // SAFETY: our own suspended thread.
        unsafe {
            windows_sys::Win32::System::Threading::TerminateThread(thread, code);
            windows_sys::Win32::Foundation::CloseHandle(thread);
        }
        set_last_error(code);
        return std::ptr::null_mut();
    }
    if let Some(ideal) = placement.ideal {
        ideals().insert(tid, ideal);
    }
    if !id.is_null() {
        // SAFETY: the title passes room for the id.
        unsafe { id.write(tid) };
    }
    if flags & CREATE_SUSPENDED == 0 {
        // SAFETY: the thread we created suspended.
        unsafe { windows_sys::Win32::System::Threading::ResumeThread(thread) };
    }
    thread
}

fn host_set_group_affinity(thread: *mut c_void, affinity: &[u8; 16]) -> Result<(), u32> {
    let set = host!("kernel32.dll", "SetThreadGroupAffinity": extern "system" fn(*mut c_void, *const u8, *mut u8) -> i32);
    if set(thread, affinity.as_ptr(), std::ptr::null_mut()) != 0 {
        return Ok(());
    }
    // SAFETY: plain call.
    Err(unsafe { windows_sys::Win32::Foundation::GetLastError() })
}

/// Each thread's ideal processor (`PROCESSOR_NUMBER` as one dword: group,
/// number, reserved), by thread id, once set.
static IDEALS: Mutex<Option<HashMap<u32, u32>>> = Mutex::new(None);

struct Ideals(MutexGuard<'static, Option<HashMap<u32, u32>>>);

impl Ideals {
    fn insert(&mut self, tid: u32, ideal: u32) -> Option<u32> {
        self.0.get_or_insert_with(HashMap::new).insert(tid, ideal)
    }
    fn get(&self, tid: u32) -> Option<u32> {
        self.0.as_ref()?.get(&tid).copied()
    }
}

fn ideals() -> Ideals {
    Ideals(IDEALS.lock().unwrap_or_else(|e| e.into_inner()))
}

/// A processor the kernel takes as ideal: group 0 (the console has one),
/// a number below the active count. (The kernel's own check is not in the
/// reference; this is `KeSetIdealProcessorThreadEx`'s documented range.)
fn valid_ideal(ideal: u32) -> bool {
    let [group_lo, group_hi, number, _] = ideal.to_le_bytes();
    // Fixed for the process's life (a call into the Wine server otherwise).
    static ACTIVE: std::sync::OnceLock<u32> = std::sync::OnceLock::new();
    // SAFETY: plain call.
    let active = *ACTIVE.get_or_init(|| unsafe {
        windows_sys::Win32::System::Threading::GetActiveProcessorCount(0)
    });
    u16::from_le_bytes([group_lo, group_hi]) == 0 && u32::from(number) < active
}

fn thread_id(thread: *mut c_void) -> u32 {
    // The current thread's pseudo handle, as titles mostly pass: its id is
    // in the TEB; any other handle takes a call into the Wine server.
    if thread as isize == -2 {
        // SAFETY: plain call.
        return unsafe { windows_sys::Win32::System::Threading::GetCurrentThreadId() };
    }
    // SAFETY: plain call; 0 for a handle that is not a thread.
    unsafe { windows_sys::Win32::System::Threading::GetThreadId(thread) }
}

/// The ideal processor the kernel gave a thread we were never told of: the
/// host's answer (it has none to keep, so group 0, processor 0).
fn initial_ideal(thread: *mut c_void) -> u32 {
    let get = host!("kernel32.dll", "GetThreadIdealProcessorEx": extern "system" fn(*mut c_void, *mut u32) -> i32);
    let mut ideal = 0;
    if get(thread, &mut ideal) == 0 {
        return 0;
    }
    ideal
}

/// `SetThreadIdealProcessorEx` (+0x10e4e0): `NtSetInformationThread`
/// (`ThreadIdealProcessorEx`, 4 bytes) swaps in the new processor; on
/// success the previous one is written if asked for.
pub extern "system" fn SetThreadIdealProcessorEx(
    thread: *mut c_void,
    ideal: *const u32,
    previous: *mut u32,
) -> i32 {
    // SAFETY: the title passes a PROCESSOR_NUMBER.
    let asked = unsafe { ideal.read_unaligned() };
    let tid = thread_id(thread);
    if tid == 0 {
        set_last_error(ERROR_INVALID_HANDLE);
        return 0;
    }
    if !valid_ideal(asked) {
        set_last_error(ERROR_INVALID_PARAMETER);
        return 0;
    }
    let before = ideals()
        .insert(tid, asked)
        .unwrap_or_else(|| initial_ideal(thread));
    if !previous.is_null() {
        // SAFETY: the title passes room for a PROCESSOR_NUMBER.
        unsafe { previous.write_unaligned(before) };
    }
    1
}

/// `GetThreadIdealProcessorEx`: what [`SetThreadIdealProcessorEx`] set.
pub extern "system" fn GetThreadIdealProcessorEx(thread: *mut c_void, ideal: *mut u32) -> i32 {
    let tid = thread_id(thread);
    if tid == 0 {
        set_last_error(ERROR_INVALID_HANDLE);
        return 0;
    }
    let now = ideals().get(tid).unwrap_or_else(|| initial_ideal(thread));
    // SAFETY: the title passes room for a PROCESSOR_NUMBER.
    unsafe { ideal.write_unaligned(now) };
    1
}

pub(crate) fn module() -> HleModule {
    HleModule {
        name: "threads",
        dlls: &[
            "kernel32.dll",
            "kernelbase.dll",
            "api-ms-win-core-processthreads-*",
        ],
        exports: vec![
            hle_export!(InitializeProcThreadAttributeList),
            hle_export!(DeleteProcThreadAttributeList),
            hle_export!(UpdateProcThreadAttribute),
            hle_export!(CreateRemoteThreadEx),
            hle_export!(SetThreadIdealProcessorEx),
            hle_export!(GetThreadIdealProcessorEx),
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

    /// A list for `n` attributes, as the title makes one.
    fn list(n: u32) -> Vec<u64> {
        let mut size = 0;
        assert_eq!(
            InitializeProcThreadAttributeList(std::ptr::null_mut(), n, 0, &mut size),
            0
        );
        assert_eq!(
            (last_error(), size),
            (ERROR_INSUFFICIENT_BUFFER, (n as usize + 1) * 24)
        );
        let mut list = vec![0xAAAA_AAAA_AAAA_AAAAu64; size / 8];
        assert_eq!(
            InitializeProcThreadAttributeList(list.as_mut_ptr().cast(), n, 0, &mut size),
            1
        );
        list
    }

    fn update(
        list: &mut [u64],
        flags: u32,
        attribute: usize,
        value: *const c_void,
        size: usize,
    ) -> Result<(), u32> {
        let p = list.as_mut_ptr().cast();
        match UpdateProcThreadAttribute(
            p,
            flags,
            attribute,
            value.cast_mut(),
            size,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
        ) {
            1 => Ok(()),
            _ => Err(last_error()),
        }
    }

    /// InitializeProcThreadAttributeList (+0xffbe0): header written, flags
    /// and count checked before the buffer; Wine accepts flags and 32+.
    #[test]
    fn attribute_lists_start_as_the_console_makes_them() {
        let l = list(2);
        // present 0, capacity 2, count 0, (reserved untouched), extended null.
        assert_eq!(l[0], 2 << 32);
        assert_eq!(l[1] as u32, 0);
        assert_eq!(l[2], 0);
        let mut size = 4096;
        let mut buf = vec![0u8; 4096];
        assert_eq!(
            InitializeProcThreadAttributeList(buf.as_mut_ptr(), 1, 1, &mut size),
            0
        );
        assert_eq!((last_error(), size), (ERROR_INVALID_PARAMETER, 4096));
        assert_eq!(
            InitializeProcThreadAttributeList(buf.as_mut_ptr(), 32, 0, &mut size),
            0
        );
        assert_eq!(last_error(), ERROR_INVALID_PARAMETER);
        assert_eq!(
            InitializeProcThreadAttributeList(buf.as_mut_ptr(), 31, 0, &mut size),
            1
        );
        assert_eq!(size, 32 * 24);
    }

    /// UpdateProcThreadAttribute (+0xf6ee0): each check and its error.
    #[test]
    fn attributes_are_checked_as_the_console_checks_them() {
        let affinity = [0x3u64, 0];
        let ideal = 2u32;
        let mut l = list(2);
        assert_eq!(
            update(&mut l, 2, IDEAL_PROCESSOR, (&raw const ideal).cast(), 4),
            Err(ERROR_INVALID_PARAMETER)
        );
        assert_eq!(
            update(&mut l, 0, IDEAL_PROCESSOR, (&raw const ideal).cast(), 8),
            Err(ERROR_BAD_LENGTH)
        );
        assert_eq!(
            update(&mut l, 0, 0x2_0001, (&raw const ideal).cast(), 4),
            Err(ERROR_NOT_SUPPORTED)
        );
        assert_eq!(
            update(&mut l, 0, 0x2_0008, (&raw const ideal).cast(), 3),
            Err(ERROR_INVALID_PARAMETER)
        );
        assert_eq!(
            update(&mut l, 0, GROUP_AFFINITY, affinity.as_ptr().cast(), 16),
            Ok(())
        );
        // Wine: ERROR_PROCESS_MODE_ALREADY_BACKGROUND (698).
        assert_eq!(
            update(&mut l, 0, GROUP_AFFINITY, affinity.as_ptr().cast(), 16),
            Err(ERROR_ALREADY_EXISTS)
        );
        assert_eq!(
            update(&mut l, 1, IDEAL_PROCESSOR, (&raw const ideal).cast(), 4),
            Err(ERROR_INVALID_PARAMETER)
        );
        assert_eq!(
            update(&mut l, 0, IDEAL_PROCESSOR, (&raw const ideal).cast(), 4),
            Ok(())
        );
        assert_eq!(
            update(&mut l, 0, 0x2_000b, (&raw const ideal).cast(), 4),
            Err(ERROR_GEN_FAILURE)
        );
        // mask bits 3 and 5, count 2; entries {attribute, size, value}.
        assert_eq!(l[0], (2 << 32) | (1 << 3) | (1 << 5));
        assert_eq!(l[1] as u32, 2);
        assert_eq!(&l[3..5], &[GROUP_AFFINITY as u64, 16]);
        assert_eq!(l[5], affinity.as_ptr() as u64);
        assert_eq!(&l[6..8], &[IDEAL_PROCESSOR as u64, 4]);
        // An input attribute may not ask for a size back.
        let mut l = list(1);
        let mut back = 0usize;
        let p = l.as_mut_ptr().cast();
        let r = UpdateProcThreadAttribute(
            p,
            0,
            IDEAL_PROCESSOR,
            (&raw const ideal).cast_mut().cast(),
            4,
            std::ptr::null_mut(),
            &mut back,
        );
        assert_eq!((r, last_error()), (0, ERROR_INVALID_PARAMETER));
    }

    /// The extended flags (0x60001) accumulate in their one entry, and the
    /// flags before can be asked for.
    #[test]
    fn extended_flags_accumulate_in_one_entry() {
        let mut l = list(2);
        let (one, two) = (1u32, 4u32);
        assert_eq!(
            update(&mut l, 0, EXTENDED_FLAGS, (&raw const one).cast(), 4),
            Ok(())
        );
        let mut before = 0xFFFF_FFFFu32;
        let p = l.as_mut_ptr().cast();
        let r = UpdateProcThreadAttribute(
            p,
            0,
            EXTENDED_FLAGS,
            (&raw const two).cast_mut().cast(),
            4,
            (&raw mut before).cast(),
            std::ptr::null_mut(),
        );
        assert_eq!((r, before), (1, 1));
        assert_eq!(l[1] as u32, 1, "still one entry");
        assert_eq!(l[5], 5, "the flags themselves, or-ed");
        let eight = 8u32;
        assert_eq!(
            update(&mut l, 0, EXTENDED_FLAGS, (&raw const eight).cast(), 4),
            Err(ERROR_INVALID_PARAMETER)
        );
        assert_eq!(
            update(&mut l, 1, EXTENDED_FLAGS, (&raw const two).cast(), 4),
            Ok(())
        );
        assert_eq!(l[5], 4, "replaced");
    }

    extern "system" fn report(out: *mut c_void) -> u32 {
        let mut affinity = [0usize; 2];
        // SAFETY: plain calls on this thread.
        unsafe {
            let get = host!("kernel32.dll", "GetThreadGroupAffinity": extern "system" fn(*mut c_void, *mut usize) -> i32);
            get(
                windows_sys::Win32::System::Threading::GetCurrentThread(),
                affinity.as_mut_ptr(),
            );
            *out.cast::<usize>() = affinity[0];
        }
        0
    }

    fn wait(thread: *mut c_void) {
        // SAFETY: our thread.
        unsafe {
            windows_sys::Win32::System::Threading::WaitForSingleObject(thread, 5000);
            windows_sys::Win32::Foundation::CloseHandle(thread);
        }
    }

    /// CreateRemoteThreadEx (+0xcda70): flags beyond suspended/reservation
    /// fail (Wine creates the thread); a process attribute fails; group
    /// affinity and ideal processor hold from the thread's first instruction.
    #[test]
    fn threads_start_where_their_attributes_place_them() {
        // SAFETY: a pseudo handle.
        let me = unsafe { windows_sys::Win32::System::Threading::GetCurrentProcess() };
        let mut seen = 0usize;
        let out = (&raw mut seen).cast();
        let start = report as *mut c_void;
        for flags in [0x8, 0x8_0000] {
            let t = CreateRemoteThreadEx(
                me,
                std::ptr::null_mut(),
                0,
                start,
                out,
                flags,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
            );
            assert!(t.is_null());
            assert_eq!(last_error(), ERROR_INVALID_PARAMETER);
        }
        let parent = 0usize;
        let mut l = list(1);
        assert_eq!(
            update(&mut l, 0, 0x2_0000, (&raw const parent).cast(), 8),
            Ok(())
        );
        let t = CreateRemoteThreadEx(
            me,
            std::ptr::null_mut(),
            0,
            start,
            out,
            0,
            l.as_mut_ptr().cast(),
            std::ptr::null_mut(),
        );
        assert!(t.is_null());
        assert_eq!(last_error(), ERROR_INVALID_PARAMETER);

        let affinity = [0x6u64, 0];
        let ideal = 2u32 << 16; // group 0, processor 2
        let mut l = list(2);
        assert_eq!(
            update(&mut l, 0, GROUP_AFFINITY, affinity.as_ptr().cast(), 16),
            Ok(())
        );
        assert_eq!(
            update(&mut l, 0, IDEAL_PROCESSOR, (&raw const ideal).cast(), 4),
            Ok(())
        );
        let mut tid = 0;
        let t = CreateRemoteThreadEx(
            me,
            std::ptr::null_mut(),
            0,
            start,
            out,
            0,
            l.as_mut_ptr().cast(),
            &mut tid,
        );
        assert!(!t.is_null());
        let mut now = 0;
        assert_eq!(GetThreadIdealProcessorEx(t, &mut now), 1);
        assert_eq!(now, 2 << 16);
        wait(t);
        assert_eq!(seen, 0x6);
        assert_ne!(tid, 0);

        // Suspended threads stay so.
        seen = 0;
        let t = CreateRemoteThreadEx(
            me,
            std::ptr::null_mut(),
            0,
            start,
            out,
            CREATE_SUSPENDED,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
        );
        assert!(!t.is_null());
        std::thread::sleep(std::time::Duration::from_millis(50));
        assert_eq!(seen, 0);
        // SAFETY: our thread.
        unsafe { windows_sys::Win32::System::Threading::ResumeThread(t) };
        wait(t);
        assert_ne!(seen, 0);
    }

    /// SetThreadIdealProcessorEx (+0x10e4e0): succeeds and swaps (Wine:
    /// ERROR_CALL_NOT_IMPLEMENTED); a bad processor or handle fails.
    #[test]
    fn ideal_processors_are_kept() {
        std::thread::spawn(|| {
            // SAFETY: a pseudo handle.
            let me = unsafe { windows_sys::Win32::System::Threading::GetCurrentThread() };
            let mut before = 0xFFFF_FFFFu32;
            assert_eq!(SetThreadIdealProcessorEx(me, &(3 << 16), &mut before), 1);
            assert_eq!(before, 0, "the host's group 0, processor 0");
            assert_eq!(SetThreadIdealProcessorEx(me, &(1 << 16), &mut before), 1);
            assert_eq!(before, 3 << 16);
            assert_eq!(
                SetThreadIdealProcessorEx(me, &(1 << 16), std::ptr::null_mut()),
                1
            );
            assert_eq!(SetThreadIdealProcessorEx(me, &(200 << 16), &mut before), 0);
            assert_eq!(last_error(), ERROR_INVALID_PARAMETER);
            assert_eq!(SetThreadIdealProcessorEx(me, &1, &mut before), 0, "group 1");
            assert_eq!(before, 3 << 16, "untouched on failure");
            assert_eq!(
                SetThreadIdealProcessorEx(std::ptr::null_mut(), &0, &mut before),
                0
            );
            assert_eq!(last_error(), ERROR_INVALID_HANDLE);
        })
        .join()
        .unwrap();
    }
}
