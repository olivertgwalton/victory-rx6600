//! A byte census of the memory this crate holds, one line every
//! `VICTORY_CENSUS_FRAMES` frames (off unless set; 120 is a good start).
//!
//! The memory line (`copystat`, diagnostics builds only) says much more but
//! in prose, at irregular times, and in builds the launchers do not ship.
//! This line is for comparing runs of release builds: the same keys every
//! time, each one now and at its highest sample since the start, in MiB.
//!
//! Every number comes from a counter the owner of the memory keeps once per
//! allocation (a host copy counts itself once however many keys find it),
//! so nothing is counted for each reference to it. What the line cannot
//! know is said beside it: textures placed in pool blocks are counted with
//! the host textures and not again with the blocks, and the title's own
//! memory is in the process's private commit only.
//!
//! Host lists' command allocators hold driver memory no counter here can
//! see (about 9 GB of the 40-46 GB in `docs/findings.md`): the line gives
//! the number of host lists alive beside the bytes, and the private commit
//! includes what they hold.
//!
//! The card's own numbers (DXGI's budget and use of the local and the
//! non-local group) are printed beside the sum of what this crate knows it
//! put in local memory, so the gap between the two is the driver's, the
//! swap chain's, descriptor heaps' and everything not counted here.

use std::sync::OnceLock;
use std::sync::atomic::{AtomicU64, Ordering};

/// The title's own memory, committed (told by the runtime's xmem: its
/// commits and its physical pages).
static TITLE_BYTES: OnceLock<fn() -> u64> = OnceLock::new();

/// Set once, by the runtime's xmem.
pub fn set_title_bytes(f: fn() -> u64) {
    let _ = TITLE_BYTES.set(f);
}

/// The keys, in the order they are printed.
const KEYS: [&str; 28] = [
    "title_committed",
    "duplicates",
    "private_commit",
    "working_set",
    "title_copies",
    "card_copies",
    "card_windows",
    "mapped_windows",
    "snapshot_pool",
    "texture_staging",
    "texture_pool_blocks",
    "texture_pool_free",
    "texture_pool_resting",
    "texture_pool_empty",
    "target_pool_free",
    "staging_pool_blocks",
    "host_textures",
    "title_filled_textures",
    "targets_and_written",
    "list_scratch",
    "list_scratch_spare",
    "list_uploads_unretired",
    "view_copies",
    "address_directory",
    "address_directory_only",
    "known_local",
    "dxgi_local_used",
    "dxgi_nonlocal_used",
];

/// The highest sample of each key since the start.
static PEAK: [AtomicU64; KEYS.len()] = [const { AtomicU64::new(0) }; KEYS.len()];

/// A census every this many frames; 0: none.
fn every() -> u64 {
    static N: OnceLock<u64> = OnceLock::new();
    *N.get_or_init(|| interval(std::env::var("VICTORY_CENSUS_FRAMES").ok().as_deref()))
}

fn interval(setting: Option<&str>) -> u64 {
    setting.and_then(|v| v.trim().parse().ok()).unwrap_or(0)
}

/// Called once per frame begun.
pub(crate) fn frame(frame: u64) {
    let n = every();
    if n == 0 || !frame.is_multiple_of(n) {
        return;
    }
    log(frame);
}

/// The process's working set and private commit, in bytes.
fn process_memory() -> (u64, u64) {
    use windows_sys::Win32::System::ProcessStatus::{
        K32GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS,
    };
    use windows_sys::Win32::System::Threading::GetCurrentProcess;
    let mut c = PROCESS_MEMORY_COUNTERS {
        cb: size_of::<PROCESS_MEMORY_COUNTERS>() as u32,
        ..Default::default()
    };
    // SAFETY: our own process and a sized structure.
    unsafe { K32GetProcessMemoryInfo(GetCurrentProcess(), &mut c, c.cb) };
    (c.WorkingSetSize as u64, c.PagefileUsage as u64)
}

/// `u64::MAX` stands for a number the census could not take this time (its
/// lock was busy); it is printed as `?` and leaves the peak alone.
const UNKNOWN: u64 = u64::MAX;

fn log(frame: u64) {
    use std::sync::atomic::Ordering::Relaxed;
    let (working, private) = process_memory();
    let copies = crate::shadow::census_bytes();
    let [texture_blocks, staging_blocks] = crate::pool::memory().map(|mb| mb << 20);
    let host_textures = crate::resource::HOST_TEXTURES[1].load(Relaxed);
    let filled = crate::resource::KINDS[1].load(Relaxed);
    let alive = |i: usize| crate::host::ALIVE[i].load(Relaxed);
    let directory = crate::memory::retained();
    let card = crate::host::host().and_then(|h| h.video_memory());
    // What this crate put in the card's own memory: copies' card sides and
    // their twins, windows there, and host textures. (Pool blocks hold
    // placed host textures, which the textures count already.)
    let known_local = copies[2] + copies[3] + copies[4] + copies[5] + host_textures;
    // Bytes of title data held again somewhere else: host copies of title
    // memory, their card sides and twins, windows on the card (mapped or
    // not), host textures (each made from title memory), view copies. The
    // console holds each byte once; the goal is for this to be near 0.
    let duplicates = copies[1] + known_local + crate::shadow::view_copy_bytes();
    let room_parts = crate::pool::texture_room_parts();
    let values: [u64; KEYS.len()] = [
        TITLE_BYTES.get().map_or(UNKNOWN, |f| f()),
        duplicates,
        private,
        working,
        copies[1],
        copies[2] + copies[3],
        copies[4],
        copies[5],
        copies[6],
        crate::texture::live_staging_bytes(),
        texture_blocks,
        crate::pool::texture_room(),
        room_parts[0],
        room_parts[1],
        crate::pool::target_room(),
        staging_blocks,
        host_textures,
        filled,
        host_textures.saturating_sub(filled),
        alive(2),
        alive(4),
        alive(7),
        crate::shadow::view_copy_bytes(),
        directory.map_or(UNKNOWN, |d| d[1]),
        directory.map_or(UNKNOWN, |d| d[3]),
        known_local,
        card.map_or(UNKNOWN, |c| c[1]),
        card.map_or(UNKNOWN, |c| c[3]),
    ];
    let line = format_line(&values, &PEAK);
    let (budget, nonlocal_budget) = card.map_or((UNKNOWN, UNKNOWN), |c| (c[0], c[2]));
    tracing::warn!(target: "victory_gpu::census", "texture pool frame {frame}: {}", crate::pool::texture_holes());
    tracing::warn!(target: "victory_gpu::census", "texture pool frame {frame}: {}", crate::resource::pool_miss_line());
    tracing::warn!(target: "victory_gpu::census", "texture pool frame {frame}: by size: {}", crate::pool::texture_misses_line());
    tracing::warn!(target: "victory_gpu::census", "texture pool frame {frame}: made in free ranges with no room {}, found none {} (VICTORY_POOL_GAPS)", crate::draw::GAPS[0].load(std::sync::atomic::Ordering::Relaxed), crate::draw::GAPS[1].load(std::sync::atomic::Ordering::Relaxed));
    tracing::warn!(
        target: "victory_gpu::census",
        "memory census frame {frame}: {line} dxgi_local_budget={} dxgi_nonlocal_budget={} unaccounted_local={} \
         host_lists={} (MiB, now/peak; host_lists a count)",
        mib(budget),
        mib(nonlocal_budget),
        card.map_or("?".to_string(), |c| mib(c[1].saturating_sub(known_local))),
        alive(0),
    );
}

fn mib(bytes: u64) -> String {
    if bytes == UNKNOWN {
        "?".to_string()
    } else {
        format!("{:.0}", bytes as f64 / (1u64 << 20) as f64)
    }
}

/// `key=now/peak` for each key, updating the peaks.
fn format_line(values: &[u64; KEYS.len()], peaks: &[AtomicU64; KEYS.len()]) -> String {
    let mut out = String::with_capacity(KEYS.len() * 24);
    for (i, key) in KEYS.iter().enumerate() {
        let value = values[i];
        let peak = if value == UNKNOWN {
            peaks[i].load(Ordering::Relaxed)
        } else {
            peaks[i].fetch_max(value, Ordering::Relaxed).max(value)
        };
        if !out.is_empty() {
            out.push(' ');
        }
        out.push_str(key);
        out.push('=');
        out.push_str(&mib(value));
        out.push('/');
        out.push_str(&mib(peak));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn interval_is_off_unless_a_number_is_set() {
        assert_eq!(interval(None), 0);
        assert_eq!(interval(Some("")), 0);
        assert_eq!(interval(Some("abc")), 0);
        assert_eq!(interval(Some(" 120 ")), 120);
    }

    #[test]
    fn peaks_keep_the_highest_known_sample() {
        const MIB: u64 = 1 << 20;
        let peaks: [AtomicU64; KEYS.len()] = [const { AtomicU64::new(0) }; KEYS.len()];
        let mut values = [0u64; KEYS.len()];
        values[0] = 300 * MIB;
        let first = format_line(&values, &peaks);
        assert!(first.starts_with("title_committed=300/300 "));
        values[0] = 100 * MIB;
        values[1] = UNKNOWN;
        let second = format_line(&values, &peaks);
        assert!(second.starts_with("title_committed=100/300 duplicates=?/0 "));
        assert_eq!(second.split(' ').count(), KEYS.len());
    }
}
