//! What the card wrote, so that copy-back looks only there.
//!
//! A copy of a title buffer in the PC's memory that shaders may write
//! (`Shadow::gpu_writes`) was copied whole before each submission (its
//! snapshot) and compared whole after it, to find what the shaders wrote: some
//! 150 MB a frame of each, most of it in ranges the shaders never wrote (four
//! of 11.9 MB came back unchanged every frame; of 37 MB, 4 written). On the
//! helper threads and the landing thread that was the largest use of the
//! CPU after recording (`shadow.rs`' `find_pieces`: 57 % and 43 % of them,
//! profile base).
//!
//! Now the shaders say where they wrote. Every store and atomic of a
//! translated shader to title memory puts the number of the submission
//! running into a dword for its 64 KiB page (`victory_shader::emit::SLOT_MARKS`;
//! a store whose address the shader cannot tell, into one dword for those).
//! Each submission begins with a list of ours that writes its number where
//! the shaders read it ([`epoch_list`]): in order on the queue, so every
//! shader of the submission reads its own. The card's copy commands into
//! copies (`card_steps`, `place`) are noted on the CPU at submission, with
//! the same numbers, beside them ([`copies_written`]). A landing then knows
//! that a page whose marks are older than its flight was written by nothing
//! of that flight or after it ([`untouched_since`]), and does not compare it.
//! The snapshot is still taken: which pages the shaders will write is known
//! only once they ran, and what they wrote is told from the title's own
//! writes meanwhile by the bytes before.
//!
//! The marks lie in the PC's memory, mapped: written by the card over the
//! bus, read here once the flight is done. `VICTORY_WRITE_MARKS=0`: every
//! range is compared whole, as before (the shaders mark all the same).

use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Mutex, OnceLock};

use victory_shader::emit::{MARKS_DWORDS, MARKS_EPOCH, MARKS_UNKNOWN, MEMORY_PAGES};
use windows::Win32::Graphics::Direct3D12::*;
use windows::core::Interface;

/// Whether landings skip what the marks say no shader wrote
/// (`VICTORY_WRITE_MARKS=0`: they do not).
pub(crate) fn on() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("VICTORY_WRITE_MARKS").is_none_or(|v| v != "0"))
}

struct Marks {
    buffer: ID3D12Resource,
    /// The mapping: [`MARKS_DWORDS`] dwords.
    data: usize,
    gpu: u64,
    /// The lists that write a submission's number, each with the host
    /// fence's value behind the submission it was last in.
    lists: Mutex<Vec<(ID3D12CommandAllocator, ID3D12GraphicsCommandList2, u64)>>,
    /// Per page, the number of the last submission whose copy commands of
    /// ours wrote copies there (CPU only: the card's stores go above).
    copies: Box<[AtomicU32]>,
}

// SAFETY: D3D12 objects are free-threaded; the mapping is of memory the
// struct owns, read and written as atomics or by the card.
unsafe impl Send for Marks {}
// SAFETY: as above.
unsafe impl Sync for Marks {}

fn marks() -> Option<&'static Marks> {
    static MARKS: OnceLock<Option<Marks>> = OnceLock::new();
    MARKS
        .get_or_init(|| {
            let host = crate::host::host()?;
            let bytes = u64::from(MARKS_DWORDS) * 4;
            let (buffer, data) = host.shared_buffer(bytes)?;
            crate::host::name(&buffer, || "what shaders wrote: marks".into());
            // SAFETY: a fresh mapping of `bytes` bytes, nothing on the card yet.
            unsafe { std::ptr::write_bytes(data, 0, bytes as usize) };
            // SAFETY: a live buffer.
            let gpu = unsafe { buffer.GetGPUVirtualAddress() };
            let copies = (0..MEMORY_PAGES).map(|_| AtomicU32::new(0)).collect();
            Some(Marks { buffer, data: data as usize, gpu, lists: Mutex::new(Vec::new()), copies })
        })
        .as_ref()
}

/// The marks' address, for the root view every translated shader that
/// stores to title memory has (`SLOT_MARKS`). `None` only if the host could
/// not make them.
pub(crate) fn gpu() -> Option<u64> {
    marks().map(|m| m.gpu)
}

/// A submission's mark: its number, never 0 (the marks start at 0).
fn epoch(number: u64) -> u32 {
    (number as u32).max(1)
}

/// The list to run first in submission `number`, which writes the number
/// where its shaders read it. `behind` is the value the host's fence takes
/// behind the submission, `done` the value it has now: a list is used again
/// once its last submission is done.
pub(crate) fn epoch_list(device: &ID3D12Device, number: u64, behind: u64, done: u64) -> Option<ID3D12CommandList> {
    let m = marks()?;
    let mut lists = m.lists.lock().unwrap_or_else(|e| e.into_inner());
    let i = match lists.iter().position(|l| l.2 <= done) {
        Some(i) => i,
        None => {
            // SAFETY: plain object creation.
            let made = unsafe {
                (|| -> windows::core::Result<(ID3D12CommandAllocator, ID3D12GraphicsCommandList2)> {
                    let allocator: ID3D12CommandAllocator = device.CreateCommandAllocator(D3D12_COMMAND_LIST_TYPE_DIRECT)?;
                    let list: ID3D12GraphicsCommandList2 = device.CreateCommandList(0, D3D12_COMMAND_LIST_TYPE_DIRECT, &allocator, None)?;
                    list.Close()?;
                    Ok((allocator, list))
                })()
            };
            let (allocator, list) = made.ok()?;
            lists.push((allocator, list, 0));
            lists.len() - 1
        }
    };
    let (allocator, list, last) = &mut lists[i];
    *last = behind;
    // SAFETY: the list's last run is done (`done`); the marks live for good.
    unsafe {
        allocator.Reset().ok()?;
        list.Reset(&*allocator, None).ok()?;
        let at = D3D12_WRITEBUFFERIMMEDIATE_PARAMETER { Dest: m.gpu + u64::from(MARKS_EPOCH) * 4, Value: epoch(number) };
        list.WriteBufferImmediate(1, &at, None);
        // The shaders that follow read it: from the copy that wrote it to
        // their unordered access.
        list.ResourceBarrier(&[D3D12_RESOURCE_BARRIER {
            Type: D3D12_RESOURCE_BARRIER_TYPE_TRANSITION,
            Flags: D3D12_RESOURCE_BARRIER_FLAG_NONE,
            Anonymous: D3D12_RESOURCE_BARRIER_0 {
                Transition: std::mem::ManuallyDrop::new(D3D12_RESOURCE_TRANSITION_BARRIER {
                    pResource: std::mem::ManuallyDrop::new(Some(m.buffer.clone())),
                    Subresource: D3D12_RESOURCE_BARRIER_ALL_SUBRESOURCES,
                    StateBefore: D3D12_RESOURCE_STATE_COPY_DEST,
                    StateAfter: D3D12_RESOURCE_STATE_UNORDERED_ACCESS,
                }),
            },
        }]);
        list.Close().ok()?;
        list.cast().ok()
    }
}

/// Copy commands of ours in submission `number` write `ranges` of title
/// memory (address, bytes) in copies: their pages are marked as a shader's
/// stores would mark them.
pub(crate) fn copies_written(ranges: impl Iterator<Item = (usize, usize)>, number: u64) {
    let Some(m) = marks() else { return };
    let e = epoch(number);
    for (at, len) in ranges {
        let (first, last) = (at >> 16, (at + len.max(1) - 1) >> 16);
        for page in first..=last {
            let page = (page as usize).min(MARKS_UNKNOWN as usize);
            m.copies[page].fetch_max(e, Ordering::AcqRel);
        }
    }
}

/// Whether nothing of submission `since` or after it wrote title memory
/// `[address, address + len)`: neither shaders (their marks are older, and
/// so is the mark of stores of no known address) nor copy commands of ours.
/// Read once those submissions are done. False when [`on`] is off.
pub(crate) fn untouched_since(address: usize, len: usize, since: u64) -> bool {
    if !on() {
        return false;
    }
    let Some(m) = marks() else { return false };
    let e = epoch(since);
    // SAFETY: the mapping, read as atomics (the card may be writing later
    // submissions' marks meanwhile).
    let card = |page: usize| unsafe { (*((m.data as *const AtomicU32).add(page))).load(Ordering::Acquire) };
    if card(MARKS_UNKNOWN as usize) >= e {
        return false;
    }
    let (first, last) = (address >> 16, (address + len.max(1) - 1) >> 16);
    (first..=last).all(|page| {
        let page = page.min(MARKS_UNKNOWN as usize);
        page != MARKS_UNKNOWN as usize && card(page) < e && m.copies[page].load(Ordering::Acquire) < e
    })
}
