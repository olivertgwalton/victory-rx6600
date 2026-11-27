//! Copy-back on access (`VICTORY_LAZY_BACK=1`, off by default): what shaders
//! wrote to a copy in the PC's memory stays in the copy until something on
//! the CPU touches the title memory it belongs to, instead of going into
//! title memory after every flight.
//!
//! Why: a landing's blocks are, per frame, mostly landed again before
//! anything on the CPU looks at them (58% in run landwatch), or read only by
//! our own next refresh, which finds the bytes it just put there
//! (`Shadow::take_landed`'s echo); the title touches some 6% of them
//! (`crate::watch::land_watch`). The console has one memory: the CPU sees
//! what the GPU wrote when it looks, and nothing is moved for it.
//!
//! How (as Ryujinx's flush on read, `BufferModifiedRangeList`): a piece a
//! flight wrote is *owed* instead of merged. Its 64 KiB block is made
//! read-only, title memory is compared with the copy's snapshot of the
//! piece (what the copy held when the flight began, which a refresh had
//! taken from title memory): equal, title memory has not been written
//! since, and the block is made no-access and noted; else it is merged as
//! before. From then on title memory there is frozen and is itself the
//! basis of the merge: when anything touches the block (the title, our
//! copies, the next refresh of another copy of the same memory) the fault
//! comes here ([`fault`]), the copy's bytes that differ from title memory
//! go in, and the block gets its protection back. Later flights that write
//! the same piece leave it owed.
//!
//! What it leaves to the eager landing: copies in the card's memory (their
//! bytes come back by a fetch), memory that is not write-watched private
//! memory (views of physical pages are shown at more than one address,
//! which one protection cannot cover), a block another copy owes.
//!
//! Before the kernel writes title memory (a file read) the owed blocks
//! there are landed ([`land_range`]); freed memory forgets them
//! ([`forget_range`]); a copy let go lands what it owes first
//! ([`land_copy`]). What it cannot see: the kernel *reading* an owed block
//! (a file write from it) fails instead of faulting.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering::Relaxed};
use std::sync::{Mutex, MutexGuard, OnceLock};

use windows_sys::Win32::System::Memory::{PAGE_NOACCESS, PAGE_READONLY, VirtualProtect};

use crate::shadow::Shadow;

/// The unit owed and protected: a landing's piece.
pub(crate) const BLOCK: usize = 64 << 10;

pub(crate) fn on() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| {
        let on = std::env::var_os("VICTORY_LAZY_BACK").is_some_and(|v| v == "1");
        if on {
            tracing::info!("copy-back on access (VICTORY_LAZY_BACK=1): pieces shaders wrote go into title memory when the CPU touches them");
        }
        on
    })
}

/// An owed block: the copies that owe it (one, as a rule) and the block's
/// protection before.
struct Owed {
    copies: Vec<*const Shadow>,
    protect: u32,
}

// SAFETY: the copies are only read under the table's lock, and a copy takes
// itself out of the table (under the lock) before it goes ([`land_copy`]).
unsafe impl Send for Owed {}

static TABLE: Mutex<Option<HashMap<usize, Owed>>> = Mutex::new(None);
/// Every block ever owed: a fault there that another thread's touch has
/// answered already (the protection is back) is retried, not passed on.
static EVER: Mutex<Option<std::collections::HashSet<usize>>> = Mutex::new(None);
/// Blocks owed now: the fault handler looks no further when 0.
static COUNT: AtomicU64 = AtomicU64::new(0);

fn table() -> MutexGuard<'static, Option<HashMap<usize, Owed>>> {
    TABLE.lock().unwrap_or_else(|e| e.into_inner())
}

/// Since the last line: pieces owed, pieces left owed by a later flight,
/// merged at once (title memory had changed, or another copy owes the
/// block), landed by a fault, landed before a kernel write, landed as a
/// copy went, forgotten with freed memory, bytes landed.
static STATS: [AtomicU64; 8] = [const { AtomicU64::new(0) }; 8];

fn protect(block: usize, protection: u32) -> Option<u32> {
    let mut old = 0;
    // SAFETY: a block of committed title memory; only its protection changes.
    (unsafe { VirtualProtect((block * BLOCK) as *const _, BLOCK, protection, &mut old) } != 0).then_some(old)
}

/// The outcome of [`owe`].
pub(crate) enum Owe {
    /// Owed now, or still: nothing goes into title memory.
    Owed,
    /// Merge as before.
    Merge,
}

/// A flight wrote `[at, at + n)` of `copy` (title addresses, inside one
/// block); `same` says whether title memory there still holds what the
/// copy's snapshot holds (read while the block is read-only). Owes the
/// block if it can.
pub(crate) fn owe(copy: &Shadow, at: usize, same: impl FnOnce() -> bool) -> Owe {
    let block = at / BLOCK;
    let mut t = table();
    let map = t.get_or_insert_with(HashMap::new);
    if let Some(owed) = map.get(&block) {
        if owed.copies.len() == 1 && std::ptr::eq(owed.copies[0], copy) {
            STATS[1].fetch_add(1, Relaxed);
            return Owe::Owed;
        }
        // Another copy owes it: both go in now, and this piece merges.
        let owed = map.remove(&block).expect("looked up");
        COUNT.fetch_sub(1, Relaxed);
        land_block(block, owed);
        STATS[2].fetch_add(1, Relaxed);
        return Owe::Merge;
    }
    let Some(before) = protect(block, PAGE_READONLY) else {
        return Owe::Merge;
    };
    // From here on a write faults and waits for the table's lock.
    if !same() {
        protect(block, before);
        STATS[2].fetch_add(1, Relaxed);
        return Owe::Merge;
    }
    if protect(block, PAGE_NOACCESS).is_none() {
        protect(block, before);
        return Owe::Merge;
    }
    map.insert(block, Owed { copies: vec![copy as *const Shadow], protect: before });
    EVER.lock().unwrap_or_else(|e| e.into_inner()).get_or_insert_with(Default::default).insert(block);
    COUNT.fetch_add(1, Relaxed);
    STATS[0].fetch_add(1, Relaxed);
    Owe::Owed
}

/// Puts what the copies owe into the block and gives it its protection
/// back. Under the table's lock.
fn land_block(block: usize, owed: Owed) {
    protect(block, owed.protect);
    for copy in owed.copies {
        // SAFETY: the copy is alive: it takes itself out of the table before
        // it goes, under the lock held here.
        let n = unsafe { &*copy }.land_owed(block * BLOCK, BLOCK);
        STATS[7].fetch_add(n as u64, Relaxed);
    }
}

/// From the fault handler: an access to `address` faulted. True if it was
/// an owed block, now landed (the access may go on).
pub(crate) fn fault(address: usize, write: bool) -> bool {
    let block = address / BLOCK;
    if COUNT.load(Relaxed) == 0 && !ever(block) {
        return false;
    }
    let mut t = table();
    let Some(owed) = t.as_mut().and_then(|m| m.remove(&block)) else {
        drop(t);
        // Landed by another thread's touch since this access faulted.
        return ever(block) && allows(address, write);
    };
    COUNT.fetch_sub(1, Relaxed);
    land_block(block, owed);
    STATS[3].fetch_add(1, Relaxed);
    true
}

fn ever(block: usize) -> bool {
    EVER.lock().unwrap_or_else(|e| e.into_inner()).as_ref().is_some_and(|e| e.contains(&block))
}

/// Whether the page at `address` now allows the access.
fn allows(address: usize, write: bool) -> bool {
    use windows_sys::Win32::System::Memory::{MEM_COMMIT, MEMORY_BASIC_INFORMATION, PAGE_EXECUTE_READWRITE, PAGE_READWRITE, VirtualQuery};
    // SAFETY: plain data filled in by the query.
    let mut info: MEMORY_BASIC_INFORMATION = unsafe { std::mem::zeroed() };
    // SAFETY: a valid out pointer of the right size.
    if unsafe { VirtualQuery(address as *const _, &mut info, size_of_val(&info)) } == 0 || info.State != MEM_COMMIT {
        return false;
    }
    let p = info.Protect & 0xff;
    if write { p == PAGE_READWRITE || p == PAGE_EXECUTE_READWRITE } else { p != PAGE_NOACCESS && p != 0 }
}

/// Lands the owed blocks of `[at, at + len)` (before the kernel writes it).
pub(crate) fn land_range(at: usize, len: usize) {
    if COUNT.load(Relaxed) == 0 || len == 0 {
        return;
    }
    let mut t = table();
    let Some(map) = t.as_mut() else { return };
    for block in at / BLOCK..at.saturating_add(len).div_ceil(BLOCK) {
        if let Some(owed) = map.remove(&block) {
            COUNT.fetch_sub(1, Relaxed);
            land_block(block, owed);
            STATS[4].fetch_add(1, Relaxed);
        }
    }
}

/// Forgets the owed blocks of `[at, at + len)`: the title freed the memory.
pub(crate) fn forget_range(at: usize, len: usize) {
    if COUNT.load(Relaxed) == 0 || len == 0 {
        return;
    }
    let mut t = table();
    let Some(map) = t.as_mut() else { return };
    let before = map.len();
    map.retain(|&block, _| block < at / BLOCK || block >= at.saturating_add(len).div_ceil(BLOCK));
    let gone = (before - map.len()) as u64;
    COUNT.fetch_sub(gone, Relaxed);
    STATS[6].fetch_add(gone, Relaxed);
}

/// Lands what `copy` owes, before it goes.
pub(crate) fn land_copy(copy: &Shadow) {
    if COUNT.load(Relaxed) == 0 {
        return;
    }
    let mut t = table();
    let Some(map) = t.as_mut() else { return };
    let blocks: Vec<usize> = map.iter().filter(|(_, o)| o.copies.iter().any(|&c| std::ptr::eq(c, copy))).map(|(&b, _)| b).collect();
    for block in blocks {
        if let Some(owed) = map.remove(&block) {
            COUNT.fetch_sub(1, Relaxed);
            land_block(block, owed);
            STATS[5].fetch_add(1, Relaxed);
        }
    }
}

/// The frame line.
pub(crate) fn line(frames: f64) {
    if !on() || frames <= 0.0 {
        return;
    }
    let s: [f64; 8] = std::array::from_fn(|i| STATS[i].swap(0, Relaxed) as f64 / frames);
    tracing::info!(
        "copy-back on access, per frame: {:.0} pieces owed, {:.0} left owed by a later flight, {:.0} merged at once; \
         landed {:.1} by a touch, {:.1} before kernel writes, {:.1} as a copy went, {:.1} forgotten with freed memory; \
         {:.2} MB landed; {} blocks owed now",
        s[0],
        s[1],
        s[2],
        s[3],
        s[4],
        s[5],
        s[6],
        s[7] / 1e6,
        COUNT.load(Relaxed)
    );
}
