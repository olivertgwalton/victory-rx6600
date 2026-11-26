//! `victory_unix.dll`, a Wine builtin that carries a Unix library of ours
//! (`tools/wine-unixlib`), and the calls into it: the kernel's write
//! tracking ([`crate::uffd`]) and title memory as a dma-buf
//! ([`crate::dmabuf`]). Found through `WINEDLLPATH`; absent on Windows and
//! where it is not installed, and then nothing of either is on.

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

/// The calls, by their index in the library's table.
pub(crate) const UFFD_INIT: u32 = 0;
pub(crate) const UFFD_TRACK: u32 = 1;
pub(crate) const UFFD_UNTRACK: u32 = 2;
pub(crate) const UFFD_SCAN: u32 = 3;
pub(crate) const DMABUF: u32 = 4;
pub(crate) const GTT: u32 = 5;

pub(crate) struct Lib {
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
    /// Calls `code` with `args`, in the layout the Unix side reads and
    /// writes for it; whether it succeeded.
    pub(crate) fn call(&self, code: u32, args: &mut [u64]) -> bool {
        // SAFETY: ntdll's dispatcher with our library's handle; `args` is
        // the caller's layout for `code`.
        unsafe { (*self.dispatcher)(self.handle, code, args.as_mut_ptr().cast()) == 0 }
    }
}

/// The library, loaded at the first call; `None` if not under Wine or not
/// found.
pub(crate) fn lib() -> Option<&'static Lib> {
    static LIB: OnceLock<Option<Lib>> = OnceLock::new();
    LIB.get_or_init(|| {
        if !crate::wine_dxc::under_wine() {
            return None;
        }
        // SAFETY: loading our builtin and asking ntdll for its Unix library;
        // the export and the query are Wine's own (Wine 9 and later).
        unsafe {
            let Ok(dll) = LoadLibraryW(w!("victory_unix.dll")) else {
                tracing::info!("victory_unix.dll not found (WINEDLLPATH): no write tracking in the kernel, no dma-bufs");
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
                tracing::warn!(status = format!("{status:#x}"), "victory_unix.dll has no Unix library");
                return None;
            }
            Some(Lib { handle, dispatcher })
        }
    })
    .as_ref()
}
