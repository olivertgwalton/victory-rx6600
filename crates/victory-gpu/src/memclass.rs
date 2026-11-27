//! Copies of title memory by what the title asked that memory to be: its
//! xmem class (`victory_hle::modules::xmem` tells, from the arguments the
//! title gave `XMemVirtualAlloc`). Bytes the copies send up to the card and
//! bytes they bring back into title memory are counted per class, for the
//! frame line `copies by xmem class`.
//!
//! What it is for: the console has one memory; every byte counted here is a
//! byte the layer moves because the PC has two. Which classes carry them
//! says which policy changes would save them (memory the GPU only reads
//! never needs copying back; CPU-only memory never needs copying up; and
//! so on).

use std::sync::OnceLock;
use std::sync::atomic::{AtomicU64, Ordering};

/// The classes, in the order [`Classifier`] numbers them.
pub const CLASSES: [&str; 9] = [
    "not title memory",
    "CPU only",
    "GPU read-only",
    "GPU read-write",
    "write-combined",
    "coherent",
    "page pool",
    "32-bit heap",
    "tiled (GPU page mappings)",
];

/// A title address's class (an index into [`CLASSES`]).
pub type Classifier = fn(usize) -> u8;

static CLASSIFIER: OnceLock<Classifier> = OnceLock::new();

/// Set once, by the runtime's xmem.
pub fn set_classifier(f: Classifier) {
    let _ = CLASSIFIER.set(f);
}

fn class(at: usize) -> usize {
    CLASSIFIER.get().map_or(0, |f| f(at) as usize).min(CLASSES.len() - 1)
}

static UP: [AtomicU64; 9] = [const { AtomicU64::new(0) }; 9];
static BACK: [AtomicU64; 9] = [const { AtomicU64::new(0) }; 9];

/// `n` bytes at title address `at` went up to the card.
pub(crate) fn up(at: usize, n: usize) {
    if n != 0 {
        UP[class(at)].fetch_add(n as u64, Ordering::Relaxed);
    }
}

/// `n` bytes at title address `at` came back into title memory.
pub(crate) fn back(at: usize, n: usize) {
    if n != 0 {
        BACK[class(at)].fetch_add(n as u64, Ordering::Relaxed);
    }
}

/// The frame line: MB a frame up and back, by class (classes with none
/// left out).
pub(crate) fn line(frames: f64) {
    let take = |a: &[AtomicU64; 9]| a.each_ref().map(|c| c.swap(0, Ordering::Relaxed) as f64 / 1e6 / frames);
    let (up, back) = (take(&UP), take(&BACK));
    let parts: Vec<String> = (0..CLASSES.len())
        .filter(|&i| up[i] >= 0.05 || back[i] >= 0.05)
        .map(|i| format!("{}: up {:.1}, back {:.1}", CLASSES[i], up[i], back[i]))
        .collect();
    if !parts.is_empty() {
        tracing::info!("copies by xmem class, MB a frame: {}", parts.join("; "));
    }
}

/// Bytes of title memory held in copies in the PC's memory, by class:
/// every copy's CPU side (copies with a card side too), since the start.
static HELD_PC: [[std::sync::atomic::AtomicI64; 7]; 9] = [const { [const { std::sync::atomic::AtomicI64::new(0) }; 7] }; 9];

/// The makers of copies, in the order `shadow::KIND` numbers them.
pub(crate) const KINDS: [&str; 7] = ["other", "unbacked", "title buffer", "lookup", "by address", "window", "exact view"];

/// A copy of `n` bytes from title address `at` was made (`n` > 0) or let go.
pub(crate) fn held(at: usize, n: i64, kind: u8) {
    HELD_PC[class(at)][usize::from(kind).min(6)].fetch_add(n, Ordering::Relaxed);
}

/// For the census: per class, MB of copies in the PC's memory, of card
/// sides in the card's memory, and of card sides that are in the PC's
/// memory all the same (`card` gives those two, from the live copies).
pub(crate) fn held_line(card: &[[u64; 2]; 9]) -> String {
    (0..CLASSES.len())
        .filter_map(|i| {
            let kinds: Vec<u64> = HELD_PC[i].iter().map(|k| k.load(Ordering::Relaxed).max(0) as u64).collect();
            let pc: u64 = kinds.iter().sum();
            let [local, shared] = card[i];
            let by: Vec<String> = kinds.iter().zip(KINDS).filter(|(b, _)| **b >= 1 << 20).map(|(b, k)| format!("{k} {}", b >> 20)).collect();
            (pc + local + shared >= 1 << 20).then(|| {
                format!("{}: PC {} MB ({}), card {} MB, card sides in PC memory {} MB", CLASSES[i], pc >> 20, by.join(", "), local >> 20, shared >> 20)
            })
        })
        .collect::<Vec<_>>()
        .join("; ")
}

/// The class of a title address (an index into [`CLASSES`]).
pub(crate) fn class_of(at: usize) -> usize {
    class(at)
}
