//! Write tracking by the kernel for the hot blocks of view memory: those the
//! title writes again soon after each time the guards make them read-only.
//!
//! The guards ([`super::guard`]) are cheap where memory stays clean: a frame
//! asks about some 10 GB of view memory, nearly all of it clean, and a clean
//! block is a bit in a set, no call to the kernel. They are dear where the
//! title keeps writing: each refresh makes the block read-only again, the
//! title's next write faults into our handler, waits for the guards' lock and
//! opens it: hundreds of faults a frame and some 35 ms of lock waits under
//! Proton. The kernel's tracking costs the other way round. A userfaultfd in
//! asynchronous write-protect mode (`crate::uffd`) takes the title's writes
//! with no fault reaching user mode, and `PAGEMAP_SCAN` says which 4 KiB
//! pages were written since the last scan, protecting them again in the same
//! call; but the scan walks every page-table entry of what it is given,
//! written or not, some 2.4 ms a GB here. Tracking all view memory that way
//! cost 75 ms a frame against the guards' 35. (Linux made both for Windows'
//! `GetWriteWatch`; Wine's write watch has them for private memory only, and
//! its sections, memfds, are shared.)
//!
//! So each does what it is good at: a block the guards' faults show hot (two
//! faults soon after one another, [`faulted`]) is handed to the kernel. From
//! then on the guards leave it unprotected, and at each asking it is scanned
//! instead ([`quiet`]): told as written only when the kernel saw a write.
//! The hot set is small (at most [`MOST`] blocks), so its scans are too.
//!
//! A page shown at several addresses has page-table entries at each, and a
//! write through one is seen at that one only; the guards protect such pages
//! together, so they stay theirs ([`crate::inplace::shown_elsewhere`]). The
//! runtime's own writes, through views of its own, reach no entry of ours:
//! [`wrote`] says which tracked pages those were, for the journal. A block
//! mapped anew is forgotten ([`forget`]); the kernel's registration went
//! with the old mapping.
//!
//! Off unless `VICTORY_UFFD=1`. Measured (diagnostics build, 1080p): as fast
//! as the guards alone (16.2 against 16.5 fps, within a run's noise), with
//! some 580 hot blocks asked about 11,600 times a frame, 99% found quiet, in
//! 3 ms of scans, and the guards' lock waits 6.7 ms a frame instead of 11.7.

use std::collections::{BTreeSet, HashMap};
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering::Relaxed};
use std::time::{Duration, Instant};

use super::PAGE;

/// At most this many blocks (of [`PAGE`] bytes) are handed to the kernel:
/// 256 MiB, a scan of ~0.6 ms if all were asked about at once.
const MOST: usize = 4096;

/// Two faults of a block this close together make it hot.
const SOON: Duration = Duration::from_millis(50);

#[derive(Default)]
struct Hot {
    /// Hot blocks, to be registered at their next asking.
    wanted: BTreeSet<usize>,
    /// Hot blocks the kernel tracks.
    tracked: BTreeSet<usize>,
    /// Blocks the kernel would not take (or with other views): the guards'.
    refused: BTreeSet<usize>,
    /// A block's last fault, and how many came soon after one another.
    faults: HashMap<usize, (Instant, u8)>,
}

static HOT: Mutex<Option<Hot>> = Mutex::new(None);

/// How many blocks are wanted or tracked: none, and the guards ask nothing.
static COUNT: AtomicUsize = AtomicUsize::new(0);

fn hot<T>(f: impl FnOnce(&mut Hot) -> T) -> T {
    f(HOT.lock().unwrap_or_else(|e| e.into_inner()).get_or_insert_with(Hot::default))
}

/// Since the start: scans, their microseconds, blocks registered, refused,
/// asked about and found quiet, and runtime writes noted.
static STATS: [AtomicU64; 7] = [const { AtomicU64::new(0) }; 7];

/// Whether the kernel tracks writes for us at all.
pub(super) fn on() -> bool {
    crate::uffd::on()
}

/// A write fault opened `block`: twice soon after one another, and it is
/// hot. (With the guards' lock held: a lock of this module's alone, no call
/// to the kernel.)
pub(super) fn faulted(block: usize) {
    if !on() {
        return;
    }
    hot(|h| {
        if h.tracked.contains(&block) || h.wanted.contains(&block) || h.refused.contains(&block) {
            return;
        }
        let now = Instant::now();
        if h.faults.len() >= 1 << 13 {
            h.faults.retain(|_, (at, _)| now.duration_since(*at) < SOON);
        }
        let soon = match h.faults.get(&block) {
            Some(&(at, n)) if now.duration_since(at) < SOON => n.saturating_add(1),
            _ => 0,
        };
        if soon >= 2 && h.wanted.len() + h.tracked.len() < MOST {
            h.faults.remove(&block);
            h.wanted.insert(block);
            COUNT.fetch_add(1, Relaxed);
        } else {
            h.faults.insert(block, (now, soon));
        }
    });
}

/// The hot blocks of `[first, last)`, in order: those the guards leave to
/// the kernel ([`quiet`]).
pub(super) fn hot_in(first: usize, last: usize) -> Vec<usize> {
    if COUNT.load(Relaxed) == 0 || first >= last {
        return Vec::new();
    }
    hot(|h| h.tracked.range(first..last).chain(h.wanted.range(first..last)).copied().collect::<BTreeSet<_>>().into_iter().collect())
}

/// Of the hot blocks `blocks` (in order), those the kernel says nobody wrote
/// since they were last asked about: the rest count as written. Blocks wanted
/// are registered here and count as written (they were open); a block the
/// kernel refuses is the guards' again, written for now. The caller holds the
/// blocks ([`super::Asking`]) and does not hold the guards' lock.
pub(super) fn quiet(blocks: &[usize]) -> BTreeSet<usize> {
    let mut quiet = BTreeSet::new();
    if blocks.is_empty() {
        return quiet;
    }
    let started = Instant::now();
    let (tracked, wanted): (Vec<usize>, Vec<usize>) = hot(|h| blocks.iter().partition(|b| h.tracked.contains(b)));
    for block in wanted {
        let alone = crate::inplace::shown_elsewhere(block * PAGE, (block + 1) * PAGE).is_empty();
        let took = alone && crate::uffd::track(block * PAGE, PAGE);
        STATS[2 + usize::from(!took)].fetch_add(1, Relaxed);
        hot(|h| {
            if h.wanted.remove(&block) {
                if took {
                    h.tracked.insert(block);
                } else {
                    h.refused.insert(block);
                    COUNT.fetch_sub(1, Relaxed);
                }
            }
        });
    }
    // One scan for each run of neighbours.
    let mut runs = Vec::new();
    let mut i = 0;
    while i < tracked.len() {
        let from = tracked[i];
        let mut to = from + 1;
        i += 1;
        while i < tracked.len() && tracked[i] == to {
            to += 1;
            i += 1;
        }
        STATS[0].fetch_add(1, Relaxed);
        if !crate::uffd::scan(from * PAGE, to * PAGE, &mut runs) {
            // The mapping changed under us: written, and looked at anew.
            forget(from * PAGE, (to - from) * PAGE);
            runs.clear();
            continue;
        }
        let mut written = runs.drain(..).flat_map(|(a, b)| a / PAGE..b.div_ceil(PAGE)).peekable();
        for block in from..to {
            while written.next_if(|&w| w < block).is_some() {}
            if written.peek() != Some(&block) {
                quiet.insert(block);
            }
        }
    }
    STATS[1].fetch_add(started.elapsed().as_micros() as u64, Relaxed);
    STATS[4].fetch_add(blocks.len() as u64, Relaxed);
    STATS[5].fetch_add(quiet.len() as u64, Relaxed);
    quiet
}

/// The pages (in [`PAGE`]s) of `[at, end)` the kernel tracks, which the
/// runtime wrote through a view of its own: no page-table entry of ours saw
/// it, so the caller journals them as written.
pub(super) fn wrote(at: usize, end: usize) -> Vec<usize> {
    if COUNT.load(Relaxed) == 0 || at >= end {
        return Vec::new();
    }
    let pages: Vec<usize> = hot(|h| h.tracked.range(at / PAGE..end.div_ceil(PAGE)).copied().collect());
    STATS[6].fetch_add(u64::from(!pages.is_empty()), Relaxed);
    pages
}

/// What is mapped over `[at, at + len)` changed: the kernel's registration
/// went with the old mapping, and a block it refused may be another now.
pub(super) fn forget(at: usize, len: usize) {
    if !on() || len == 0 {
        return;
    }
    let blocks = at / PAGE..at.saturating_add(len).div_ceil(PAGE);
    hot(|h| {
        for set in [&mut h.tracked, &mut h.wanted] {
            let gone: Vec<usize> = set.range(blocks.clone()).copied().collect();
            COUNT.fetch_sub(gone.len(), Relaxed);
            gone.iter().for_each(|b| _ = set.remove(b));
        }
        let gone: Vec<usize> = h.refused.range(blocks).copied().collect();
        gone.iter().for_each(|b| _ = h.refused.remove(b));
    });
}

/// The frame line's account of the kernel's tracking since the last call.
pub(crate) fn line(frames: f64) {
    if !on() || frames <= 0.0 {
        return;
    }
    static LAST: Mutex<[u64; 7]> = Mutex::new([0; 7]);
    let now: [u64; 7] = std::array::from_fn(|i| STATS[i].load(Relaxed));
    let d: [f64; 7] = {
        let mut last = LAST.lock().unwrap_or_else(|e| e.into_inner());
        let d = std::array::from_fn(|i| (now[i] - last[i]) as f64 / frames);
        *last = now;
        d
    };
    let (tracked, refused) = hot(|h| (h.tracked.len(), h.refused.len()));
    tracing::info!(
        "hot blocks tracked by the kernel per frame (VICTORY_UFFD=1): {:.0} asked, {:.0} of them quiet; {:.0} scans {:.2} ms; \
         {:.1} registered, {:.1} refused, {:.1} runtime writes noted; {} blocks tracked, {} refused",
        d[4], d[5], d[0], d[1] / 1e3, d[2], d[3], d[6], tracked, refused
    );
}
