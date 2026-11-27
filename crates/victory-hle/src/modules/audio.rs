//! The console's audio hardware (`AcpHal.dll`):
//!
//! * the APU heap: host memory with 32-bit device addresses;
//! * SHAPE context arrays allocated from it;
//! * the audio coprocessor's command and message queues, where every
//!   command completes at once and is acknowledged as the hardware would;
//! * the XMA decoder: enabled XMA contexts are decoded on a thread of ours
//!   into their output rings, like the hardware does.
//!
//! SHAPE's other units (rate conversion, EQ, filters, mixing through
//! flowgraphs) are not emulated, so voices that need them stay silent.
//! Mixed audio leaves through the host's WASAPI, which titles and
//! middleware reach through COM like on PC. fmod's console build sets up
//! the coprocessor inside `System_Create` and waits there for its first
//! command to be acknowledged; the title uses only XMA contexts.
#![allow(non_snake_case)]
#![allow(clippy::not_unsafe_ptr_arg_deref)]

use std::alloc::Layout;
use std::collections::VecDeque;
use std::ffi::c_void;
use std::sync::{Mutex, MutexGuard, OnceLock, RwLock, RwLockReadGuard};
use std::time::Instant;

use super::xasync::E_INVALIDARG;
use super::xgameruntime::{HRESULT, S_OK};
use crate::{HleModule, hle_export};

#[cfg(all(windows, feature = "diagnostics"))]
pub mod capture;

const E_FAIL: HRESULT = 0x8000_4005_u32 as i32;
const E_OUTOFMEMORY: HRESULT = 0x8007_000E_u32 as i32;

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

// The APU heap, as `AcpHal.dll` keeps it.
//
// Opening the device (on the first allocation, mapping, free or context
// request) makes three heaps of device memory: heap 1 ("cached", 64 MiB
// unless `ApuCreateHeap` said otherwise), heap 2 (the non-cached size, by
// default 0x114000, plus 8 KiB) and an internal 4 KiB one; then takes three
// blocks from heap 1 for itself (0x1000 bytes, then two of 256). A heap is
// an address-ordered list of blocks. An allocation rounds an alignment
// below 256 up to 256 and its size up to the alignment, takes the smallest
// free block that holds it at that alignment of its device address
// (stopping at an exact fit), and splits off the rest. The padding before
// the aligned address is up to the next multiple of the alignment strictly
// above the block's start, so a block already aligned gives up one whole
// alignment. Freeing merges a
// block with free neighbours. The heap counts free bytes, bytes asked for,
// alignment padding, the largest free block (recomputed only when that
// block is taken) and blocks in use, which `ApuHeapGetState` reports. When
// the last block a title took is freed and no interface is connected, the
// device closes, and `ApuCreateHeap` may size it again.
//
// The heaps' device addresses are ours to choose: heap 1 starts at 64 KiB,
// each next one at the following 64 KiB boundary.

const ALREADY_OPEN: HRESULT = 0x8ACA_0001_u32 as i32;
const NO_HEAP: HRESULT = 0x8ACA_0002_u32 as i32;
const CONTEXTS_ALLOCATED: HRESULT = 0x8AC8_0001_u32 as i32;
const NO_CONTEXTS: HRESULT = 0x8AC8_0002_u32 as i32;
const STILL_CONNECTED: HRESULT = 0x8AC8_0010_u32 as i32;

const CACHED_DEFAULT: u32 = 0x400_0000;
const NON_CACHED_DEFAULT: u32 = 0x11_4000;
/// The heaps' first device address.
const APU_BASE: u64 = 0x1_0000;
/// Blocks the device takes from heap 1 when it opens.
const DEVICE_BLOCKS: u32 = 3;

#[derive(Clone, Copy, Debug)]
struct Block {
    /// Bytes from the block's start to the next block's.
    size: u32,
    /// Bytes asked for, rounded up to the alignment.
    asked: u32,
    /// The block's start, as an offset into the heap.
    start: u32,
    /// The address handed out (host and device offsets are the same), or
    /// `None` for a block never handed out since it was split off.
    user: Option<u32>,
    used: bool,
}

/// What `ApuHeapGetState` copies out.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct HeapState {
    free: u32,
    used: u32,
    overhead: u32,
    largest: u32,
    blocks: u32,
}

struct Arena {
    host: usize,
    apu: u32,
    size: u32,
    layout: Layout,
    blocks: Vec<Block>,
    state: HeapState,
    /// The start of the block counted as the largest free one.
    largest: Option<u32>,
    /// The largest padding any block has had: how far past its end a
    /// block in use can still claim addresses (see `claims`).
    most_pad: u32,
}

impl Arena {
    fn new(apu: u32, size: u32) -> Option<Arena> {
        let layout = Layout::from_size_align(size as usize, 4096).ok()?;
        // SAFETY: non-zero size (every heap has some).
        let host = unsafe { std::alloc::alloc_zeroed(layout) } as usize;
        (host != 0).then(|| Arena {
            host,
            apu,
            size,
            layout,
            blocks: vec![Block {
                size,
                asked: 0,
                start: 0,
                user: None,
                used: false,
            }],
            state: HeapState {
                free: size,
                largest: size,
                ..HeapState::default()
            },
            largest: Some(0),
            most_pad: 0,
        })
    }

    fn holds_host(&self, p: usize) -> bool {
        p.wrapping_sub(self.host) < self.size as usize
    }

    fn holds_apu(&self, apu: u32) -> bool {
        apu.wrapping_sub(self.apu) < self.size
    }

    /// A block of `size` bytes at `align`, as offset into the heap.
    fn take(&mut self, size: u32, align: u32) -> Option<u32> {
        let align = if align & 0xff != 0 { (align + 0xff) & !0xff } else { align };
        let size = if size & (align - 1) != 0 {
            (size + align - 1) & !(align - 1)
        } else {
            size
        };
        let mut best: Option<(usize, u32, u32)> = None;
        for (i, b) in self.blocks.iter().enumerate() {
            if b.used {
                continue;
            }
            // The console's padding: up to the next multiple strictly above,
            // so an aligned block gives up a whole alignment.
            let apu = self.apu as u64 + b.start as u64;
            let pad = ((apu + align as u64) / align as u64 * align as u64 - apu) as u32;
            let need = pad.wrapping_add(size);
            if b.size >= need && best.is_none_or(|(j, _, _)| b.size < self.blocks[j].size) {
                best = Some((i, pad, need));
                if b.size == need {
                    break;
                }
            }
        }
        let (i, pad, need) = best?;
        let block = self.blocks[i];
        if block.size > need {
            self.blocks.insert(
                i + 1,
                Block {
                    size: block.size - need,
                    asked: 0,
                    start: block.start + need,
                    user: None,
                    used: false,
                },
            );
        }
        let user = block.start + pad;
        self.most_pad = self.most_pad.max(pad);
        self.blocks[i] = Block {
            size: need,
            asked: size,
            start: block.start,
            user: Some(user),
            used: true,
        };
        self.state.free -= need;
        self.state.used += size;
        self.state.overhead += need - size;
        if self.largest == Some(block.start) {
            self.largest = None;
            self.state.largest = 0;
            for b in self.blocks.iter().filter(|b| !b.used) {
                if b.size > self.state.largest {
                    self.state.largest = b.size;
                    self.largest = Some(b.start);
                }
            }
        }
        self.state.blocks += 1;
        Some(user)
    }

    /// Frees the block handed out at `user`; false if there is none.
    fn give_back(&mut self, user: u32) -> bool {
        let Some(i) = self
            .blocks
            .iter()
            .position(|b| b.used && b.user == Some(user))
        else {
            return false;
        };
        let block = self.blocks[i];
        let mut at = i;
        if let Some(next) = self.blocks.get(i + 1).copied()
            && !next.used
        {
            if self.largest == Some(next.start) {
                self.largest = Some(block.start);
            }
            self.blocks[i].size += next.size;
            self.blocks.remove(i + 1);
        }
        if i > 0 && !self.blocks[i - 1].used {
            if self.largest == Some(block.start) {
                self.largest = Some(self.blocks[i - 1].start);
            }
            self.blocks[i - 1].size += self.blocks[i].size;
            self.blocks.remove(i);
            at = i - 1;
        } else {
            self.blocks[i].used = false;
        }
        if self.largest.is_none() {
            self.largest = Some(self.blocks[at].start);
        }
        self.state.free += block.size;
        self.state.used -= block.asked;
        self.state.overhead -= block.size - block.asked;
        let largest = self.largest.expect("set above");
        self.state.largest = self
            .blocks
            .iter()
            .find(|b| b.start == largest)
            .map_or(0, |b| b.size);
        self.state.blocks -= 1;
        true
    }

    /// The block a heap offset maps through, as the console's walk finds
    /// it (0x18000547c, 0x180005508): blocks in order, the walk ending at
    /// the first one handed out above the offset, a block in use claiming
    /// `size` bytes from its handed-out address (so up to its padding past
    /// its end). Every handed-out address lies inside its own block, so no
    /// block before the one holding the offset ends the walk, and only
    /// blocks within `most_pad` before it can claim it: a binary search and
    /// a short look back give the walk's answer.
    fn claims(&self, offset: u32) -> bool {
        let k = self.blocks.partition_point(|b| b.start <= offset);
        let Some(k) = k.checked_sub(1) else {
            return false;
        };
        for b in self.blocks[..k].iter().rev() {
            if b.start + b.size + self.most_pad <= offset {
                break;
            }
            if b.used && offset < b.user.unwrap_or(0) + b.size {
                return true;
            }
        }
        let b = &self.blocks[k];
        let user = b.user.unwrap_or(0);
        !(b.user.is_some() && offset < user) && b.used && offset < user + b.size
    }

    /// The device address of a host address inside a block in use, 0 if
    /// none.
    fn map(&self, p: usize) -> u32 {
        if !self.holds_host(p) {
            return 0;
        }
        let offset = (p - self.host) as u32;
        if self.claims(offset) { self.apu + offset } else { 0 }
    }

    /// The host address of a device address inside a block in use.
    fn unmap(&self, apu: u32) -> Option<usize> {
        if !self.holds_apu(apu) {
            return None;
        }
        let offset = apu - self.apu;
        self.claims(offset).then(|| self.host + offset as usize)
    }

    /// The console's walk itself, which `claims` must agree with.
    #[cfg(test)]
    fn walk(&self, offset: u32) -> bool {
        for b in &self.blocks {
            let user = b.user.unwrap_or(0);
            if offset < user && b.user.is_some() {
                return false;
            }
            if b.used && offset < user + b.size {
                return true;
            }
        }
        false
    }
}

impl Drop for Arena {
    fn drop(&mut self) {
        // SAFETY: allocated in `Arena::new` with this layout.
        unsafe { std::alloc::dealloc(self.host as *mut u8, self.layout) };
    }
}

/// The SHAPE context arrays the device was given.
#[derive(Clone, Copy)]
struct Contexts {
    /// Offsets of the blocks in heap 2, by array (XMA and PCM share one).
    blocks: [Option<u32>; 5],
}

struct Device {
    cached: u32,
    non_cached: u32,
    /// Heaps 1, 2 and the internal one; all `None` while closed.
    heaps: Option<[Arena; 3]>,
    contexts: Option<Contexts>,
    /// Interfaces connected to the coprocessor.
    connected: u32,
}

// SAFETY: the arenas' host addresses are plain memory shared with title
// code; the device's lock guards the bookkeeping.
unsafe impl Send for Device {}

static DEVICE: Mutex<Device> = Mutex::new(Device {
    cached: CACHED_DEFAULT,
    non_cached: NON_CACHED_DEFAULT,
    heaps: None,
    contexts: None,
    connected: 0,
});

impl Device {
    /// Opens the device if it is closed.
    fn open(&mut self) -> Result<&mut [Arena; 3], HRESULT> {
        if self.heaps.is_none() {
            let sizes = [self.cached as u64, self.non_cached as u64 + 0x2000, 0x1000];
            let mut apu = APU_BASE;
            let mut heaps = Vec::new();
            for size in sizes {
                if apu + size > u32::MAX as u64 {
                    return Err(E_OUTOFMEMORY);
                }
                heaps.push(Arena::new(apu as u32, size as u32).ok_or(E_OUTOFMEMORY)?);
                apu = (apu + size).next_multiple_of(0x1_0000);
            }
            let mut heaps: [Arena; 3] = heaps.try_into().map_err(|_| E_FAIL)?;
            // The device's own blocks.
            for (size, align) in [(0x1000, 0x100), (0x14, 4), (8, 4)] {
                heaps[0].take(size, align).ok_or(E_OUTOFMEMORY)?;
            }
            heaps[2].take(0x4c, 4).ok_or(E_OUTOFMEMORY)?;
            self.heaps = Some(heaps);
        }
        Ok(self.heaps.as_mut().expect("opened above"))
    }

    /// Closes the device once titles hold nothing of it. True if context
    /// arrays went with it: the caller then resets the XMA decoder, after
    /// letting go of the device (the decoder reads device memory under its
    /// own lock).
    #[must_use]
    fn close_if_idle(&mut self) -> bool {
        if self.connected == 0
            && let Some(heaps) = &self.heaps
            && heaps[0].state.blocks == DEVICE_BLOCKS
            && heaps[1].state.blocks == 0
        {
            self.heaps = None;
            return self.contexts.take().is_some();
        }
        false
    }

    /// The heap of a host address: heap 1 if it holds it, else heap 2.
    fn heap_of_host(heaps: &[Arena; 3], p: usize) -> &Arena {
        if heaps[0].holds_host(p) { &heaps[0] } else { &heaps[1] }
    }
}

fn device() -> MutexGuard<'static, Device> {
    lock(&DEVICE)
}

/// Sizes heaps 1 and 2 and opens the device; a non-cached size of 0 keeps
/// the default. Too late once the device is open.
pub extern "system" fn ApuCreateHeap(cached: u32, non_cached: u32) -> HRESULT {
    let mut device = device();
    if cached == 0 {
        return E_INVALIDARG;
    }
    if device.heaps.is_some() {
        return ALREADY_OPEN;
    }
    device.cached = cached;
    if non_cached != 0 {
        device.non_cached = non_cached;
    }
    device.open().err().unwrap_or(S_OK)
}

/// A block of heap `heap` (1 or 2); both outputs are cleared first.
pub extern "system" fn ApuAlloc(
    out: *mut *mut c_void,
    apu: *mut u32,
    size: u32,
    align: u32,
    heap: u32,
) -> HRESULT {
    if !(1..=2).contains(&heap) {
        return E_INVALIDARG;
    }
    let mut device = device();
    let heaps = match device.open() {
        Ok(h) => h,
        Err(e) => return e,
    };
    let valid = !out.is_null() && size != 0 && align != 0 && align & 3 == 0 && align.is_power_of_two();
    // SAFETY: optional out parameters, cleared when given.
    unsafe {
        if !out.is_null() {
            out.write(std::ptr::null_mut());
        }
        if !apu.is_null() {
            apu.write(0);
        }
    }
    if !valid {
        return E_INVALIDARG;
    }
    let arena = &mut heaps[heap as usize - 1];
    let Some(offset) = arena.take(size, align) else {
        return E_OUTOFMEMORY;
    };
    if size >= 32 << 20 {
        tracing::info!(mb = size >> 20, heap, "audio memory for the title from our allocator");
    }
    // SAFETY: checked non-null; optional.
    unsafe {
        out.write((arena.host + offset as usize) as *mut c_void);
        if !apu.is_null() {
            apu.write(arena.apu + offset);
        }
    }
    S_OK
}

pub extern "system" fn ApuFree(ptr: *mut c_void) {
    let mut device = device();
    let Ok(heaps) = device.open() else {
        return;
    };
    let p = ptr as usize;
    let index = if heaps[0].holds_host(p) { 0 } else { 1 };
    let arena = &mut heaps[index];
    if p.wrapping_sub(arena.host) < u32::MAX as usize {
        arena.give_back((p - arena.host) as u32);
    }
    if device.close_if_idle() {
        drop(device);
        reset_xma();
    }
}

/// Heap 1 or 2's counters.
pub extern "system" fn ApuHeapGetState(out: *mut HeapState, heap: u32) -> HRESULT {
    let device = device();
    if out.is_null() || !(1..=2).contains(&heap) {
        return E_INVALIDARG;
    }
    let Some(heaps) = &device.heaps else {
        return NO_HEAP;
    };
    // SAFETY: checked non-null.
    unsafe { out.write_unaligned(heaps[heap as usize - 1].state) };
    S_OK
}

pub extern "system" fn ApuMapVirtualAddress(ptr: *const c_void) -> u32 {
    let mut device = device();
    match device.open() {
        Ok(heaps) => Device::heap_of_host(heaps, ptr as usize).map(ptr as usize),
        Err(_) => 0,
    }
}

/// Whether `ptr` lies in a block in use at a device address aligned to
/// `align` (0 counts as 1).
pub extern "system" fn ApuIsVirtualAddressValid(ptr: *const c_void, align: u32) -> bool {
    let apu = ApuMapVirtualAddress(ptr);
    apu != 0 && apu & (align.max(1) - 1) == 0
}

/// The host address of a device address in a block in use, null elsewhere.
pub extern "system" fn ApuMapApuAddress(apu: u32) -> *mut c_void {
    let mut device = device();
    let Ok(heaps) = device.open() else {
        return std::ptr::null_mut();
    };
    let arena = if heaps[0].holds_apu(apu) { &heaps[0] } else { &heaps[1] };
    arena.unmap(apu).map_or(std::ptr::null_mut(), |p| p as *mut c_void)
}

/// The host address of a device address anywhere in the device's heaps, as
/// the hardware sees it.
fn heap_unmap(apu: u32) -> Option<*mut u8> {
    let device = device();
    let heaps = device.heaps.as_ref()?;
    heaps
        .iter()
        .find(|a| a.holds_apu(apu))
        .map(|a| (a.host + (apu - a.apu) as usize) as *mut u8)
}

/// `len` bytes of device memory at a device address, if one heap holds
/// them all.
fn heap_bytes<'a>(apu: u32, len: usize) -> Option<&'a mut [u8]> {
    let device = device();
    let heaps = device.heaps.as_ref()?;
    let a = heaps.iter().find(|a| a.holds_apu(apu))?;
    let offset = (apu - a.apu) as usize;
    (offset + len <= a.size as usize)
        // SAFETY: inside a heap of the open device, which the title and the
        // audio hardware share like on the console.
        .then(|| unsafe { std::slice::from_raw_parts_mut((a.host + offset) as *mut u8, len) })
}

// SHAPE context arrays.

/// Six counts (rate conversion, EQ/compression, filter/volume, DMA, XMA,
/// PCM), then the six arrays and their device addresses.
#[repr(C, packed)]
pub struct ShapeContexts {
    counts: [u32; 6],
    arrays: [*mut c_void; 6],
    apu: [u32; 6],
}

/// Most contexts of each kind.
const MOST_CONTEXTS: [u32; 6] = [0x1000, 0x1000, 0x1000, 0x1000, 0x200, 0xE00];
/// Bytes per context of the first four kinds, and their alignment; XMA
/// (64 bytes) and PCM (32) contexts share one block aligned to 64.
const CONTEXT_BYTES: [(u32, u32); 4] = [(32, 0x20), (160, 0x10), (32, 0x20), (16, 0x10)];

/// Frees the context blocks in heap 2 and lets go of the device, then
/// resets the XMA decoder.
fn free_contexts(mut device: MutexGuard<'_, Device>) {
    if let Some(contexts) = device.contexts.take()
        && let Some(heaps) = device.heaps.as_mut()
    {
        for offset in contexts.blocks.into_iter().flatten() {
            heaps[1].give_back(offset);
        }
    }
    drop(device);
    reset_xma();
}

fn reset_xma() {
    *XMA.write().unwrap_or_else(|e| e.into_inner()) = Xma::EMPTY;
}

/// Gives the device its SHAPE context arrays, from heap 2. A second request
/// while arrays are given frees them and fails, as on the console.
pub extern "system" fn AcpHalAllocateShapeContexts(contexts: *mut ShapeContexts) -> HRESULT {
    let mut device = device();
    if let Err(e) = device.open() {
        return e;
    }
    if device.contexts.is_some() {
        free_contexts(device);
        return CONTEXTS_ALLOCATED;
    }
    if contexts.is_null() {
        return E_INVALIDARG;
    }
    // SAFETY: the title passes its request; packed fields are copied out.
    let counts = unsafe { std::ptr::addr_of!((*contexts).counts).read_unaligned() };
    if counts.iter().zip(MOST_CONTEXTS).any(|(&c, most)| c > most)
        || !((counts[0] == 0 && counts[5] == 0) || counts[0] == counts[4] + counts[5])
    {
        return E_INVALIDARG;
    }
    let mut arrays: [*mut c_void; 6] = [std::ptr::null_mut(); 6];
    let mut addresses = [0u32; 6];
    let mut given = Contexts { blocks: [None; 5] };
    let heaps = device.heaps.as_mut().expect("opened above");
    let heap = &mut heaps[1];
    let mut failed = false;
    for i in 0..5 {
        let (count, bytes, align) = if i < 4 {
            (counts[i], counts[i] * CONTEXT_BYTES[i].0, CONTEXT_BYTES[i].1)
        } else {
            (counts[4] + counts[5], (counts[5] + 2 * counts[4]) * 32, 0x40)
        };
        if count == 0 {
            continue;
        }
        let Some(offset) = heap.take(bytes, align) else {
            failed = true;
            break;
        };
        given.blocks[i] = Some(offset);
        let (host, apu) = (heap.host + offset as usize, heap.apu + offset);
        if i < 4 {
            (arrays[i], addresses[i]) = (host as *mut c_void, apu);
        } else {
            if counts[4] != 0 {
                (arrays[4], addresses[4]) = (host as *mut c_void, apu);
            }
            if counts[5] != 0 {
                arrays[5] = (host + counts[4] as usize * 64) as *mut c_void;
                // The console adds the XMA count itself, not its bytes.
                addresses[5] = apu + counts[4];
            }
        }
    }
    device.contexts = Some(given);
    if failed {
        free_contexts(device);
        return E_OUTOFMEMORY;
    }
    drop(device);
    // SAFETY: as above.
    unsafe {
        std::ptr::addr_of_mut!((*contexts).arrays).write_unaligned(arrays);
        std::ptr::addr_of_mut!((*contexts).apu).write_unaligned(addresses);
    }
    let mut xma = XMA.write().unwrap_or_else(|e| e.into_inner());
    xma.array = arrays[4] as usize;
    xma.voices = (0..counts[4]).map(|_| Mutex::new(None)).collect();
    S_OK
}

pub extern "system" fn AcpHalReleaseShapeContexts() -> HRESULT {
    let device = device();
    if device.contexts.is_none() {
        let mut device = device;
        if device.close_if_idle() {
            drop(device);
            reset_xma();
        }
        return NO_CONTEXTS;
    }
    if device.connected != 0 {
        return STILL_CONNECTED;
    }
    free_contexts(device);
    let mut device = self::device();
    if device.close_if_idle() {
        drop(device);
        reset_xma();
    }
    S_OK
}

// The XMA decoder.
//
// An XMA context (16 words in the shared context array) names up to two
// input buffers of 2 KiB packets, a ring of 256-byte output blocks the
// hardware fills with 16-bit PCM, and the read offset the title advances
// by command. Words and bit fields, as titles write them:
// 0: input 0 size in packets (0-11), loop count (12-19, 255 forever),
//    valid inputs (20-21), output size in blocks (22-26), write block
//    (27-31, hardware);
// 1: input 1 size (0-11), loop end subframe (12-13), loop start subframes
//    to skip (17-19), subframes to skip at the start (24-26), sample rate
//    (27-28: 24, 32, 44.1, 48 kHz), stereo (29), output writable (31);
// 2: read position in bits (0-25), error status (26-30), error (31);
// 3: loop start in bits (0-25); 4: loop end in bits (0-25), current input
//    (31); 5, 6: input addresses; 7: output address; 8: overlap buffer;
//    9: read block (0-4).
// Titles read back only the write block, "writable" and the read block,
// and take whatever lies between read and write.
// shortcut: each enabled context decodes from a cold state and the overlap
// buffer is ignored (titles copy it between consecutive contexts of one
// voice); keep decoder state there if a title's seams turn out audible.

use victory_formats::xma;

const ENABLE_XMA: u32 = 5;
const ENABLE_XMAS: u32 = 6;
const DISABLE_XMA: u32 = 7;
const DISABLE_XMAS: u32 = 8;
const ADVANCE_XMA_READ: u32 = 18;

const BLOCK_BYTES: usize = 256;
const SAMPLES_PER_BLOCK: usize = BLOCK_BYTES / 2;
/// Error status: the input ran out.
const INPUT_INVALID: u32 = 4;

struct Voice {
    decoder: Option<xma::Decoder>,
    pcm: VecDeque<i16>,
    /// Samples per channel still to drop (start or loop subframe skips).
    skip: usize,
    ended: bool,
    trace: Trace,
}

/// What a voice did, for the debug log when it is disabled.
struct Trace {
    since: Instant,
    /// Samples per channel written to the ring.
    produced: usize,
    /// Blocks the title took (command 18), how often, and how often it
    /// found the ring less than full.
    taken: usize,
    takes: u32,
    partial_takes: u32,
    frames: u32,
    bad_frames: u32,
    /// The loudest sample written.
    peak: i16,
    /// Milliseconds after enabling when the input ran out.
    ended_ms: Option<u128>,
}

struct Xma {
    /// Host address of the context array.
    array: usize,
    /// One entry per context, `Some` while enabled. Each has its own lock,
    /// so a command for one voice never waits for others to decode (the
    /// title sends them from its time-critical mixer thread).
    voices: Vec<Mutex<Option<Voice>>>,
}

impl Xma {
    const EMPTY: Xma = Xma {
        array: 0,
        voices: Vec::new(),
    };
}

/// Written only when the context arrays are allocated or released.
static XMA: RwLock<Xma> = RwLock::new(Xma::EMPTY);

fn xma() -> RwLockReadGuard<'static, Xma> {
    XMA.read().unwrap_or_else(|e| e.into_inner())
}

/// A context's words in title memory.
#[derive(Clone, Copy)]
struct Context(*mut u32);

impl Context {
    fn get(self, word: usize) -> u32 {
        // SAFETY: inside the context array; the title reads and writes it
        // concurrently, like the hardware's.
        unsafe { self.0.add(word).read_volatile() }
    }

    fn set(self, word: usize, value: u32) {
        // SAFETY: as above.
        unsafe { self.0.add(word).write_volatile(value) }
    }

    fn field(self, word: usize, shift: u32, bits: u32) -> u32 {
        (self.get(word) >> shift) & ((1 << bits) - 1)
    }

    fn set_field(self, word: usize, shift: u32, bits: u32, value: u32) {
        let mask = ((1u32 << bits) - 1) << shift;
        self.set(word, (self.get(word) & !mask) | ((value << shift) & mask));
    }

    fn channels(self) -> usize {
        1 + self.field(1, 29, 1) as usize
    }
}

impl Xma {
    fn context(&self, index: u32) -> Option<Context> {
        ((index as usize) < self.voices.len() && self.array != 0)
            .then(|| Context((self.array + index as usize * 64) as *mut u32))
    }

    fn enable(&self, index: u32) {
        let Some(context) = self.context(index) else {
            return;
        };
        let mut slot = lock(&self.voices[index as usize]);
        context.set_field(2, 26, 6, 0);
        tracing::debug!(
            index,
            words = ?(0..10).map(|w| format!("{:08x}", context.get(w))).collect::<Vec<_>>(),
            "XMA voice enabled"
        );
        #[cfg(feature = "diagnostics")]
        dump_input(index, context);
        *slot = Some(Voice {
            decoder: None,
            pcm: VecDeque::new(),
            skip: context.field(1, 24, 3) as usize * xma::SUBFRAME_SAMPLES,
            ended: false,
            trace: Trace {
                since: Instant::now(),
                produced: 0,
                taken: 0,
                takes: 0,
                partial_takes: 0,
                frames: 0,
                bad_frames: 0,
                peak: 0,
                ended_ms: None,
            },
        });
        drop(slot);
        wake_decoder();
    }

    fn disable(&self, index: u32) {
        let Some(context) = self.context(index) else {
            return;
        };
        let Some(voice) = lock(&self.voices[index as usize]).take() else {
            return;
        };
        let t = &voice.trace;
        tracing::debug!(
            index,
            ms = t.since.elapsed().as_millis(),
            produced = t.produced,
            taken = t.taken * SAMPLES_PER_BLOCK / context.channels(),
            takes = t.takes,
            partial_takes = t.partial_takes,
            frames = t.frames,
            bad_frames = t.bad_frames,
            peak = t.peak,
            input_ended_ms = ?t.ended_ms,
            read_bit = context.field(2, 0, 26),
            input_bits = context.field(0, 0, 12) * xma::PACKET_BYTES as u32 * 8,
            "XMA voice disabled"
        );
    }

    /// Moves the title's read block on, which frees output space.
    fn advance_read(&self, index: u32, blocks: u32) {
        let Some(context) = self.context(index) else {
            return;
        };
        // The decoder writes the same words.
        let mut slot = lock(&self.voices[index as usize]);
        let size = context.field(0, 22, 5).max(1);
        // The count is the title's: reduced first, so the sum stays small.
        let read = (context.field(9, 0, 5) + blocks % size) % size;
        context.set_field(9, 0, 5, read);
        if blocks > 0 {
            context.set_field(1, 31, 1, 1);
        }
        if let Some(voice) = &mut *slot {
            let t = &mut voice.trace;
            t.taken += blocks as usize;
            t.takes += 1;
            t.partial_takes += u32::from(blocks < size);
        }
        drop(slot);
        wake_decoder();
    }

    /// Fills every enabled context's output as far as it has room.
    fn pump(&self) {
        let start = Instant::now();
        let mut active = 0;
        for index in 0..self.voices.len() as u32 {
            if let (Some(context), Some(voice)) =
                (self.context(index), &mut *lock(&self.voices[index as usize]))
            {
                pump(context, voice);
                active += 1;
            }
        }
        let took = start.elapsed();
        if took.as_millis() >= 4 {
            tracing::debug!(active, ms = took.as_millis(), "slow XMA decoding pass");
        }
    }
}

fn pump(context: Context, voice: &mut Voice) {
    let size = context.field(0, 22, 5);
    if size == 0 {
        return;
    }
    while context.field(1, 31, 1) == 1 {
        if xma_refill() {
            // (codec-d/cool_fox fabb872a, `VICTORY_XMA_REFILL=1`.) A skipped
            // frame or an input boundary can advance the decoder without
            // producing samples: only whole blocks are published, but for the
            // padded tail at the end, and at most 8 empty steps are taken so
            // a broken looping voice holds neither its lock nor other voices.
            for _ in 0..8 {
                if voice.pcm.len() >= SAMPLES_PER_BLOCK || !decode(context, voice) {
                    break;
                }
            }
            if voice.pcm.is_empty() || (voice.pcm.len() < SAMPLES_PER_BLOCK && !voice.ended) {
                return;
            }
        } else if voice.pcm.len() < SAMPLES_PER_BLOCK && !decode(context, voice) {
            return;
        }
        // The write block is the title's too: taken modulo the ring before
        // the first write, which would otherwise land past the ring's end.
        let write = context.field(0, 27, 5) % size;
        let address = context.get(7) as usize + write as usize * BLOCK_BYTES;
        let Some(out) = heap_bytes(address as u32, BLOCK_BYTES) else {
            return;
        };
        for pair in out.as_chunks_mut::<2>().0 {
            let sample = voice.pcm.pop_front().unwrap_or(0);
            voice.trace.peak = voice.trace.peak.max(sample.saturating_abs());
            pair.copy_from_slice(&sample.to_le_bytes());
        }
        voice.trace.produced += SAMPLES_PER_BLOCK / context.channels();
        let write = (write + 1) % size;
        context.set_field(0, 27, 5, write);
        // The read block is the title's: taken modulo the ring like the
        // write block and `advance_read`'s. Compared as it is, one at or
        // past the end would never be met, and a stream that loops would
        // keep this voice (and its lock) filling forever.
        if write == context.field(9, 0, 5) % size {
            // Full: nothing more until the title advances its read block.
            context.set_field(1, 31, 1, 0);
        }
    }
}

/// Decodes the next frame of the current input into `voice.pcm`; false
/// when there is nothing left to decode.
/// Whether an ended XMA voice goes on when the title gives it input again,
/// and only whole blocks are published (`VICTORY_XMA_REFILL=1`, from
/// codec-d/cool_fox fabb872a; off by default).
fn xma_refill() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    // On by default here (the NVIDIA fork took codec-d/cool_fox's refill as
    // it is); VICTORY_XMA_REFILL=0 turns it off.
    *ON.get_or_init(|| std::env::var_os("VICTORY_XMA_REFILL").is_none_or(|v| v != "0"))
}

fn decode(context: Context, voice: &mut Voice) -> bool {
    let valid = context.field(0, 20, 2);
    if voice.ended {
        // (`VICTORY_XMA_REFILL=1`: the title can refill either input while
        // the context stays enabled; the voice goes on once a valid buffer is
        // there, its decoder's overlap reset by the flush at its end.)
        if !xma_refill() || context.field(0, 20, 2) == 0 {
            return false;
        }
        voice.ended = false;
        if context.field(2, 26, 5) == INPUT_INVALID {
            context.set_field(2, 26, 5, 0);
        }
    }
    let channels = context.channels();
    let mut current = context.field(4, 31, 1);
    if valid & (1 << current) == 0 {
        if valid & (1 << (1 - current)) == 0 {
            // Out of input: the overlap tail, then nothing.
            if let Some(decoder) = &mut voice.decoder {
                let mut tail = Vec::new();
                decoder.flush(&mut tail);
                voice.pcm.extend(tail);
            }
            voice.ended = true;
            voice.trace.ended_ms = Some(voice.trace.since.elapsed().as_millis());
            context.set_field(2, 26, 5, INPUT_INVALID);
            context.set_field(2, 31, 1, 1);
            return !voice.pcm.is_empty();
        }
        current = 1 - current;
        context.set_field(4, 31, 1, current);
        context.set_field(2, 0, 26, 0);
    }
    let (address, packets) = match current {
        0 => (context.get(5), context.field(0, 0, 12)),
        _ => (context.get(6), context.field(1, 0, 12)),
    };
    let Some(data) = heap_bytes(address & !0x7ff, packets as usize * xma::PACKET_BYTES) else {
        context.set_field(0, 20 + current, 1, 0);
        return true;
    };
    let mut at = context.field(2, 0, 26) as usize;
    if at < 32 {
        at = 32 + xma::packet(data, 0).map_or(0, |p| p.first_frame_bit as usize);
    }
    let Some(frame) = xma::read_frame(data, at) else {
        // This input is used up.
        context.set_field(0, 20 + current, 1, 0);
        return true;
    };
    let decoder = voice.decoder.get_or_insert_with(|| {
        const RATES: [u32; 4] = [24_000, 32_000, 44_100, 48_000];
        xma::Decoder::new(channels, RATES[context.field(1, 27, 2) as usize])
    });
    let mut pcm = Vec::with_capacity(xma::FRAME_SAMPLES * channels);
    voice.trace.frames += 1;
    if let Err(e) = decoder.decode(&frame.data, frame.bits, &mut pcm) {
        voice.trace.bad_frames += 1;
        if voice.trace.bad_frames <= 2 {
            tracing::debug!(
                at,
                bits = frame.bits,
                head = ?&frame.data[..frame.data.len().min(8)],
                %e,
                "XMA frame did not decode"
            );
        }
    }
    let mut next = frame.next.unwrap_or(0);
    let loops = context.field(0, 12, 8);
    let loop_end = context.field(4, 0, 26) as usize;
    let ends_loop = loop_end != 0
        && (frame.start == loop_end
            || (frame.start < loop_end && frame.next.is_none_or(|n| loop_end < n)));
    let looping = loops > 0 && ends_loop;
    if looping {
        let keep = (context.field(1, 12, 2) as usize + 1) * xma::SUBFRAME_SAMPLES * channels;
        pcm.truncate(keep);
        next = context.field(3, 0, 26) as usize;
        if loops != 255 {
            context.set_field(0, 12, 8, loops - 1);
        }
    }
    // What is still to skip is this frame's (the stream's first, or a
    // loop's first when it is its last too).
    let skip = (voice.skip * channels).min(pcm.len());
    voice.skip -= skip / channels;
    voice.pcm.extend(&pcm[skip..]);
    if looping {
        // The loop's own skip is of its first frame, decoded next: not of
        // this one, its last.
        voice.skip = context.field(1, 17, 3) as usize * xma::SUBFRAME_SAMPLES;
    }
    if next == 0 {
        context.set_field(0, 20 + current, 1, 0);
    }
    context.set_field(2, 0, 26, next as u32);
    true
}

/// `VICTORY_XMA_DUMP=<dir>`: saves the context words (`<n>-<index>.ctx`)
/// and input buffer 0 (`.xma`) of the first 32 voices enabled, to test
/// the decoder on a title's streams outside the title.
#[cfg(feature = "diagnostics")]
fn dump_input(index: u32, context: Context) {
    static COUNT: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
    let Some(dir) = std::env::var_os("VICTORY_XMA_DUMP") else {
        return;
    };
    let n = COUNT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    if n >= 32 {
        return;
    }
    let dir = std::path::PathBuf::from(dir);
    let words: Vec<u8> = (0..16).flat_map(|w| context.get(w).to_le_bytes()).collect();
    let input = heap_bytes(
        context.get(5) & !0x7ff,
        context.field(0, 0, 12) as usize * xma::PACKET_BYTES,
    );
    let _ = std::fs::create_dir_all(&dir);
    let _ = std::fs::write(dir.join(format!("{n:02}-{index}.ctx")), words);
    let _ = std::fs::write(dir.join(format!("{n:02}-{index}.xma")), input.unwrap_or_default());
}

/// Starts the decoding thread on the first enabled voice, and has it fill
/// rings now: a voice just enabled or read from.
fn wake_decoder() {
    static DECODER: OnceLock<std::thread::Thread> = OnceLock::new();
    DECODER
        .get_or_init(|| {
            std::thread::Builder::new()
                .name("victory XMA decoder".into())
                .spawn(|| {
                    loop {
                        xma().pump();
                        // A 256-byte output block lasts 2.7 ms at 48 kHz mono.
                        std::thread::park_timeout(std::time::Duration::from_millis(2));
                    }
                })
                .map_or_else(|_| std::thread::current(), |h| h.thread().clone())
        })
        .unpark();
}

/// Runs an XMA command; false for one this does not know.
fn xma_command(command: u32, word: impl Fn(usize) -> Option<u32>) -> bool {
    let xma = xma();
    match command {
        ENABLE_XMA => xma.enable(word(0).unwrap_or(u32::MAX)),
        DISABLE_XMA => xma.disable(word(0).unwrap_or(u32::MAX)),
        ENABLE_XMAS | DISABLE_XMAS => {
            for i in 0..xma.voices.len() as u32 {
                if word(i as usize / 32).unwrap_or(0) & (1 << (i % 32)) != 0 {
                    match command {
                        ENABLE_XMAS => xma.enable(i),
                        _ => xma.disable(i),
                    }
                }
            }
        }
        ADVANCE_XMA_READ => xma.advance_read(word(0).unwrap_or(u32::MAX), word(1).unwrap_or(0)),
        _ => return false,
    }
    true
}

// The audio coprocessor.

const LOAD_FLOWGRAPH: u32 = 1;
const REGISTER_MESSAGES: u32 = 2;
const UNREGISTER_MESSAGES: u32 = 3;

/// The interface's own errors (AcpHal.dll 0x1800029a0, 0x180002e70,
/// 0x180003130).
const ALREADY_CONNECTED: HRESULT = 0x8AC8_0009_u32 as i32;
const NOT_CONNECTED: HRESULT = 0x8AC8_000A_u32 as i32;
const SUBMIT_UNCONNECTED: HRESULT = 0x8000_000E_u32 as i32;
const E_UNEXPECTED: HRESULT = 0x8000_FFFF_u32 as i32;
/// Interfaces the device takes at once.
const INTERFACES: u32 = 4;
/// The highest command kind; 4 is the one that carries no data.
const LAST_COMMAND: u32 = 27;
const NO_DATA_COMMAND: u32 = 4;

const FLOWGRAPH_COMPLETED: u32 = 0x02;
const COMMAND_COMPLETED: u32 = 0x10;
const DISCONNECTED: u32 = 0x80;

/// Audio frames of 128 samples at 48 kHz.
const FRAMES_PER_SECOND: u64 = 375;

#[repr(C, packed)]
#[derive(Clone, Copy)]
struct Message {
    kind: u32,
    dropped: u32,
    usec: u32,
    data: [u8; 16],
}

struct State {
    connected: bool,
    /// The command ring, its completion words and the message ring, in
    /// heap 1 (offsets of their blocks), and the counts they were sized
    /// for.
    rings: [Option<u32>; 3],
    sized: (u32, u32),
    /// Message kinds the title registered for.
    mask: u32,
    messages: VecDeque<Message>,
    /// Queue size given to `connect`.
    capacity: usize,
    /// Messages lost to a full queue since the last one queued.
    dropped: u32,
    since: Instant,
}

impl State {
    fn usec(&self) -> u64 {
        self.since.elapsed().as_micros() as u64
    }

    fn frame(&self) -> u32 {
        (self.usec() * FRAMES_PER_SECOND / 1_000_000) as u32
    }

    /// Queues a registered message; a full queue drops it and counts it in
    /// the next one that fits.
    fn post(&mut self, kind: u32, data: &[u8]) {
        if self.mask & kind == 0 {
            return;
        }
        if self.messages.len() >= self.capacity {
            self.dropped = self.dropped.saturating_add(1);
            tracing::debug!(kind, dropped = self.dropped, "coprocessor message dropped");
            return;
        }
        let mut message = Message {
            kind,
            dropped: std::mem::take(&mut self.dropped),
            usec: self.usec() as u32,
            data: [0; 16],
        };
        message.data[..data.len()].copy_from_slice(data);
        self.messages.push_back(message);
    }
}

/// The interface object: its method table, then our state.
#[repr(C)]
pub struct Acp {
    vtable: &'static AcpMethods,
    state: Mutex<State>,
}

#[repr(C)]
struct AcpMethods {
    connect: extern "system" fn(*mut Acp, u32, u32) -> HRESULT,
    disconnect: extern "system" fn(*mut Acp) -> HRESULT,
    submit: extern "system" fn(*mut Acp, u32, u64, u32, *const c_void, u32) -> HRESULT,
    pop_message: extern "system" fn(*mut Acp, *mut Message) -> bool,
    message_count: extern "system" fn(*mut Acp) -> u32,
    release: extern "system" fn(*mut Acp),
}

static ACP_METHODS: AcpMethods = AcpMethods {
    connect,
    disconnect,
    submit,
    pop_message,
    message_count,
    release,
};

fn state(this: *mut Acp) -> MutexGuard<'static, State> {
    // SAFETY: title code calls methods on the object `AcpHalCreate` made,
    // which lives until `release`.
    lock(unsafe { &(*this).state })
}

/// Connects with rings for `commands` commands and `messages` messages,
/// taken from heap 1 as the console takes them (256 bytes per command, 12
/// per command for completions, 256 per message), kept across a
/// reconnection with the same counts.
extern "system" fn connect(this: *mut Acp, commands: u32, messages: u32) -> HRESULT {
    let mut state = state(this);
    if state.connected {
        return ALREADY_CONNECTED;
    }
    if commands == 0 {
        return E_INVALIDARG;
    }
    let mut device = device();
    let heaps = match device.open() {
        Ok(h) => h,
        Err(e) => return e,
    };
    let heap = &mut heaps[0];
    let mut free = |offset: &mut Option<u32>| {
        if let Some(o) = offset.take() {
            heap.give_back(o);
        }
    };
    if state.rings[0].is_none() || state.sized.0 != commands {
        free(&mut state.rings[0]);
        free(&mut state.rings[1]);
    }
    if state.rings[2].is_none() || state.sized.1 != messages {
        free(&mut state.rings[2]);
    }
    let sizes = [commands.checked_mul(256), commands.checked_mul(12), messages.checked_mul(256)];
    let mut failed = false;
    for (ring, size) in state.rings.iter_mut().zip(sizes) {
        match size {
            Some(0) => {}
            Some(size) if ring.is_none() => match heaps[0].take(size, 256) {
                Some(o) => *ring = Some(o),
                None => failed = true,
            },
            Some(_) => {}
            None => failed = true,
        }
    }
    if failed || device.connected >= INTERFACES {
        let heap = &mut device.heaps.as_mut().expect("opened above")[0];
        for ring in &mut state.rings {
            if let Some(o) = ring.take() {
                heap.give_back(o);
            }
        }
        return if failed { E_OUTOFMEMORY } else { E_UNEXPECTED };
    }
    device.connected += 1;
    state.sized = (commands, messages);
    state.connected = true;
    state.capacity = messages as usize;
    S_OK
}

extern "system" fn disconnect(this: *mut Acp) -> HRESULT {
    let mut state = state(this);
    if !state.connected {
        return NOT_CONNECTED;
    }
    state.post(DISCONNECTED, &[]);
    state.connected = false;
    drop(state);
    device().connected -= 1;
    S_OK
}

/// Done at once: message registrations take effect, a flowgraph completes
/// unprocessed, a command with an id is acknowledged, and its notification
/// word is set.
extern "system" fn submit(
    this: *mut Acp,
    command: u32,
    id: u64,
    _frame: u32,
    data: *const c_void,
    notification: u32,
) -> HRESULT {
    static SEEN: Mutex<u64> = Mutex::new(0);
    if !state(this).connected {
        return SUBMIT_UNCONNECTED;
    }
    if (data.is_null() && command != NO_DATA_COMMAND)
        || (!data.is_null() && (command == NO_DATA_COMMAND || !(1..=LAST_COMMAND).contains(&command)))
    {
        return E_INVALIDARG;
    }
    if command < 64 && *lock(&SEEN) & 1 << command == 0 {
        *lock(&SEEN) |= 1 << command;
        tracing::info!(command, "first coprocessor command of this kind");
    }
    let mut state = state(this);
    let word = |at: usize| -> Option<u32> {
        // SAFETY: the command's data starts with these u32 fields.
        (!data.is_null()).then(|| unsafe { data.cast::<u32>().add(at).read_unaligned() })
    };
    match command {
        REGISTER_MESSAGES => state.mask |= word(0).unwrap_or(0),
        UNREGISTER_MESSAGES => state.mask &= !word(0).unwrap_or(0),
        LOAD_FLOWGRAPH => {
            let flowgraph = word(1).unwrap_or(0);
            state.post(FLOWGRAPH_COMPLETED, &flowgraph.to_le_bytes());
        }
        _ if xma_command(command, word) => {}
        // shortcut: context updates and the other SHAPE units' commands are
        // acknowledged but not carried out; add them when a title sends
        // them (each kind is logged once above).
        _ => {}
    }
    if id != 0 {
        let mut done = [0u8; 16];
        done[..4].copy_from_slice(&command.to_le_bytes());
        done[4..12].copy_from_slice(&id.to_le_bytes());
        done[12..].copy_from_slice(&state.frame().to_le_bytes());
        state.post(COMMAND_COMPLETED, &done);
    }
    if notification != 0
        && let Some(word) = heap_unmap(notification)
    {
        // SAFETY: a u32 in a heap block, which the title reads as volatile.
        unsafe { word.cast::<u32>().write_volatile(1) };
    }
    S_OK
}

/// The oldest message, if there is one and room for it.
extern "system" fn pop_message(this: *mut Acp, out: *mut Message) -> bool {
    if out.is_null() {
        return false;
    }
    let Some(message) = state(this).messages.pop_front() else {
        return false;
    };
    // SAFETY: the title passes room for one message.
    unsafe { out.write_unaligned(message) };
    true
}

extern "system" fn message_count(this: *mut Acp) -> u32 {
    state(this).messages.len() as u32
}

/// Disconnects if connected, gives back the rings and frees the object.
extern "system" fn release(this: *mut Acp) {
    let _ = disconnect(this);
    let rings = std::mem::take(&mut state(this).rings);
    let mut device = device();
    if let Some(heaps) = device.heaps.as_mut() {
        for o in rings.into_iter().flatten() {
            heaps[0].give_back(o);
        }
    }
    let reset = device.close_if_idle();
    drop(device);
    if reset {
        reset_xma();
    }
    // SAFETY: made by `AcpHalCreate` with `Box::into_raw`; released once.
    drop(unsafe { Box::from_raw(this) });
}

pub extern "system" fn AcpHalCreate(out: *mut *mut Acp) -> HRESULT {
    if out.is_null() {
        return E_INVALIDARG;
    }
    let acp = Box::new(Acp {
        vtable: &ACP_METHODS,
        state: Mutex::new(State {
            connected: false,
            rings: [None; 3],
            sized: (0, 0),
            mask: 0,
            messages: VecDeque::new(),
            capacity: 0,
            dropped: 0,
            since: Instant::now(),
        }),
    });
    // SAFETY: checked non-null.
    unsafe { out.write(Box::into_raw(acp)) };
    S_OK
}

pub(crate) fn module() -> HleModule {
    HleModule {
        name: "audio",
        dlls: &["acphal.dll"],
        exports: vec![
            hle_export!(AcpHalAllocateShapeContexts),
            hle_export!(AcpHalReleaseShapeContexts),
            hle_export!(AcpHalCreate),
            hle_export!(ApuAlloc),
            hle_export!(ApuCreateHeap),
            hle_export!(ApuFree),
            hle_export!(ApuHeapGetState),
            hle_export!(ApuIsVirtualAddressValid),
            hle_export!(ApuMapApuAddress),
            hle_export!(ApuMapVirtualAddress),
        ],
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn heap_maps_every_byte_of_a_block() {
        let _serial = fresh();
        let (mut ptr, mut apu) = (std::ptr::null_mut(), 0);
        assert_eq!(ApuAlloc(&mut ptr, &mut apu, 0x100, 128, 1), S_OK);
        // Alignments below 256 become 256.
        assert!(apu != 0 && apu.is_multiple_of(256));
        let inner = ptr.cast::<u8>().wrapping_add(0x40).cast_const().cast();
        assert_eq!(ApuMapVirtualAddress(inner), apu + 0x40);
        assert!(ApuIsVirtualAddressValid(ptr, 128));
        // Any alignment, power of two or not, is a mask of `align - 1`.
        assert!(ApuIsVirtualAddressValid(ptr, 0));
        assert_eq!(ApuIsVirtualAddressValid(inner, 3), (apu + 0x40) & 2 == 0);
        assert_eq!(ApuMapApuAddress(apu + 4), ptr.cast::<u8>().wrapping_add(4).cast());
        ApuFree(ptr);
        assert_eq!(ApuMapVirtualAddress(ptr), 0);
        for (align, heap) in [(6, 1), (2, 1), (0, 1), (256, 0), (256, 3)] {
            ptr = 1 as *mut c_void;
            assert_eq!(ApuAlloc(&mut ptr, &mut apu, 0x100, align, heap), E_INVALIDARG);
            assert_eq!(ptr.is_null(), heap != 0 && heap != 3, "cleared unless the heap is bad");
        }
        assert_eq!(ApuAlloc(&mut ptr, &mut apu, 0, 256, 1), E_INVALIDARG);
    }

    /// The context arrays and the heap exist once per process: tests that
    /// use them run one at a time.
    static CONTEXT_TESTS: Mutex<()> = Mutex::new(());

    /// One test at a time, each with the device as a new process has it.
    fn fresh() -> MutexGuard<'static, ()> {
        let serial = lock(&CONTEXT_TESTS);
        {
            let mut d = device();
            d.heaps = None;
            d.contexts = None;
            d.connected = 0;
            d.cached = CACHED_DEFAULT;
            d.non_cached = NON_CACHED_DEFAULT;
        }
        reset_xma();
        serial
    }

    fn state(heap: u32) -> HeapState {
        let mut s = HeapState::default();
        assert_eq!(ApuHeapGetState(&mut s, heap), S_OK);
        s
    }

    /// The heap as AcpHal.dll keeps it (heap allocator at 0x180005038,
    /// free at 0x18000532c, state at ApuHeapGetState): best fit, the
    /// device's own three blocks first, the console's padding, counters.
    #[test]
    fn heap_counts_and_fits_as_the_console() {
        let _serial = fresh();
        // A closed device has no heaps; sizing it opens it.
        assert_eq!(ApuHeapGetState(&mut HeapState::default(), 1), NO_HEAP);
        assert_eq!(ApuCreateHeap(0, 0), E_INVALIDARG);
        assert_eq!(ApuCreateHeap(0x10_0000, 0x1_0000), S_OK);
        assert_eq!(ApuCreateHeap(0x10_0000, 0), ALREADY_OPEN);
        assert_eq!(ApuHeapGetState(std::ptr::null_mut(), 1), E_INVALIDARG);
        assert_eq!(ApuHeapGetState(&mut HeapState::default(), 3), E_INVALIDARG);
        // The device's blocks: 0x1000 at 0x100 (padded by 0x100: the start
        // is aligned), then 0x14 and 8 at 256 (each padded by 256).
        assert_eq!(
            state(1),
            HeapState {
                free: 0x10_0000 - 0x1100 - 0x200 - 0x200,
                used: 0x1200,
                overhead: 0x300,
                largest: 0x10_0000 - 0x1500,
                blocks: 3,
            }
        );
        assert_eq!(
            state(2),
            HeapState {
                free: 0x1_2000,
                largest: 0x1_2000,
                ..HeapState::default()
            }
        );
        let alloc = |size, align, heap| {
            let (mut p, mut apu) = (std::ptr::null_mut(), 0);
            assert_eq!(ApuAlloc(&mut p, &mut apu, size, align, heap), S_OK);
            (p, apu)
        };
        let (a, apu_a) = alloc(0x1000, 0x1000, 2);
        let (b, apu_b) = alloc(0x300, 4, 2);
        let (c, _) = alloc(0x100, 4, 2);
        assert!(apu_a.is_multiple_of(0x1000) && apu_b.is_multiple_of(0x100));
        let s = state(2);
        // 0x1000 + its pad, 0x300 and 0x100 + 256 each.
        assert_eq!((s.used, s.overhead, s.blocks), (0x1400, 0x1000 + 0x200, 3));
        // Freeing `b` leaves a 0x400 hole; the best fit for 0x200 is that
        // hole, not the big rest of the heap.
        ApuFree(b);
        let (d, apu_d) = alloc(0x200, 4, 2);
        assert_eq!(apu_d, apu_b);
        assert_eq!(
            ApuMapApuAddress(apu_a + 8),
            a.cast::<u8>().wrapping_add(8).cast()
        );
        assert!(ApuMapApuAddress(0).is_null());
        // Out of memory: outputs cleared.
        let (mut p, mut apu) = (1 as *mut c_void, 1);
        assert_eq!(ApuAlloc(&mut p, &mut apu, 0x10_0000, 4, 2), E_OUTOFMEMORY);
        assert!(p.is_null() && apu == 0);
        for p in [a, c, d] {
            ApuFree(p);
        }
        // Everything back: the device closes and may be sized again.
        assert_eq!(ApuHeapGetState(&mut HeapState::default(), 2), NO_HEAP);
        assert_eq!(ApuCreateHeap(CACHED_DEFAULT, 0), S_OK);
        let (p, _) = alloc(16, 16, 1);
        ApuFree(p);
        assert_eq!(ApuHeapGetState(&mut HeapState::default(), 1), NO_HEAP);
    }

    /// The binary-search map gives the console walk's answer at every
    /// offset, after allocations of mixed alignment and frees.
    #[test]
    fn map_agrees_with_the_console_walk() {
        let mut a = Arena::new(0x1_0000, 0x4_0000).unwrap();
        let mut live = Vec::new();
        let mut seed = 7u32;
        for round in 0..300 {
            seed = seed.wrapping_mul(1_103_515_245).wrapping_add(12345);
            if live.len() > 8 && seed % 3 == 0 {
                let user = live.swap_remove((seed as usize >> 8) % live.len());
                assert!(a.give_back(user));
            } else {
                let align = [4, 256, 0x400, 0x1000][(seed >> 4) as usize % 4];
                if let Some(u) = a.take(0x40 + (seed >> 12) % 0x900, align) {
                    live.push(u);
                }
            }
            if round % 50 == 49 {
                for offset in (0..0x4_0000).step_by(0x10) {
                    assert_eq!(a.claims(offset), a.walk(offset), "offset {offset:#x}");
                }
            }
        }
    }

    /// The interface as AcpHal.dll's methods check it (connect
    /// 0x1800029a0, disconnect 0x180002e70, submit 0x180003130, pop
    /// 0x180003060, release via 0x180002854).
    #[test]
    fn interface_rules_as_the_console() {
        let _serial = fresh();
        let mut acps = [std::ptr::null_mut(); 5];
        for a in &mut acps {
            assert_eq!(AcpHalCreate(a), S_OK);
        }
        assert_eq!(AcpHalCreate(std::ptr::null_mut()), E_INVALIDARG);
        let acp = acps[0];
        // SAFETY: just created.
        let m = unsafe { (*acp).vtable };
        let word = 0u32;
        let data = (&raw const word).cast();
        assert_eq!((m.disconnect)(acp), NOT_CONNECTED);
        assert_eq!((m.submit)(acp, REGISTER_MESSAGES, 0, 0, data, 0), SUBMIT_UNCONNECTED);
        assert_eq!((m.connect)(acp, 0, 4), E_INVALIDARG);
        assert_eq!((m.connect)(acp, 4, 2), S_OK);
        assert_eq!((m.connect)(acp, 4, 2), ALREADY_CONNECTED);
        // Rings in heap 1: 4 x 256, 4 x 12 (raised to 256) and 2 x 256
        // bytes, after the device's own three blocks.
        let s = state(1);
        assert_eq!((s.blocks, s.used - 0x1200), (6, 0x400 + 0x100 + 0x200));
        // Data rules: only command 4 goes without, kinds 1-27.
        assert_eq!((m.submit)(acp, REGISTER_MESSAGES, 0, 0, std::ptr::null(), 0), E_INVALIDARG);
        assert_eq!((m.submit)(acp, NO_DATA_COMMAND, 0, 0, data, 0), E_INVALIDARG);
        assert_eq!((m.submit)(acp, NO_DATA_COMMAND, 0, 0, std::ptr::null(), 0), S_OK);
        assert_eq!((m.submit)(acp, 0, 0, 0, data, 0), E_INVALIDARG);
        assert_eq!((m.submit)(acp, LAST_COMMAND + 1, 0, 0, data, 0), E_INVALIDARG);
        assert_eq!((m.submit)(acp, LAST_COMMAND, 7, 0, data, 0), S_OK);
        // A null output pops nothing.
        let mask = u32::MAX;
        assert_eq!((m.submit)(acp, REGISTER_MESSAGES, 9, 0, (&raw const mask).cast(), 0), S_OK);
        assert_eq!((m.message_count)(acp), 1);
        assert!(!(m.pop_message)(acp, std::ptr::null_mut()));
        assert_eq!((m.message_count)(acp), 1);
        // Four interfaces at once, not five.
        for &other in &acps[1..4] {
            // SAFETY: created above.
            assert_eq!(unsafe { ((*other).vtable.connect)(other, 1, 0) }, S_OK);
        }
        // SAFETY: as above.
        assert_eq!(unsafe { ((*acps[4]).vtable.connect)(acps[4], 1, 0) }, E_UNEXPECTED);
        // Releasing disconnects and gives the rings back; with nothing
        // left the device closes.
        for a in acps {
            // SAFETY: created above, released once.
            unsafe { ((*a).vtable.release)(a) };
        }
        assert_eq!(ApuHeapGetState(&mut HeapState::default(), 1), NO_HEAP);
    }

    /// Context requests as AcpHalAllocateShapeContexts (0x180003428)
    /// checks and sizes them.
    #[test]
    fn shape_contexts_as_the_console() {
        let _serial = fresh();
        let request = |counts| ShapeContexts {
            counts,
            arrays: [std::ptr::null_mut(); 6],
            apu: [0; 6],
        };
        for bad in [
            [0x1001, 0, 0, 0, 0, 0],
            [0, 0, 0, 0, 0x201, 0],
            [0, 0, 0, 0, 0, 0xE01],
            [3, 0, 0, 0, 1, 1],
            [0, 0, 0, 0, 0, 2],
        ] {
            assert_eq!(AcpHalAllocateShapeContexts(&mut request(bad)), E_INVALIDARG);
        }
        assert_eq!(AcpHalReleaseShapeContexts(), NO_CONTEXTS);
        let mut r = request([3, 2, 1, 4, 1, 2]);
        assert_eq!(AcpHalAllocateShapeContexts(&mut r), S_OK);
        let (arrays, apu) = (r.arrays, r.apu);
        assert!(arrays.iter().all(|p| !p.is_null()));
        // XMA (64 bytes) then PCM (32) in one block; the PCM address is
        // the XMA one plus the XMA count, as the console writes it.
        assert_eq!(arrays[5] as usize - arrays[4] as usize, 64);
        assert_eq!(apu[5], apu[4] + 1);
        for (i, size) in [96u32, 320, 32, 64].into_iter().enumerate() {
            assert_eq!(ApuMapVirtualAddress(arrays[i]), apu[i]);
            let last = arrays[i].cast::<u8>().wrapping_add(size as usize - 1).cast_const().cast();
            assert_eq!(ApuMapVirtualAddress(last), apu[i] + size - 1);
        }
        // A second request frees the first and fails.
        assert_eq!(AcpHalAllocateShapeContexts(&mut r), CONTEXTS_ALLOCATED);
        assert_eq!(AcpHalReleaseShapeContexts(), NO_CONTEXTS);
        assert_eq!(AcpHalAllocateShapeContexts(&mut request([1, 0, 0, 0, 1, 0])), S_OK);
        // Release refuses while an interface is connected.
        let mut acp = std::ptr::null_mut();
        assert_eq!(AcpHalCreate(&mut acp), S_OK);
        // SAFETY: just created.
        let m = unsafe { (*acp).vtable };
        assert_eq!((m.connect)(acp, 4, 4), S_OK);
        assert_eq!(AcpHalReleaseShapeContexts(), STILL_CONNECTED);
        assert_eq!((m.disconnect)(acp), S_OK);
        (m.release)(acp);
        assert_eq!(AcpHalReleaseShapeContexts(), S_OK);
    }

    /// Time per ApuAlloc/ApuFree pair with 2000 blocks live, and per
    /// ApuMapVirtualAddress of the last. `cargo test --release --
    /// --ignored bench --nocapture`.
    #[test]
    #[ignore]
    fn bench_heap() {
        let _serial = fresh();
        assert_eq!(ApuCreateHeap(256 << 20, 0), S_OK);
        let mut live = Vec::new();
        for i in 0..2000u32 {
            let (mut p, mut apu) = (std::ptr::null_mut(), 0);
            assert_eq!(ApuAlloc(&mut p, &mut apu, 256 + (i % 7) * 512, 256, 1), S_OK);
            live.push(p);
        }
        let rounds = 20_000;
        let start = std::time::Instant::now();
        for i in 0..rounds {
            let (mut p, mut apu) = (std::ptr::null_mut(), 0);
            assert_eq!(ApuAlloc(&mut p, &mut apu, 1024, 256, 1), S_OK);
            ApuFree(p);
            if i % 2 == 0 {
                let k = i % live.len();
                ApuFree(live[k]);
                assert_eq!(ApuAlloc(&mut live[k], &mut apu, 768, 256, 1), S_OK);
            }
        }
        let pair = start.elapsed().as_secs_f64() * 1e9 / rounds as f64;
        let last = *live.last().unwrap();
        let start = std::time::Instant::now();
        for _ in 0..rounds {
            assert_ne!(ApuMapVirtualAddress(last), 0);
        }
        let map = start.elapsed().as_secs_f64() * 1e9 / rounds as f64;
        println!("bench heap: {pair:.0} ns per alloc/free round, {map:.0} ns per map, 2000 blocks live");
        for p in live {
            ApuFree(p);
        }
    }

    /// Two packets of three silent mono frames each: a fixed one-subframe
    /// layout, no transmitted coefficients.
    fn silent_stream() -> Vec<u8> {
        let mut data = vec![0xffu8; 2 * xma::PACKET_BYTES];
        let mut put = |at: usize, value: u32, width: usize| {
            for i in 0..width {
                let (p, bit) = (at + i, (value >> (width - 1 - i)) & 1);
                data[p / 8] = (data[p / 8] & !(0x80 >> (p % 8))) | ((bit as u8) << (7 - p % 8));
            }
        };
        for packet in 0..2 {
            let base = packet * xma::PACKET_BYTES * 8;
            put(base, 3 << 26, 32);
            for frame in 0..3 {
                let at = base + 32 + frame * 31;
                put(at, 31, 15); // length
                put(at + 15, 0b10, 2); // fixed layout, one 512-sample subframe
                put(at + 17, 0, 8 + 1 + 3 + 2); // gain, no trim, empty subframe
            }
        }
        data
    }

    /// A read block past the end of the ring (6 on a ring of four) is block
    /// 2 of it: a stream that loops forever fills blocks 0 and 1 and the
    /// pass ends. It used to fill the ring for good, holding the voice.
    #[test]
    fn xma_read_blocks_past_the_ring_end_wrap() {
        let _serial = fresh();
        let mut request = ShapeContexts {
            counts: [0, 0, 0, 0, 4, 0],
            arrays: [std::ptr::null_mut(); 6],
            apu: [0; 6],
        };
        assert_eq!(AcpHalAllocateShapeContexts(&mut request), S_OK);
        let context = Context(request.arrays[4].cast::<u32>().wrapping_add(16));
        let (mut input, mut input_apu) = (std::ptr::null_mut(), 0);
        assert_eq!(ApuAlloc(&mut input, &mut input_apu, 0x1000, 0x800, 1), S_OK);
        let stream = silent_stream();
        // SAFETY: two packets were allocated.
        unsafe { std::ptr::copy_nonoverlapping(stream.as_ptr(), input.cast(), stream.len()) };
        let (mut output, mut output_apu) = (std::ptr::null_mut(), 0);
        assert_eq!(
            ApuAlloc(&mut output, &mut output_apu, 0x400, 0x100, 2),
            S_OK
        );
        // SAFETY: the ring was just allocated.
        unsafe { output.cast::<u8>().write_bytes(0x55, 0x400) };

        // As in `xma_contexts_decode_into_their_output_rings`, but looping
        // forever from the first frame to the last, and read block 6.
        context.set(0, 2 | 255 << 12 | 1 << 20 | 4 << 22);
        context.set(1, 1 << 31 | 3 << 27);
        context.set(2, 32);
        context.set(3, 32);
        context.set(4, (xma::PACKET_BYTES * 8 + 32 + 2 * 31) as u32);
        context.set(5, input_apu);
        context.set(7, output_apu);
        context.set(9, 6);

        let mut acp = std::ptr::null_mut();
        assert_eq!(AcpHalCreate(&mut acp), S_OK);
        // SAFETY: just created.
        let m = unsafe { (*acp).vtable };
        assert_eq!((m.connect)(acp, 0x1000, 0x1000), S_OK);
        let submit = |command: u32, words: &[u32]| {
            assert_eq!(
                (m.submit)(acp, command, 0xf1, u32::MAX, words.as_ptr().cast(), 0),
                S_OK
            );
        };
        submit(ENABLE_XMA, &[1]);
        // A pass on a thread of its own (the decoding thread runs them too),
        // so that one that never ends fails this test instead of hanging it.
        let (done, passed) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            xma().pump();
            let _ = done.send(());
        });
        let five = std::time::Duration::from_secs(5);
        let wait = || passed.recv_timeout(five).is_ok();
        let ended = wait();
        if !ended {
            // End it as a full ring would, to release the contexts below.
            context.set_field(1, 31, 1, 0);
            assert!(wait(), "the XMA pass did not end on a full ring either");
        }
        let (write, writable) = (context.field(0, 27, 5), context.field(1, 31, 1));
        // SAFETY: the ring.
        let ring = unsafe { std::slice::from_raw_parts(output.cast::<u8>(), 0x400) }.to_vec();

        submit(DISABLE_XMA, &[1]);
        assert_eq!((m.disconnect)(acp), S_OK);
        (m.release)(acp);
        ApuFree(input);
        ApuFree(output);
        assert_eq!(AcpHalReleaseShapeContexts(), S_OK);
        assert!(ended, "the XMA pass never ended");
        // Blocks 0 and 1 written (silence), 2 and 3 left for the title.
        assert_eq!((write, writable), (2, 0));
        assert!(ring[..0x200].iter().all(|&b| b == 0));
        assert!(ring[0x200..].iter().all(|&b| b == 0x55));
    }

    /// A write block past the end of the ring (6 on a ring of four) is block
    /// 2 of it: blocks 2, 3 and 0 are filled, up to the read block (1). It
    /// used to write block 6 first, past the ring's end.
    #[test]
    fn xma_write_blocks_past_the_ring_end_wrap() {
        let _serial = fresh();
        let mut request = ShapeContexts {
            counts: [0, 0, 0, 0, 4, 0],
            arrays: [std::ptr::null_mut(); 6],
            apu: [0; 6],
        };
        assert_eq!(AcpHalAllocateShapeContexts(&mut request), S_OK);
        let context = Context(request.arrays[4].cast::<u32>().wrapping_add(16));
        let (mut input, mut input_apu) = (std::ptr::null_mut(), 0);
        assert_eq!(ApuAlloc(&mut input, &mut input_apu, 0x1000, 0x800, 1), S_OK);
        let stream = silent_stream();
        // SAFETY: two packets were allocated.
        unsafe { std::ptr::copy_nonoverlapping(stream.as_ptr(), input.cast(), stream.len()) };
        let (mut output, mut output_apu) = (std::ptr::null_mut(), 0);
        assert_eq!(
            ApuAlloc(&mut output, &mut output_apu, 0x800, 0x100, 2),
            S_OK
        );
        // SAFETY: the allocation was just made: the ring (four blocks) and a guard
        // area after it, which the pass must not touch.
        unsafe { output.cast::<u8>().write_bytes(0x55, 0x800) };

        // As in `xma_contexts_decode_into_their_output_rings`, but looping
        // forever from the first frame to the last; write block 6, read block 1.
        context.set(0, 2 | 255 << 12 | 1 << 20 | 4 << 22 | 6 << 27);
        context.set(1, 1 << 31 | 3 << 27);
        context.set(2, 32);
        context.set(3, 32);
        context.set(4, (xma::PACKET_BYTES * 8 + 32 + 2 * 31) as u32);
        context.set(5, input_apu);
        context.set(7, output_apu);
        context.set(9, 1);

        let mut acp = std::ptr::null_mut();
        assert_eq!(AcpHalCreate(&mut acp), S_OK);
        // SAFETY: just created.
        let m = unsafe { (*acp).vtable };
        assert_eq!((m.connect)(acp, 0x1000, 0x1000), S_OK);
        let submit = |command: u32, words: &[u32]| {
            assert_eq!(
                (m.submit)(acp, command, 0xf1, u32::MAX, words.as_ptr().cast(), 0),
                S_OK
            );
        };
        submit(ENABLE_XMA, &[1]);
        // A pass on a thread of its own (the decoding thread runs them too),
        // so that one that never ends fails this test instead of hanging it.
        let (done, passed) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            xma().pump();
            let _ = done.send(());
        });
        let five = std::time::Duration::from_secs(5);
        let wait = || passed.recv_timeout(five).is_ok();
        let ended = wait();
        if !ended {
            // End it as a full ring would, to release the contexts below.
            context.set_field(1, 31, 1, 0);
            assert!(wait(), "the XMA pass did not end on a full ring either");
        }
        let (write, writable) = (context.field(0, 27, 5), context.field(1, 31, 1));
        // SAFETY: the ring.
        let ring = unsafe { std::slice::from_raw_parts(output.cast::<u8>(), 0x800) }.to_vec();

        submit(DISABLE_XMA, &[1]);
        assert_eq!((m.disconnect)(acp), S_OK);
        (m.release)(acp);
        ApuFree(input);
        ApuFree(output);
        assert_eq!(AcpHalReleaseShapeContexts(), S_OK);
        assert!(ended, "the XMA pass never ended");
        // Blocks 2, 3 and 0 written (silence), 1 left for the title, and
        // nothing past the ring.
        assert_eq!((write, writable), (1, 0));
        assert!(ring[0x400..].iter().all(|&b| b == 0x55), "written past the ring");
        let block = |n: usize| &ring[n * 0x100..(n + 1) * 0x100];
        assert!([0, 2, 3].iter().all(|&n| block(n).iter().all(|&b| b == 0)));
        assert!(block(1).iter().all(|&b| b == 0x55));
    }

    #[test]
    fn xma_contexts_decode_into_their_output_rings() {
        let _serial = fresh();
        let mut request = ShapeContexts {
            counts: [0, 0, 0, 0, 4, 0],
            arrays: [std::ptr::null_mut(); 6],
            apu: [0; 6],
        };
        assert_eq!(AcpHalAllocateShapeContexts(&mut request), S_OK);
        let context = Context(request.arrays[4].cast::<u32>().wrapping_add(16));

        let (mut input, mut input_apu) = (std::ptr::null_mut(), 0);
        assert_eq!(ApuAlloc(&mut input, &mut input_apu, 0x1000, 0x800, 1), S_OK);
        let stream = silent_stream();
        // SAFETY: two packets were allocated.
        unsafe { std::ptr::copy_nonoverlapping(stream.as_ptr(), input.cast(), stream.len()) };
        let (mut output, mut output_apu) = (std::ptr::null_mut(), 0);
        assert_eq!(
            ApuAlloc(&mut output, &mut output_apu, 0x400, 0x100, 2),
            S_OK
        );
        // SAFETY: the ring was just allocated.
        unsafe { output.cast::<u8>().write_bytes(0x55, 0x400) };

        // As the title sets one up: two packets, input 0 valid, a ring of
        // four blocks, writable, 48 kHz mono, reading from the first frame.
        context.set(0, 2 | 1 << 20 | 4 << 22);
        context.set(1, 1 << 31 | 3 << 27);
        context.set(2, 32);
        context.set(5, input_apu);
        context.set(7, output_apu);

        let mut acp = std::ptr::null_mut();
        assert_eq!(AcpHalCreate(&mut acp), S_OK);
        // SAFETY: just created.
        let m = unsafe { (*acp).vtable };
        assert_eq!((m.connect)(acp, 0x1000, 0x1000), S_OK);
        let submit = |command: u32, words: &[u32]| {
            assert_eq!(
                (m.submit)(acp, command, 0xf1, u32::MAX, words.as_ptr().cast(), 0),
                S_OK
            );
        };
        submit(ENABLE_XMA, &[1]);
        xma().pump();
        // Full: all four blocks written (silence), the write block wrapped
        // onto the read block, and the ring no longer writable.
        assert_eq!(context.field(0, 27, 5), 0);
        assert_eq!(context.field(1, 31, 1), 0);
        // SAFETY: the ring.
        let ring = unsafe { std::slice::from_raw_parts(output.cast::<u8>(), 0x400) };
        assert!(ring.iter().all(|&b| b == 0));

        // Read four blocks at a time until the input has run out and the
        // ring keeps room (the decoding thread may fill it too).
        for _ in 0..20 {
            submit(ADVANCE_XMA_READ, &[1, 4]);
            xma().pump();
            if context.field(2, 31, 1) == 1 && context.field(1, 31, 1) == 1 {
                break;
            }
        }
        // Six frames and the flushed tail: 3,328 samples, 26 blocks.
        let produced = |x: &Xma| lock(&x.voices[1]).as_ref().map(|v| v.trace.produced);
        assert_eq!(produced(&xma()), Some(26 * SAMPLES_PER_BLOCK));
        assert_eq!(context.field(0, 20, 2), 0, "input used up");
        assert_eq!(context.field(2, 26, 5), INPUT_INVALID);

        // A count near `u32::MAX` wraps the read block like any other
        // (3 + 0xffff_ffff blocks on a ring of four is block 2); the sum
        // used to overflow.
        context.set_field(9, 0, 5, 3);
        submit(ADVANCE_XMA_READ, &[1, u32::MAX]);
        assert_eq!(context.field(9, 0, 5), 2);

        submit(DISABLE_XMA, &[1]);
        assert_eq!(produced(&xma()), None);
        assert_eq!((m.disconnect)(acp), S_OK);
        (m.release)(acp);
        ApuFree(input);
        ApuFree(output);
        assert_eq!(AcpHalReleaseShapeContexts(), S_OK);
    }

    /// A loop over the first two frames, which ends after the second
    /// frame's second subframe and starts two subframes into the first.
    /// Each end of the loop is cut in its own frame.
    #[test]
    fn a_loop_skips_subframes_of_its_first_frame() {
        let _serial = fresh();
        let (mut input, mut input_apu) = (std::ptr::null_mut(), 0);
        assert_eq!(ApuAlloc(&mut input, &mut input_apu, 0x1000, 0x800, 1), S_OK);
        let stream = silent_stream();
        // SAFETY: two packets were allocated.
        unsafe { std::ptr::copy_nonoverlapping(stream.as_ptr(), input.cast(), stream.len()) };

        // Frames start at bits 32, 63 and 94 of each packet.
        let (first, second, third) = (32, 63, 94);
        let (end_subframe, skip_subframes) = (1, 2);
        let mut words = [0u32; 16];
        let context = Context(words.as_mut_ptr());
        // Two packets, one loop, input 0 valid; 48 kHz mono.
        context.set(0, 2 | 1 << 12 | 1 << 20 | 4 << 22);
        context.set(
            1,
            1 << 31 | 3 << 27 | end_subframe << 12 | skip_subframes << 17,
        );
        context.set(2, first);
        context.set(3, first);
        context.set(4, second);
        context.set(5, input_apu);
        let mut voice = Voice {
            decoder: None,
            pcm: VecDeque::new(),
            skip: 0,
            ended: false,
            trace: Trace {
                since: Instant::now(),
                produced: 0,
                taken: 0,
                takes: 0,
                partial_takes: 0,
                frames: 0,
                bad_frames: 0,
                peak: 0,
                ended_ms: None,
            },
        };
        let mut step = |expect_next: u32| {
            let before = voice.pcm.len();
            assert!(decode(context, &mut voice));
            assert_eq!(context.field(2, 0, 26), expect_next);
            voice.pcm.len() - before
        };
        let subframe = xma::SUBFRAME_SAMPLES;
        // Before the loop: the first frame whole.
        assert_eq!(step(second), 4 * subframe);
        // The loop's last frame: its first two subframes, then back.
        assert_eq!(step(first), 2 * subframe, "the loop's last frame");
        assert_eq!(context.field(0, 12, 8), 0, "one loop, taken");
        // The loop's first frame again: without its first two subframes.
        assert_eq!(step(second), 2 * subframe, "the loop's first frame");
        // No loops left: both frames whole, and on to the third.
        assert_eq!(step(third), 4 * subframe);
        ApuFree(input);
    }

    #[test]
    fn full_message_queues_count_drops() {
        let mut acp = std::ptr::null_mut();
        assert_eq!(AcpHalCreate(&mut acp), S_OK);
        // SAFETY: just created.
        let m = unsafe { (*acp).vtable };
        assert_eq!((m.connect)(acp, 16, 2), S_OK);
        let mask = COMMAND_COMPLETED;
        assert_eq!(
            (m.submit)(
                acp,
                REGISTER_MESSAGES,
                0,
                u32::MAX,
                (&raw const mask).cast(),
                0
            ),
            S_OK
        );
        // Command 12 carries 12 bytes.
        let payload = [0u32; 3];
        for id in 1..=4 {
            assert_eq!((m.submit)(acp, 12, id, u32::MAX, payload.as_ptr().cast(), 0), S_OK);
        }
        assert_eq!((m.message_count)(acp), 2);
        let mut message = Message {
            kind: 0,
            dropped: 0,
            usec: 0,
            data: [0; 16],
        };
        assert!((m.pop_message)(acp, &mut message) && (m.pop_message)(acp, &mut message));
        assert_eq!((m.submit)(acp, 12, 5, u32::MAX, payload.as_ptr().cast(), 0), S_OK);
        assert!((m.pop_message)(acp, &mut message));
        let dropped = message.dropped;
        assert_eq!(dropped, 2);
        assert_eq!((m.disconnect)(acp), S_OK);
        (m.release)(acp);
    }

    #[test]
    fn startup_as_fmod_does_it() {
        let _serial = fresh();
        // Context arrays: rate conversion, DMA and XMA.
        let mut request = ShapeContexts {
            counts: [0x200, 0, 0, 0x400, 0x200, 0],
            arrays: [std::ptr::null_mut(); 6],
            apu: [0; 6],
        };
        assert_eq!(size_of::<ShapeContexts>(), 0x60);
        assert_eq!(AcpHalAllocateShapeContexts(&mut request), S_OK);
        let arrays = request.arrays;
        assert!(!arrays[0].is_null() && !arrays[3].is_null() && !arrays[4].is_null());
        assert!(arrays[1].is_null());
        // SAFETY: 0x200 rate-conversion contexts of 32 bytes were allocated.
        unsafe { arrays[0].cast::<u8>().write_bytes(0, 0x200 << 5) };

        // Connect, register every message but frame starts, then wait for
        // the registration to be acknowledged.
        let mut acp = std::ptr::null_mut();
        assert_eq!(AcpHalCreate(&mut acp), S_OK);
        // SAFETY: just created.
        let m = unsafe { (*acp).vtable };
        assert_eq!((m.connect)(acp, 0x804, 0x804), S_OK);
        let mask = 0xfe_u32;
        let data = (&raw const mask).cast();
        assert_eq!(
            (m.submit)(acp, REGISTER_MESSAGES, 1, u32::MAX, data, 0),
            S_OK
        );
        let mut message = Message {
            kind: 0,
            dropped: 0,
            usec: 0,
            data: [0; 16],
        };
        assert!((m.pop_message)(acp, &mut message));
        let (kind, dropped) = (message.kind, message.dropped);
        assert_eq!((kind, dropped), (COMMAND_COMPLETED, 0));
        assert_eq!(
            u64::from_le_bytes(message.data[4..12].try_into().unwrap()),
            1
        );
        assert!(!(m.pop_message)(acp, &mut message));
        assert_eq!((m.disconnect)(acp), S_OK);
        assert_eq!((m.message_count)(acp), 1, "disconnected");
        (m.release)(acp);
        assert_eq!(AcpHalReleaseShapeContexts(), S_OK);
    }
}
