//! Write tracking in the kernel, under Wine: `victory_unix.dll` carries a
//! Unix library (`tools/wine-unixlib`) that registers title memory with a
//! userfaultfd in asynchronous write-protect mode and asks `PAGEMAP_SCAN`
//! which pages were written, protecting them again in the same step (the
//! two were added to Linux for `GetWriteWatch`). It works for private
//! memory and for section views alike, with no fault reaching user mode: in
//! place of the write watch Wine refuses for views (3,000 to 4,000 failed
//! askings a frame, 0.4 to 0.9 s of threads) and of our guards (0.3 to 0.5
//! s of faults). See `crate::watch` for how answers become the journal.
//!
//! Each view of a page has its own page table entries, so a write through
//! one view is seen at that view only: `crate::watch::kernel_track`
//! hands it pages shown at one address alone. Off unless `VICTORY_UFFD=1`
//! (and the library is found, through `WINEDLLPATH`).

use std::ffi::c_void;
use std::sync::OnceLock;

use windows::Win32::Foundation::HANDLE;
use windows::Win32::System::LibraryLoader::{GetModuleHandleW, GetProcAddress, LoadLibraryW};
use windows::Win32::System::Threading::GetCurrentProcess;
use windows::core::{s, w};

type Dispatcher = unsafe extern "system" fn(u64, u32, *mut c_void) -> i32;
type Query = unsafe extern "system" fn(HANDLE, *const c_void, u32, *mut c_void, usize, *mut usize) -> i32;

/// `MemoryWineUnixFuncs`: the Unix library of a builtin module.
const MEMORY_WINE_UNIX_FUNCS: u32 = 1000;
const INIT: u32 = 0;
const TRACK: u32 = 1;
const UNTRACK: u32 = 2;
const SCAN: u32 = 3;

struct Lib {
    handle: u64,
    /// ntdll's `__wine_unix_call_dispatcher`, a pointer to the function.
    dispatcher: *const Dispatcher,
}

// SAFETY: the handle and ntdll's dispatcher are process-wide and the Unix
// side takes no lock of ours.
unsafe impl Send for Lib {}
// SAFETY: as above.
unsafe impl Sync for Lib {}

impl Lib {
    fn call(&self, code: u32, args: &mut [u64]) -> bool {
        // SAFETY: ntdll's dispatcher with our library's handle; `args` is
        // the layout the Unix side reads and writes for `code`.
        unsafe { (*self.dispatcher)(self.handle, code, args.as_mut_ptr().cast()) == 0 }
    }
}

fn lib() -> Option<&'static Lib> {
    static LIB: OnceLock<Option<Lib>> = OnceLock::new();
    LIB.get_or_init(|| {
        if std::env::var_os("VICTORY_UFFD").is_none_or(|v| v != "1") || !crate::wine_dxc::under_wine() {
            return None;
        }
        // SAFETY: loading our builtin and asking ntdll for its Unix library;
        // the export and the query are Wine's own (Wine 9 and later).
        let lib = unsafe {
            let Ok(dll) = LoadLibraryW(w!("victory_unix.dll")) else {
                tracing::info!("write tracking in the kernel: victory_unix.dll not found (WINEDLLPATH); the write watch and guards answer");
                return None;
            };
            let nt = GetModuleHandleW(w!("ntdll.dll")).ok()?;
            let query: Query = std::mem::transmute(GetProcAddress(nt, s!("NtQueryVirtualMemory"))?);
            let dispatcher = GetProcAddress(nt, s!("__wine_unix_call_dispatcher"))? as *const Dispatcher;
            let mut handle = 0u64;
            let status = query(
                GetCurrentProcess(),
                dll.0 as *const c_void,
                MEMORY_WINE_UNIX_FUNCS,
                (&raw mut handle).cast(),
                size_of::<u64>(),
                std::ptr::null_mut(),
            );
            if status != 0 {
                tracing::warn!(status = format!("{status:#x}"), "write tracking in the kernel: victory_unix.dll has no Unix library");
                return None;
            }
            Lib { handle, dispatcher }
        };
        let mut a = [0u64; 1];
        if !lib.call(INIT, &mut a) {
            tracing::warn!(error = a[0] as i64, "write tracking in the kernel: no userfaultfd (vm.unprivileged_userfaultfd, or an older kernel)");
            return None;
        }
        tracing::info!(features = format!("{:#x}", a[0]), "write tracking in the kernel: userfaultfd write-protect and PAGEMAP_SCAN");
        Some(lib)
    })
    .as_ref()
}

/// Whether the kernel tracks writes for us.
pub(crate) fn on() -> bool {
    lib().is_some()
}

/// Registers `[start, start + len)` and protects it: writes from now on are
/// reported by [`scan`]. False if the kernel refuses (memory it cannot
/// track, or already registered by another userfaultfd).
pub(crate) fn track(start: usize, len: usize) -> bool {
    let Some(lib) = lib() else { return false };
    let mut a = [start as u64, len as u64, 0];
    lib.call(TRACK, &mut a)
}

/// Stops tracking `[start, start + len)`.
#[allow(dead_code)]
pub(crate) fn untrack(start: usize, len: usize) {
    if let Some(lib) = lib() {
        let mut a = [start as u64, len as u64, 0];
        lib.call(UNTRACK, &mut a);
    }
}

/// The runs `[start, end)` of 4 KiB pages written in `[start, end)` since
/// the last scan of them (or their registration), appended to `out`; the
/// pages are protected again. False if the kernel refused.
pub(crate) fn scan(start: usize, end: usize, out: &mut Vec<(usize, usize)>) -> bool {
    let Some(lib) = lib() else { return false };
    let mut vec = [[0u64; 3]; 512];
    let mut at = start;
    while at < end {
        let mut a = [at as u64, end as u64, vec.as_mut_ptr() as u64, vec.len() as u64, 0, 0];
        if !lib.call(SCAN, &mut a) {
            return false;
        }
        let n = a[4] as i64;
        if n < 0 {
            return false;
        }
        out.extend(vec[..n as usize].iter().map(|r| (r[0] as usize, r[1] as usize)));
        let walked = a[5] as usize;
        if walked <= at || n < vec.len() as i64 && walked >= end {
            break;
        }
        at = walked;
    }
    true
}
