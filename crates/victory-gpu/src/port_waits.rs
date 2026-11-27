//! Renderer port #8 (`VICTORY_PORT_WAITS=1`, off by default): the title's
//! wait for a value the GPU writes is a sleep on the word's address, ended
//! by the layer's store of the value. The site and why are in
//! `victory_loader::wait_sites`; this is what its call reaches.

use std::sync::OnceLock;
use std::sync::atomic::{AtomicU64, Ordering::Relaxed};

pub fn on() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| std::env::var("VICTORY_PORT_WAITS").is_ok_and(|v| v.trim() == "1"))
}

/// The longest one sleep, in milliseconds (`VICTORY_PORT_WAITS_MS`, 8): a
/// store that wakes nobody (a shader's, a landing's bytes, the title's own)
/// is seen after this at the latest, by the loop's own look. A net, not the
/// way out: run wait2, with 200 ms, 30 of 24,576 sleeps ended by it and the
/// frame rate was the same (5.4 ms a sleep, about 7 a frame).
fn most_ms() -> u32 {
    static MS: OnceLock<u32> = OnceLock::new();
    *MS.get_or_init(|| std::env::var("VICTORY_PORT_WAITS_MS").ok().and_then(|v| v.trim().parse().ok()).unwrap_or(8u32).max(1))
}

/// Sleeps, of them ended by the timeout, and microseconds slept.
static STATS: [AtomicU64; 3] = [const { AtomicU64::new(0) }; 3];

/// The address of [`wait`], for the loader's stub.
pub fn wait_address() -> usize {
    wait as *const () as usize
}

/// Called from the title's wait loop in place of its `Sleep(0)`: `word` is
/// what it waits for, reached when bit 55 of `*word - value` is clear (its
/// own test; all ones is its "no value"). Returns at once if that holds,
/// otherwise when the word is stored to (`crate::queue::Written::write`
/// wakes its address) or the timeout ends. The loop looks again either way.
extern "system" fn wait(word: *const u64, value: u64) {
    if word.is_null() {
        std::thread::yield_now();
        return;
    }
    // SAFETY: the word the title's loop has just read.
    let seen = unsafe { word.read_volatile() };
    if seen == u64::MAX || seen.wrapping_sub(value) & (1 << 55) == 0 {
        return;
    }
    let started = std::time::Instant::now();
    // SAFETY: waits while the word still holds `seen`; a store between the
    // read above and this call ends it at once.
    #[cfg(windows)]
    let woken = unsafe { windows_sys::Win32::System::Threading::WaitOnAddress(word.cast(), (&raw const seen).cast(), 8, most_ms()) } != 0;
    #[cfg(not(windows))]
    let woken = {
        std::thread::yield_now();
        true
    };
    let n = STATS[0].fetch_add(1, Relaxed) + 1;
    if !woken {
        STATS[1].fetch_add(1, Relaxed);
    }
    let us = STATS[2].fetch_add(started.elapsed().as_micros() as u64, Relaxed);
    if n.is_multiple_of(4096) {
        tracing::info!(
            "renderer port 8 (VICTORY_PORT_WAITS=1): the title's waits for values the GPU writes slept {n} times so far, {} of them to the timeout of {} ms (a store that woke nobody), {:.2} ms a sleep",
            STATS[1].load(Relaxed),
            most_ms(),
            us as f64 / n as f64 / 1000.0
        );
    }
}
