//! Which bytes a landing put into title memory the CPU then touches
//! (`VICTORY_LAND_WATCH=1`, diagnostics builds): the question before
//! leaving what shaders wrote on the card until something on the CPU asks
//! for it, instead of fetching it after every flight.
//!
//! Each 64 KiB block a landing wrote is made no-access. The first touch,
//! the title's or ours (`crate::safecopy`'s copies, the next landing's
//! merge), comes to the fault handler ([`fault`]), which gives the block its
//! protection back and counts who touched it and how. A block landed again
//! untouched was fetched for nothing. Before the kernel writes title memory
//! (a file read) the range is let go ([`release`]): a no-access page would
//! cut the read short.
//!
//! What it does not see: another address of the same page (a page shown
//! twice), and the card reading title memory in place (do not combine with
//! `VICTORY_INPLACE`, whose host-pointer heaps the card would fault on).

use std::collections::HashMap;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering::Relaxed};

use windows_sys::Win32::System::Memory::{MEMORY_BASIC_INFORMATION, PAGE_NOACCESS, VirtualProtect, VirtualQuery};

use super::BLOCK;

/// Watched blocks: their protection before.
static WATCHED: Mutex<Option<HashMap<usize, u32>>> = Mutex::new(None);
/// Every block ever watched: a fault there that another thread's touch
/// already answered (the protection is back) is retried, not passed on.
static EVER: super::Bits = super::Bits::new();
/// Where our own first touches came from: the layer's frames (offsets in
/// its executable), and how often, since the last line.
static SITES: Mutex<Option<HashMap<Vec<u32>, u64>>> = Mutex::new(None);
/// How many there are: the handler looks no further when none.
static COUNT: AtomicUsize = AtomicUsize::new(0);

/// Since the last line: blocks landed (watched), landed again untouched,
/// touched by the title reading, by the title writing, by our reads, by our
/// writes, let go for a kernel write, found changed by the guards meanwhile.
static STATS: [AtomicU64; 8] = [const { AtomicU64::new(0) }; 8];

pub(crate) fn on() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("VICTORY_LAND_WATCH").is_some_and(|v| v == "1"))
}

fn protection(block: usize) -> Option<u32> {
    // SAFETY: plain data filled in by the query.
    let mut info: MEMORY_BASIC_INFORMATION = unsafe { std::mem::zeroed() };
    // SAFETY: a valid out pointer of the right size.
    let n = unsafe { VirtualQuery((block * BLOCK) as *const _, &mut info, size_of_val(&info)) };
    (n != 0 && info.State == windows_sys::Win32::System::Memory::MEM_COMMIT).then_some(info.Protect)
}

/// A landing put bytes into `ranges` of title memory (address, bytes).
pub(crate) fn landed(ranges: impl Iterator<Item = (usize, usize)>) {
    if !on() {
        return;
    }
    let mut blocks: Vec<usize> = ranges.flat_map(|(at, n)| at / BLOCK..(at + n.max(1)).div_ceil(BLOCK)).collect();
    blocks.sort_unstable();
    blocks.dedup();
    let left_alone: Vec<bool> = {
        let g = super::guards();
        blocks.iter().map(|&b| g.left_alone(b)).collect()
    };
    let mut watched = WATCHED.lock().unwrap_or_else(|e| e.into_inner());
    let map = watched.get_or_insert_with(HashMap::new);
    for (block, alone) in blocks.into_iter().zip(left_alone) {
        if alone {
            continue;
        }
        if map.contains_key(&block) {
            if protection(block) == Some(PAGE_NOACCESS) {
                // Fetched again, and nothing on the CPU looked at it since.
                STATS[1].fetch_add(1, Relaxed);
                continue;
            }
            // The guards changed it meanwhile: not ours to judge.
            map.remove(&block);
            COUNT.fetch_sub(1, Relaxed);
            STATS[7].fetch_add(1, Relaxed);
        }
        let Some(before) = protection(block) else { continue };
        if before == PAGE_NOACCESS {
            continue;
        }
        let mut old = 0;
        // SAFETY: a committed block of title memory; its protection is given
        // back at its first touch.
        if unsafe { VirtualProtect((block * BLOCK) as *const _, BLOCK, PAGE_NOACCESS, &mut old) } != 0 {
            map.insert(block, old);
            EVER.set(block);
            COUNT.fetch_add(1, Relaxed);
            STATS[0].fetch_add(1, Relaxed);
        }
    }
}

/// From the fault handler: a fault at `at`; `write`, `ours` as the handler
/// found them. Whether it was a watched block (its protection is back, and
/// the access may go on).
pub(crate) fn fault(at: usize, write: bool, ours: bool, context: *const windows_sys::Win32::System::Diagnostics::Debug::CONTEXT) -> bool {
    if !on() {
        return false;
    }
    let block = at / BLOCK;
    if !EVER.any(block, block + 1) {
        return false;
    }
    let mut watched = WATCHED.lock().unwrap_or_else(|e| e.into_inner());
    let Some(old) = watched.as_mut().and_then(|m| m.remove(&block)) else {
        drop(watched);
        // Answered by another thread's touch meanwhile: go on if it may.
        return protection(block).is_some_and(|p| p != PAGE_NOACCESS && (!write || p != windows_sys::Win32::System::Memory::PAGE_READONLY));
    };
    if ours {
        note_site(context);
    }
    COUNT.fetch_sub(1, Relaxed);
    let mut was = 0;
    // SAFETY: the block was ours to make no-access; this puts back what it had.
    unsafe { VirtualProtect((block * BLOCK) as *const _, BLOCK, old, &mut was) };
    STATS[2 + usize::from(write) + 2 * usize::from(ours)].fetch_add(1, Relaxed);
    true
}

/// Counts where an access of ours came from: the frames of the faulting
/// thread that lie in the layer's executable, unwound from the fault's
/// context (a frame with no unwind data, `crate::safecopy`'s copy, is taken
/// for a leaf: its return address on top of the stack).
fn note_site(context: *const windows_sys::Win32::System::Diagnostics::Debug::CONTEXT) {
    use windows_sys::Win32::System::Diagnostics::Debug::{RtlLookupFunctionEntry, RtlVirtualUnwind, CONTEXT};
    use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
    static RANGE: std::sync::OnceLock<(usize, usize)> = std::sync::OnceLock::new();
    let (base, end) = *RANGE.get_or_init(|| {
        // SAFETY: our own executable's handle and headers.
        unsafe {
            let base = GetModuleHandleW(std::ptr::null()) as usize;
            let nt = base + *((base + 0x3c) as *const u32) as usize;
            let size = *((nt + 0x50) as *const u32) as usize;
            (base, base + size)
        }
    });
    // SAFETY: the context the system passed, copied; the stack read is the
    // faulting thread's own, below its top.
    let mut ctx: CONTEXT = unsafe { std::ptr::read(context) };
    let mut ours = Vec::new();
    for _ in 0..24 {
        let pc = ctx.Rip;
        if pc == 0 {
            break;
        }
        if (pc as usize) >= base && (pc as usize) < end {
            ours.push((pc as usize - base) as u32);
            if ours.len() == 8 {
                break;
            }
        }
        let mut image = 0u64;
        // SAFETY: a lookup by address; null if no unwind data.
        let entry = unsafe { RtlLookupFunctionEntry(pc, &mut image, std::ptr::null_mut()) };
        if entry.is_null() {
            // A leaf: the return address is on top of the stack.
            // SAFETY: as above.
            ctx.Rip = unsafe { *(ctx.Rsp as *const u64) };
            ctx.Rsp += 8;
        } else {
            let (mut data, mut frame) = (std::ptr::null_mut(), 0u64);
            // SAFETY: unwinding a copy of the context one frame.
            unsafe { RtlVirtualUnwind(0, image, pc, entry, &mut ctx, &mut data, &mut frame, std::ptr::null_mut()) };
        }
    }
    let mut sites = SITES.lock().unwrap_or_else(|e| e.into_inner());
    *sites.get_or_insert_with(HashMap::new).entry(ours).or_insert(0) += 1;
}

/// The kernel is about to write `[at, at + len)`: watched blocks there get
/// their protection back first.
pub(crate) fn release(at: usize, len: usize) {
    if COUNT.load(Relaxed) == 0 || len == 0 {
        return;
    }
    let (first, last) = (at / BLOCK, at.saturating_add(len).div_ceil(BLOCK));
    let mut watched = WATCHED.lock().unwrap_or_else(|e| e.into_inner());
    let Some(map) = watched.as_mut() else { return };
    for block in first..last {
        if let Some(old) = map.remove(&block) {
            COUNT.fetch_sub(1, Relaxed);
            let mut was = 0;
            // SAFETY: as in `fault`.
            unsafe { VirtualProtect((block * BLOCK) as *const _, BLOCK, old, &mut was) };
            STATS[6].fetch_add(1, Relaxed);
        }
    }
}

/// The frame line's account since the last call.
pub(crate) fn line(frames: f64) {
    if !on() || frames <= 0.0 {
        return;
    }
    let s: [f64; 8] = std::array::from_fn(|i| STATS[i].swap(0, Relaxed) as f64 / frames);
    tracing::info!(
        "landed blocks the CPU touches, per frame (VICTORY_LAND_WATCH): {:.0} landed (64 KiB), {:.0} landed again untouched; \
         first touches: title reads {:.0}, title writes {:.0}, our reads {:.0}, our writes {:.0}; {:.1} let go for kernel writes, \
         {:.1} changed by the guards; {} watched now",
        s[0],
        s[1],
        s[2],
        s[3],
        s[4],
        s[5],
        s[6],
        s[7],
        COUNT.load(Relaxed)
    );
    let mut sites: Vec<(Vec<u32>, u64)> = SITES.lock().unwrap_or_else(|e| e.into_inner()).take().unwrap_or_default().into_iter().collect();
    sites.sort_by_key(|s| std::cmp::Reverse(s.1));
    let per = frames;
    for (stack, n) in sites.into_iter().take(6) {
        let frames: Vec<String> = stack.iter().map(|f| format!("{f:#x}")).collect();
        tracing::info!("landed blocks first read by us from: {:.1} a frame, frames (offsets in the executable) {}", n as f64 / per, frames.join(" "));
    }
}
