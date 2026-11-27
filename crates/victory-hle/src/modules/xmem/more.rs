//! The `xmem.dll` exports the title does not import: options, write-watch
//! passthroughs, the heaps of a memory type (hysteresis, preallocation,
//! statistics), Win32 heaps and their names, tags, the graphics memory
//! configuration, statistics and the two exports `xmem.dll` forwards to
//! `xbthunk.dll`. Each follows the function of `xmem.dll` named beside it;
//! `tests.rs` replays what the console's library did with the same calls.
#![allow(non_snake_case)]
#![allow(clippy::not_unsafe_ptr_arg_deref)]

use std::collections::HashMap;
use std::ffi::c_void;
use std::sync::Mutex;

use windows_sys::Win32::Foundation::HANDLE;
use windows_sys::core::BOOL;

use super::spec::{self, ERROR_INVALID_PARAMETER};
use super::{env, env_mut, set_last_error, with_heap, BLOCKS};

/// What `XMemSetOption` set that [`spec::Env`] does not hold.
#[derive(Default)]
struct Options {
    /// Option 0x10000 (`xmem!0x1800108f8`, `xmem!0x180010988`): write
    /// watching suspended, and the value that keeps it so.
    write_watch_suspended: bool,
    write_watch_value: u32,
    /// Option 1 (`xmem!0x180010940`): `XMemAlloc` calls validated.
    validate: u32,
    /// Option 8 (`xmem!0x180010790`): history buffer, bytes.
    history: u64,
    /// Option 9 (`xmem!0x180010798`).
    option9: bool,
    /// Option 4 (`xmem!0x1800108e0`).
    option4: u32,
    /// Option 3 and 7: passed to `xbthunk.dll` on the console.
    realloc_free_callback: u32,
    writeable_mapped_files: u32,
}

static OPTIONS: Mutex<Options> = Mutex::new(Options {
    write_watch_suspended: false,
    write_watch_value: 0,
    validate: 0,
    history: 0,
    option9: false,
    option4: 0,
    realloc_free_callback: 0,
    writeable_mapped_files: 0,
});

fn options() -> std::sync::MutexGuard<'static, Options> {
    OPTIONS.lock().unwrap_or_else(|e| e.into_inner())
}

/// Whether any heap of a memory type exists (`xmem!0x1800108e8`): options
/// 4 and 6 change nothing after.
fn heaps_made() -> bool {
    BLOCKS.iter().any(|h| h.lock().unwrap_or_else(|e| e.into_inner()).is_some())
}

/// `XMemSetOption` (`xmem!0x180002fd0`): `{u32 option, u32, u64 value}`.
/// Every check runs in turn; an unknown option changes nothing.
pub extern "system" fn XMemSetOption(option: *const u64) {
    if option.is_null() {
        return;
    }
    // SAFETY: the title passes the 16-byte structure.
    let (id, value) = unsafe { (option.cast::<u32>().read(), option.add(1).read()) };
    let v32 = value as u32;
    let mut o = options();
    match id {
        1 => o.validate = v32,
        0x10001 => env_mut().graphics_write_watch = v32,
        3 => o.realloc_free_callback = v32,
        6 => {
            // 2 MiB chunks for value 0, 512 KiB otherwise; once a heap exists
            // E_NOTIMPL (logged only).
            if heaps_made() {
                tracing::info!("XMemSetOption 6 after a heap exists: not implemented, as on the console");
            } else {
                env_mut().large_chunks = (v32 == 0) as u32;
                if v32 != 0 {
                    o.option4 = 1;
                }
            }
        }
        8 => {
            if o.history == 0 {
                o.history = value << 20;
            }
        }
        2 => env_mut().tag_allocations = v32,
        0x10000 => {
            if v32 != 0 {
                o.write_watch_suspended = true;
            }
            o.write_watch_value = v32;
        }
        4 => {
            if heaps_made() {
                tracing::info!("XMemSetOption 4 after a heap exists: not implemented, as on the console");
            } else {
                o.option4 = v32;
            }
        }
        5 => env_mut().all_virtual = v32,
        7 => o.writeable_mapped_files = v32,
        9 => o.option9 = true,
        _ => {}
    }
}

const ERROR_CAN_NOT_COMPLETE: u32 = 1003;

/// `XMemGetWriteWatch` (`xmem!0x180002b40`): the host's, unless option
/// 0x10000 suspended it (`ERROR_CAN_NOT_COMPLETE`, returned).
/// note: the layer's GPU side watches title memory itself; a title that
/// resets write watching here also resets what it saw.
pub extern "system" fn XMemGetWriteWatch(
    flags: u32,
    base: *mut c_void,
    size: usize,
    addresses: *mut *mut c_void,
    count: *mut usize,
    granularity: *mut u32,
) -> u32 {
    if options().write_watch_suspended {
        return ERROR_CAN_NOT_COMPLETE;
    }
    // SAFETY: the title's own arguments.
    unsafe { windows_sys::Win32::System::Memory::GetWriteWatch(flags, base, size, addresses, count, granularity) }
}

/// `XMemResetWriteWatch` (`xmem!0x180002f30`).
pub extern "system" fn XMemResetWriteWatch(base: *mut c_void, size: usize) -> u32 {
    if options().write_watch_suspended {
        return ERROR_CAN_NOT_COMPLETE;
    }
    // SAFETY: the title's own arguments.
    unsafe { windows_sys::Win32::System::Memory::ResetWriteWatch(base, size) }
}

/// `XMemResumeWriteWatch` (`xmem!0x180002f50`): resumes unless option
/// 0x10000's value still holds it.
pub extern "system" fn XMemResumeWriteWatch() -> BOOL {
    let mut o = options();
    if o.write_watch_value != 0 {
        return 0;
    }
    o.write_watch_suspended = false;
    1
}

/// `XMemMakeTag` (`xmem!0x180009a20`).
pub extern "system" fn XMemMakeTag(tag: u32) -> u64 {
    spec::make_tag(tag)
}

/// The heap index of a memory type (`type` 0-15) and unit (`medium`
/// nonzero: 4 KiB units), or None (ERROR_INVALID_PARAMETER) for another
/// type.
fn index(t: u32, medium: u32) -> Option<usize> {
    if t & !0xf != 0 {
        return None;
    }
    Some(t as usize | if medium != 0 { 0x10 } else { 0 })
}

/// `XMemGetAllocationHysteresis` (`xmem!0x180005930`): bytes; the heap is
/// made if it was not; all ones (and ERROR_INVALID_PARAMETER for a bad
/// type) when there is none.
pub extern "system" fn XMemGetAllocationHysteresis(t: u32, medium: u32) -> u64 {
    let Some(i) = index(t, medium) else {
        set_last_error(ERROR_INVALID_PARAMETER);
        return u64::MAX;
    };
    match with_heap(i, |h| h.hysteresis as u64 * h.shape.chunk) {
        Some(bytes) => {
            set_last_error(0);
            bytes
        }
        None => u64::MAX,
    }
}

/// `XMemSetAllocationHysteresis` (`xmem!0x180005b60`).
pub extern "system" fn XMemSetAllocationHysteresis(t: u32, medium: u32, bytes: u64) -> BOOL {
    let Some(i) = index(t, medium) else {
        set_last_error(ERROR_INVALID_PARAMETER);
        return 0;
    };
    if with_heap(i, |h| h.set_hysteresis(bytes)).is_none() {
        return 0;
    }
    set_last_error(0);
    1
}

/// `XMemPreallocateFreeSpace` (`xmem!0x180005a70`).
pub extern "system" fn XMemPreallocateFreeSpace(t: u32, medium: u32) -> BOOL {
    let Some(i) = index(t, medium) else {
        set_last_error(ERROR_INVALID_PARAMETER);
        return 0;
    };
    match with_heap(i, |h| h.preallocate()) {
        Some(true) => {
            set_last_error(0);
            1
        }
        _ => 0,
    }
}

/// `XMemGetAllocationStatistics`' structure.
#[repr(C)]
pub struct AllocationStatistics {
    /// 0x20, set by the caller.
    pub size: u32,
    /// Bytes a heap commits at a time.
    pub chunk: u32,
    /// Bytes committed.
    pub committed: u64,
    /// Bytes committed without blocks.
    pub free: u64,
    /// Chunk commits and decommits so far.
    pub commits: u32,
    pub decommits: u32,
}

const ERROR_BAD_LENGTH: u32 = 24;

/// `XMemGetAllocationStatistics` (`xmem!0x1800059a0`): the structure's
/// size must be set (ERROR_BAD_LENGTH); a bad type is
/// ERROR_INVALID_PARAMETER and zeroes, but still succeeds (the console
/// clears the error after); a heap not made yet reads zero.
pub extern "system" fn XMemGetAllocationStatistics(t: u32, medium: u32, out: *mut AllocationStatistics) -> BOOL {
    if out.is_null() {
        return 0;
    }
    // SAFETY: the title passes the structure.
    let out = unsafe { &mut *out };
    if out.size != 0x20 {
        set_last_error(ERROR_BAD_LENGTH);
        return 0;
    }
    let chunk = if env().large_chunks != 0 { 2 << 20 } else { 512 << 10 };
    out.chunk = chunk;
    let found = match index(t, medium) {
        None => {
            set_last_error(ERROR_INVALID_PARAMETER);
            None
        }
        Some(i) => BLOCKS[i].lock().unwrap_or_else(|e| e.into_inner()).as_ref().map(|h| {
            (h.committed, h.free_chunks as u64 * h.shape.chunk, h.commits, h.decommits)
        }),
    };
    let (committed, free, commits, decommits) = found.unwrap_or_default();
    out.committed = committed;
    out.free = free;
    out.commits = commits;
    out.decommits = decommits;
    set_last_error(0);
    1
}

/// The partition heaps' names (`HeapSetInformation` class 7 on the
/// console: 8 bytes).
static HEAP_NAMES: Mutex<Option<HashMap<usize, [u8; 8]>>> = Mutex::new(None);

/// `XMemSetWin32HeapName` (`xmem!0x180003140`): the first 8 bytes of the
/// name (no more than `len`).
pub extern "system" fn XMemSetWin32HeapName(heap: HANDLE, name: *const u8, len: usize) {
    let mut buf = [0u8; 8];
    if !name.is_null() {
        // SAFETY: the title passes `len` bytes.
        unsafe { std::ptr::copy_nonoverlapping(name, buf.as_mut_ptr(), len.min(8)) };
    }
    let error = super::last_error();
    HEAP_NAMES.lock().unwrap_or_else(|e| e.into_inner()).get_or_insert_with(Default::default).insert(heap as usize, buf);
    set_last_error(error);
}

/// `XMemGetWin32HeapName` (`xmem!0x180002850`): the name up to its NUL and
/// `len - 1` bytes, NUL-terminated, and its length with the NUL. No room:
/// ERROR_INVALID_PARAMETER.
pub extern "system" fn XMemGetWin32HeapName(heap: HANDLE, out: *mut u8, len: usize, written: *mut usize) -> BOOL {
    if len == 0 {
        set_last_error(ERROR_INVALID_PARAMETER);
        return 0;
    }
    let name = HEAP_NAMES
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .as_ref()
        .and_then(|m| m.get(&(heap as usize)).copied())
        .unwrap_or_default();
    let n = name.iter().position(|&b| b == 0).unwrap_or(8).min(len - 1);
    // SAFETY: the title passes `len` bytes of room.
    unsafe {
        std::ptr::copy_nonoverlapping(name.as_ptr(), out, n);
        out.add(n).write(0);
        if !written.is_null() {
            written.write(n + 1);
        }
    }
    1
}

/// `XMemGetWin32HeapStatistics` (`xmem!0x180002940`): two figures of the
/// heap, from the system's heap walk: committed and allocated bytes.
/// note: the console's figures come from its heap manager's walk; these
/// from the host's (`HeapSummary`).
pub extern "system" fn XMemGetWin32HeapStatistics(heap: HANDLE, out: *mut [u64; 2]) -> BOOL {
    use windows_sys::Win32::System::Memory::{HEAP_SUMMARY, HeapSummary};
    let mut summary = HEAP_SUMMARY { cb: size_of::<HEAP_SUMMARY>() as u32, ..unsafe { std::mem::zeroed() } };
    // SAFETY: the title's heap and a sized structure.
    if unsafe { HeapSummary(heap, 0, &mut summary) } == 0 {
        return 0;
    }
    // SAFETY: the title passes room for two figures.
    unsafe { out.write([summary.cbCommitted as u64, summary.cbAllocated as u64]) };
    1
}

/// `XMemGetTitleHeap` (`xmem!0x180002840`): the title partition's heap,
/// the process heap here.
pub extern "system" fn XMemGetTitleHeap() -> HANDLE {
    // SAFETY: plain query.
    unsafe { windows_sys::Win32::System::Memory::GetProcessHeap() }
}

/// `XMemCreateWin32Heap` (`xmem!0x180002290`): flags (bit 15 ignored) 0-7;
/// bit 1 a heap over the committed memory at `base` (4 KiB-aligned, bit 1
/// alone), else a growable partition heap, serialized with bit 2, of the
/// tools partition with bit 0 (profiling mode only: refused without an
/// error).
pub extern "system" fn XMemCreateWin32Heap(flags: u64, base: *mut c_void) -> HANDLE {
    use windows_sys::Win32::System::Memory::{HEAP_NO_SERIALIZE, HeapCreate};
    let f = flags & !0x8000;
    if f > 7 {
        set_last_error(ERROR_INVALID_PARAMETER);
        return std::ptr::null_mut();
    }
    if f & 2 != 0 {
        if base.is_null() || base as usize & 0xfff != 0 || f != 2 {
            set_last_error(ERROR_INVALID_PARAMETER);
            return std::ptr::null_mut();
        }
        return heap_at(base);
    }
    if f & 1 != 0 && !env().devkit_tools() && !env().profiling(flags >> 15 & 1 != 0) {
        tracing::info!("XMemCreateWin32Heap: tools heap without profiling mode");
        return std::ptr::null_mut();
    }
    // SAFETY: plain heap creation.
    unsafe { HeapCreate(if f & 4 != 0 { 0 } else { HEAP_NO_SERIALIZE }, 0, 0) }
}

/// A heap over the committed region at `base`, to its end (`RtlCreateHeap`
/// with a base, as the console). Uncommitted: null, the error left as it
/// was.
fn heap_at(base: *mut c_void) -> HANDLE {
    use windows_sys::Win32::System::LibraryLoader::{GetModuleHandleA, GetProcAddress};
    use windows_sys::Win32::System::Memory::{MEM_COMMIT, MEMORY_BASIC_INFORMATION, VirtualQuery};
    // SAFETY: plain data filled in by the query.
    let mut info: MEMORY_BASIC_INFORMATION = unsafe { std::mem::zeroed() };
    // SAFETY: a valid out pointer.
    if unsafe { VirtualQuery(base, &mut info, size_of_val(&info)) } == 0 || info.State != MEM_COMMIT {
        return std::ptr::null_mut();
    }
    let len = info.BaseAddress as usize + info.RegionSize - base as usize;
    type RtlCreateHeap = unsafe extern "system" fn(u32, *mut c_void, usize, usize, *mut c_void, *mut c_void) -> HANDLE;
    // SAFETY: ntdll is always loaded; the routine has this type.
    unsafe {
        let ntdll = GetModuleHandleA(c"ntdll.dll".as_ptr().cast());
        let Some(f) = GetProcAddress(ntdll, c"RtlCreateHeap".as_ptr().cast()) else {
            return std::ptr::null_mut();
        };
        let create: RtlCreateHeap = std::mem::transmute(f);
        create(0, base, len, len, std::ptr::null_mut(), std::ptr::null_mut())
    }
}

/// Routines `XMemSetWin32HeapTrackingHooks` sets (`xmem!0x180003190`):
/// after a process-heap allocation (heap, flags, size, block), a reserved
/// one, after a process-heap free (heap, flags, block).
pub(crate) static TRACKING: Mutex<[usize; 3]> = Mutex::new([0; 3]);

pub extern "system" fn XMemSetWin32HeapTrackingHooks(alloc: usize, other: usize, free: usize) {
    *TRACKING.lock().unwrap_or_else(|e| e.into_inner()) = [alloc, other, free];
}

/// The graphics memory configuration (`xmem!0x180010920`), kept from the
/// first call of `XMemRegisterGraphicsMemoryConfiguration`
/// (`xmem!0x180002f00`): 24 bytes; `XMemGetWorkingSetStatistics` reports
/// its first two words.
pub(crate) static GRAPHICS_CONFIGURATION: Mutex<Option<[u8; 24]>> = Mutex::new(None);

pub extern "system" fn XMemRegisterGraphicsMemoryConfiguration(configuration: *const [u8; 24]) {
    let mut c = GRAPHICS_CONFIGURATION.lock().unwrap_or_else(|e| e.into_inner());
    if c.is_none() && !configuration.is_null() {
        // SAFETY: the title passes the structure.
        *c = Some(unsafe { configuration.read() });
    }
}

/// `XMemGetPhysicalMemoryLayout` (`xmem!0x1800026d0`): 16 bytes the
/// console's memory driver gives (device query 0x1500d4).
/// note: the driver's figures are not in the library; the layer has no
/// physical layout to give and reports zeroes, as the driver of the
/// harness did.
pub extern "system" fn XMemGetPhysicalMemoryLayout(out: *mut [u8; 16]) -> BOOL {
    if out.is_null() {
        return 0;
    }
    // SAFETY: the title passes 16 bytes.
    unsafe { out.write([0; 16]) };
    1
}

/// `XMemGetFreePageStatistics` (`xmem!0x1800085d0`): seven figures of the
/// title partition's free pages (0x38 bytes; the rest of the caller's
/// structure is left alone).
/// note: the figures are the kernel's (`NtManagePartition`); the layer
/// reports the host's free memory in the first and zeroes elsewhere.
pub extern "system" fn XMemGetFreePageStatistics(out: *mut [u64; 7]) -> BOOL {
    use windows_sys::Win32::System::SystemInformation::{GlobalMemoryStatusEx, MEMORYSTATUSEX};
    if out.is_null() {
        return 0;
    }
    let mut status = MEMORYSTATUSEX { dwLength: size_of::<MEMORYSTATUSEX>() as u32, ..unsafe { std::mem::zeroed() } };
    // SAFETY: a sized structure.
    unsafe { GlobalMemoryStatusEx(&mut status) };
    // SAFETY: the title passes the structure.
    unsafe { out.write([status.ullAvailPhys >> 12, 0, 0, 0, 0, 0, 0]) };
    1
}

/// `XMemDumpMemoryStats` (`xmem!0x180008540`): prints statistics when both
/// pointers are set; logged here.
pub extern "system" fn XMemDumpMemoryStats(a: *mut c_void, b: *mut c_void) {
    if a.is_null() || b.is_null() {
        return;
    }
    let mut stats = super::WorkingSet::default();
    super::XMemGetWorkingSetStatistics(0x1000_000f, &mut stats);
    tracing::info!(?stats, "XMemDumpMemoryStats");
}

/// `XMemHistoryData` (`xmem!0x1800107a8`): a data export, 0x38 bytes.
pub static mut XMEM_HISTORY_DATA: [u64; 7] = [0; 7];

/// `XMemGetPartitionHandleAndId`, forwarded to `xbthunk.dll`'s
/// `XbGetPartitionHandleAndId` (partition, handle out, id out): the title
/// partition (1) only on a retail console.
/// note: `xbthunk.dll`'s own answer was not compared; the title partition's
/// handle is the process's, its id 1.
pub extern "system" fn XMemGetPartitionHandleAndId(partition: u32, handle: *mut HANDLE, id: *mut u32) -> u8 {
    if partition != 1 {
        return 0;
    }
    // SAFETY: the caller's out pointers, when given.
    unsafe {
        if !handle.is_null() {
            handle.write(std::ptr::null_mut());
        }
        if !id.is_null() {
            id.write(1);
        }
    }
    1
}

/// `XMemSetWinHttpHooks`, forwarded to `xbthunk.dll`'s `XbSetWinHttpHooks`:
/// kept; nothing in the layer calls them.
pub(crate) static WIN_HTTP_HOOKS: Mutex<[usize; 4]> = Mutex::new([0; 4]);

pub extern "system" fn XMemSetWinHttpHooks(a: usize, b: usize, c: usize, d: usize) {
    *WIN_HTTP_HOOKS.lock().unwrap_or_else(|e| e.into_inner()) = [a, b, c, d];
}

pub(crate) fn exports() -> Vec<crate::HleExport> {
    use crate::hle_export;
    vec![
        hle_export!(XMemSetOption),
        hle_export!(XMemGetWriteWatch),
        hle_export!(XMemResetWriteWatch),
        hle_export!(XMemResumeWriteWatch),
        hle_export!(XMemMakeTag),
        hle_export!(XMemGetAllocationHysteresis),
        hle_export!(XMemSetAllocationHysteresis),
        hle_export!(XMemPreallocateFreeSpace),
        hle_export!(XMemGetAllocationStatistics),
        hle_export!(XMemSetWin32HeapName),
        hle_export!(XMemGetWin32HeapName),
        hle_export!(XMemGetWin32HeapStatistics),
        hle_export!(XMemGetTitleHeap),
        hle_export!(XMemCreateWin32Heap),
        hle_export!(XMemSetWin32HeapTrackingHooks),
        hle_export!(XMemRegisterGraphicsMemoryConfiguration),
        hle_export!(XMemGetPhysicalMemoryLayout),
        hle_export!(XMemGetFreePageStatistics),
        hle_export!(XMemDumpMemoryStats),
        hle_export!(XMemGetPartitionHandleAndId),
        hle_export!(XMemSetWinHttpHooks),
        crate::HleExport { name: "XMemHistoryData", address: (&raw const XMEM_HISTORY_DATA) as usize },
    ]
}

/// The state a fresh process starts with (tests).
#[cfg(test)]
pub(crate) fn reset() {
    *options() = Options::default();
    *HEAP_NAMES.lock().unwrap_or_else(|e| e.into_inner()) = None;
    *TRACKING.lock().unwrap_or_else(|e| e.into_inner()) = [0; 3];
    *GRAPHICS_CONFIGURATION.lock().unwrap_or_else(|e| e.into_inner()) = None;
}
