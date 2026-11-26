//! Title memory for the GPU with no copy, under Wine: the pages of a section
//! (the console's memory is sections of ours) handed to the card as a
//! dma-buf, and a heap opened on them, so a buffer placed there is the
//! title's bytes themselves.
//!
//! The way there, each piece of `tools/`:
//! - wineserver makes section memfds sealable (`tools/wine-dmabuf`);
//! - `victory_unix` (`tools/wine-unixlib`) seals the section's memfd
//!   against shrinking, makes a udmabuf of the range and wraps its fd in a
//!   handle ([`export`]);
//! - win32u's `vkAllocateMemory` imports such a handle as a dma-buf
//!   (`tools/wine-dmabuf`), and vkd3d-proton's
//!   `OpenSharedHandle(IID_ID3D12Heap)` makes a heap of it
//!   (`tools/vkd3d-proton`) ([`open_heap`]).
//!
//! What it costs, measured on the RX 6600 XT (`~/victory/uma-probe`): a
//! submission referencing four 64 MiB imports 3.7 us more than one
//! referencing none (host-pointer imports: 399 us); the card reads an import
//! at 14.4 GB/s, against 247 from its own memory, so only what the card
//! reads once or the CPU reads back belongs here. udmabuf pins the pages,
//! which count against GTT until the heap goes: import parts, never the
//! whole of title memory. Never decommit or punch a hole in a section
//! while a heap of it stands: the card would keep the old pages.
//!
//! `VICTORY_DMABUF=1` is the switch for using it ([`on`]);
//! `VICTORY_INPLACE_PROBE` tries it on memory of its own
//! (`crate::inplace::probe`).

use windows::Win32::Foundation::{CloseHandle, HANDLE};
use windows::Win32::Graphics::Direct3D12::{ID3D12Device, ID3D12Heap};

/// Whether title memory is handed to the card as dma-bufs
/// (`VICTORY_DMABUF=1`, and the library is there).
#[allow(dead_code)]
pub(crate) fn on() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("VICTORY_DMABUF").is_some_and(|v| v == "1") && crate::unixlib::lib().is_some())
}

/// Why [`export`] failed: the step, and the error (a negative errno, or
/// an NTSTATUS where Wine answered).
#[derive(Clone, Copy, Debug)]
pub(crate) struct Refused {
    step: u64,
    error: i64,
}

impl std::fmt::Display for Refused {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let e = self.error;
        match self.step {
            0 if e == -38 => write!(f, "no victory_unix.dll (WINEDLLPATH), or not under Wine"),
            0 => write!(f, "the range is not whole pages"),
            1 => write!(f, "the section's memory: no fd (NTSTATUS {:#x}; a file-backed section?)", e as u32),
            2 => write!(f, "sealing its memfd: errno {} (wineserver without tools/wine-dmabuf makes memfds that cannot be sealed)", -e),
            3 => write!(f, "/dev/udmabuf: errno {} (no such device, or no access to it: the kvm group)", -e),
            4 => write!(
                f,
                "UDMABUF_CREATE: errno {} ({})",
                -e,
                match -e {
                    22 => "the range is not inside the memfd, or not whole pages",
                    7 => "larger than size_limit_mb",
                    12 | 14 => "the pages could not be pinned",
                    _ => "see udmabuf.c",
                }
            ),
            5 => write!(f, "the handle: NTSTATUS {:#x}", e as u32),
            s => write!(f, "step {s}: {e}"),
        }
    }
}

/// A handle to a dma-buf: closed when dropped (what was opened from it
/// holds its own reference).
pub(crate) struct Exported(HANDLE);

impl Drop for Exported {
    fn drop(&mut self) {
        // SAFETY: our handle, closed once.
        unsafe {
            let _ = CloseHandle(self.0);
        }
    }
}

/// A dma-buf of `[offset, offset + len)` of `section`'s memory (a section
/// handle of ours, made with `SEC_COMMIT`; whole 4 KiB pages). The memfd is
/// sealed against shrinking from now on, which Wine's sections never do.
pub(crate) fn export(section: usize, offset: u64, len: u64) -> Result<Exported, Refused> {
    let Some(lib) = crate::unixlib::lib() else {
        return Err(Refused { step: 0, error: -38 });
    };
    if len == 0 || (offset | len) % 4096 != 0 {
        return Err(Refused { step: 0, error: -22 });
    }
    let mut a = [section as u64, offset, len, 0, 0, 0];
    if !lib.call(crate::unixlib::DMABUF, &mut a) || a[3] == 0 {
        return Err(Refused { step: a[5], error: a[4] as i64 });
    }
    Ok(Exported(HANDLE(a[3] as *mut _)))
}

/// A heap of `len` bytes that is `[offset, offset + len)` of `section`'s
/// memory, with no copy: buffers only, the PC's write-back pages. An error
/// says which step refused.
pub(crate) fn open_heap(device: &ID3D12Device, section: usize, offset: u64, len: u64) -> Result<ID3D12Heap, String> {
    let exported = export(section, offset, len).map_err(|r| r.to_string())?;
    let mut heap: Option<ID3D12Heap> = None;
    // SAFETY: a live handle of ours, which the call checks, and a slot for
    // the heap.
    unsafe { device.OpenSharedHandle(exported.0, &mut heap) }.map_err(|e| {
        format!(
            "OpenSharedHandle(ID3D12Heap): {:#010x} ({}) (vkd3d-proton without tools/vkd3d-proton, or win32u without tools/wine-dmabuf)",
            e.code().0,
            e.message().trim()
        )
    })?;
    heap.ok_or_else(|| "OpenSharedHandle(ID3D12Heap): no heap".to_owned())
}

/// The PC's memory this process's card clients hold, in KiB
/// (`drm-total-gtt` of each amdgpu client in `/proc/self/fdinfo`, read on
/// the Unix side), the imports' pinned pages among it; `None` where there
/// is no such account.
pub(crate) fn gtt_kib() -> Option<u64> {
    let lib = crate::unixlib::lib()?;
    let mut a = [0u64; 2];
    lib.call(crate::unixlib::GTT, &mut a).then_some(a[0])
}
