//! The host's acceleration structures for the title's ray tracing
//! (`VICTORY_RAY_QUERY=1`).
//!
//! The title walks its own bounding volume hierarchies, in the console's
//! node format, testing one node at a time with `image_bvh*_intersect_ray`,
//! which the translator emulates in shader code (`victory_shader::bvh`).
//! Below an instance, that walk is the costly part: dozens of box nodes for
//! each ray. Here the host makes, for each of the title's bottom-level
//! structures, a host acceleration structure of the same triangles (and a
//! top-level one holding just it, which a ray query needs), so that a
//! translated shader that tests such a structure's root node asks the card's
//! ray-tracing units instead (`victory_shader::emit::SLOT_RT`): a ray query
//! that finds the nearest four triangles, whose node pointers it returns as
//! the root's children. The title's own code then tests those triangles and
//! keeps or drops them as it would have.
//!
//! How the host learns of a structure: a shader that tests a root-shaped
//! node no entry of the table names notes the node's address
//! (`SLOT_RT_NOTES`). The host reads the notes before each submission,
//! copies the structure's pages back from the card's copies of title memory
//! (the title's builder makes and unpacks its structures on the card only:
//! the PC's memory has them packed or not at all) in the list of a
//! submission, and once that is done walks them on a thread of its own: a top-level structure (its leaves are instances) names its
//! bottom-level ones, which are walked in turn; a bottom-level one (its
//! leaves are triangles) becomes a host structure, built in a list that
//! runs first in a later submission, whose table entry that list writes
//! after the build. A structure whose pages a submission writes again (a
//! refit or a rebuild by the title's builder, or memory used for something
//! else) is walked and built again.
//!
//! The title's structures: a 128-byte header, then the root at byte 128;
//! node pointers are the node's byte offset from the structure's start in
//! 8-byte units with the node's kind in the low three bits
//! (`victory_shader::bvh`). An instance node keeps at byte 56 its
//! structure's root pointer (low 30 bits) and at byte 60 the structure's
//! address in 256-byte units.

use std::collections::{HashMap, HashSet};
use std::sync::mpsc::{Receiver, Sender, channel};
use std::sync::{Mutex, OnceLock};

use victory_shader::bvh;
use victory_shader::emit::{RT_NEGATIVE, RT_NOTES, RT_TABLE_SLOTS, rt_hash};
use windows::Win32::Graphics::Direct3D12::*;
use windows::Win32::Graphics::Dxgi::Common::*;
use windows::core::Interface;

/// Host structures at most; each takes two view slots at the end of every
/// host view heap.
const MOST: u32 = 8192;
/// View slots of [`MOST`] structures, past the slots of every host view
/// heap that has raw twins.
pub(crate) const VIEWS: u32 = 2 * MOST;
/// Nodes of one structure at most.
const MOST_NODES: usize = 1 << 22;
/// Submissions between looks for structures written again.
const LOOK_EVERY: u64 = 8;
/// Title memory read back for a structure first, and at most (four times
/// as much each time a walk leaves what was read).
const FIRST_WINDOW: u64 = 256 << 10;
const MOST_WINDOW: u64 = 16 << 20;
/// Structures read back in one list, at most.
const FETCHES: usize = 16;
/// A page of title memory as the card's copies keep them.
const PAGE: u64 = 1 << 16;

/// Diagnostics (`VICTORY_BVH_CHECK=1`): the structures the title's builder
/// unpacked on the card are read back now and then and checked: header,
/// pointers inside the structure, children's boxes inside their parent's,
/// triangles inside their box. One line each.
fn checking() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("VICTORY_BVH_CHECK").is_some_and(|v| v == "1"))
}

/// Repairs (`VICTORY_BVH_REPAIR=1`, which reads back as [`checking`] does):
/// the boxes of a structure the builder unpacked, made again from the
/// triangles under them, and the box nodes that differ written back to the
/// card's copy. The title's builder unpacks about half of its structures
/// with box bounds that do not hold their children (run chk2: 34 of 68,
/// some boxes of all four children the same, -52992 to 65504 and infinity;
/// each unpacked once, so not a second unpacking): rays miss what such a
/// box leaves out and walk everything a box of nonsense holds. shortcut:
/// the unpacking program's own fault is not found yet; this mends what it
/// leaves.
fn repairing() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("VICTORY_BVH_REPAIR").is_some_and(|v| v == "1"))
}

/// A read back for [`check`], not a structure to build: its root.
const CHECK: u64 = u64::MAX;

/// Whether shaders ask the card's ray-tracing units (`VICTORY_RAY_QUERY=1`).
pub(crate) fn on() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("VICTORY_RAY_QUERY").is_some_and(|v| v == "1"))
}

/// Host view heaps of `first` slots (the title's, theirs, the raw twins)
/// get [`VIEWS`] more at their end, from `first` on, for the structures'
/// views, and shaders translated from now on use them. Called once, before
/// the first shader is translated and the first view heap is made, when a
/// heap of `first + VIEWS` slots fits.
pub(crate) fn view_heaps(first: u32) {
    if !on() {
        return;
    }
    let Some(host) = crate::host::host() else { return };
    let mut o = D3D12_FEATURE_DATA_D3D12_OPTIONS5::default();
    // SAFETY: the query writes the structure of the given size.
    let tier = unsafe {
        host.device
            .CheckFeatureSupport(D3D12_FEATURE_D3D12_OPTIONS5, (&raw mut o).cast(), size_of_val(&o) as u32)
            .map(|()| o.RaytracingTier)
    };
    if !tier.as_ref().is_ok_and(|t| t.0 >= D3D12_RAYTRACING_TIER_1_1.0) {
        tracing::warn!(tier = ?tier, "ray queries: the host has no inline ray tracing; the title's structures are walked in shader code");
        return;
    }
    if rt().is_none() {
        return;
    }
    FIRST.set(first).ok();
    victory_shader::emit::set_rt_views(first);
    tracing::info!(first, most = MOST, "ray queries: the title's bottom-level structures get host acceleration structures");
}

/// Slots host view heaps with raw twins have past them for the structures'
/// views ([`view_heaps`]).
pub(crate) fn extra_views() -> u32 {
    if FIRST.get().is_some() { VIEWS } else { 0 }
}

static FIRST: OnceLock<u32> = OnceLock::new();

/// A host view heap of `size` slots was made, its CPU handles from `cpu` on,
/// `step` apart: the structures' views go into it too.
pub(crate) fn heap_made(cpu: usize, step: u32, size: u32) {
    let (Some(&first), Some(rt)) = (FIRST.get(), RT.get().and_then(Option::as_ref)) else { return };
    if size < first + VIEWS {
        return;
    }
    let mut s = lock(&rt.state);
    s.heaps.push((cpu, step));
    for k in s.kept.iter().flatten() {
        write_views(cpu, step, k.view, k.top_gpu, &k.side, k.triangles);
    }
}

/// The host view heap whose CPU handles start at `cpu` is going.
pub(crate) fn heap_gone(cpu: usize) {
    let Some(rt) = RT.get().and_then(Option::as_ref) else { return };
    lock(&rt.state).heaps.retain(|h| h.0 != cpu);
}

/// Diagnostics (`VICTORY_WALK_TRACE`): the walk records' buffer, made once,
/// and whether it was written to a file.
struct Walk {
    _buffer: ID3D12Resource,
    data: usize,
    gpu: u64,
    since: std::time::Instant,
    written: std::sync::atomic::AtomicBool,
}

// SAFETY: a mapping the struct owns.
unsafe impl Send for Walk {}
// SAFETY: as above.
unsafe impl Sync for Walk {}

fn walk_trace_buffer() -> Option<&'static Walk> {
    static WALK: OnceLock<Option<Walk>> = OnceLock::new();
    WALK.get_or_init(|| {
        victory_shader::emit::walk_trace()?;
        let host = crate::host::host()?;
        let bytes = u64::from(victory_shader::emit::WALK_BYTES);
        let (buffer, data) = host.shared_buffer(bytes)?;
        // SAFETY: a fresh mapping.
        unsafe { std::ptr::write_bytes(data, 0, bytes as usize) };
        // SAFETY: a live buffer.
        let gpu = unsafe { buffer.GetGPUVirtualAddress() };
        Some(Walk { _buffer: buffer, data: data as usize, gpu, since: std::time::Instant::now(), written: false.into() })
    })
    .as_ref()
}

/// The walk records' address, for the root view `SLOT_WALK`.
pub(crate) fn walk_gpu() -> Option<u64> {
    walk_trace_buffer().map(|w| w.gpu)
}

/// Writes the walk records to `walktrace.bin` beside the log, once, a while
/// into the run.
fn walk_dump() {
    let Some(w) = walk_trace_buffer() else { return };
    let secs: u64 = std::env::var("VICTORY_WALK_AFTER").ok().and_then(|v| v.parse().ok()).unwrap_or(200);
    // Recorded afresh for the last 30 s before the dump (the first frames
    // fill the buffer otherwise).
    static RESET: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
    if w.since.elapsed().as_secs() + 30 >= secs && !RESET.swap(true, std::sync::atomic::Ordering::AcqRel) {
        // SAFETY: the mapping's first dword, the count.
        unsafe { (*(w.data as *const std::sync::atomic::AtomicU32)).store(0, std::sync::atomic::Ordering::Release) };
        tracing::info!("walk trace: recording afresh");
    }
    if w.since.elapsed().as_secs() < secs || w.written.swap(true, std::sync::atomic::Ordering::AcqRel) {
        return;
    }
    let bytes = victory_shader::emit::WALK_BYTES as usize;
    // SAFETY: the mapping, read once (the card may still be writing).
    let data = unsafe { std::slice::from_raw_parts(w.data as *const u8, bytes) };
    let n = u32::from_le_bytes(data[..4].try_into().unwrap());
    let path = std::env::var("VICTORY_WALK_FILE").unwrap_or_else(|_| "walktrace.bin".into());
    let r = std::fs::write(&path, data);
    tracing::info!(records = n, path, ok = r.is_ok(), "walk trace written");
}

/// The table's address, for the root view `SLOT_RT`.
pub(crate) fn table_gpu() -> Option<u64> {
    RT.get().and_then(Option::as_ref).map(|r| r.table_gpu)
}

/// The notes' address, for the root view `SLOT_RT_NOTES`.
pub(crate) fn notes_gpu() -> Option<u64> {
    RT.get().and_then(Option::as_ref).map(|r| r.notes_gpu)
}

/// A host structure the table names.
struct Kept {
    /// The title structure's root node, and its start.
    root: u64,
    base: u64,
    /// Its first view slot (the second views the node pointers).
    view: u32,
    /// Title memory its nodes lie in.
    range: (u64, u64),
    /// The submission whose notes or looks it was walked after.
    walked_after: u64,
    top_gpu: u64,
    /// A hash of the triangles and node pointers it was built from.
    hash: u64,
    side: ID3D12Resource,
    triangles: u32,
    /// What the card reads: the structures, the triangles, the node
    /// pointers, the instance.
    _held: Vec<ID3D12Resource>,
}

/// What a walk found, made into host objects not yet built.
struct Made {
    root: u64,
    base: u64,
    hash: u64,
    range: (u64, u64),
    walked_after: u64,
    bottom: ID3D12Resource,
    top: ID3D12Resource,
    scratch: ID3D12Resource,
    vertices: ID3D12Resource,
    side: ID3D12Resource,
    instance: ID3D12Resource,
    triangles: u32,
}

enum Known {
    /// Being walked.
    Walking,
    /// A host structure: its index in `kept`.
    Kept(usize),
    /// Nothing for it (a top-level structure, or one that would not walk).
    Not,
}

struct State {
    known: HashMap<u64, Known>,
    kept: Vec<Option<Kept>>,
    /// Made, waiting for a list to build them.
    made: Vec<Made>,
    /// The table as the card will have it.
    table: Vec<[u32; 4]>,
    /// Entries to write in the next list.
    writes: Vec<(u32, [u32; 4])>,
    /// Host view heaps: CPU start, step.
    heaps: Vec<(usize, u32)>,
    lists: Vec<(ID3D12CommandAllocator, ID3D12GraphicsCommandList4, u64)>,
    /// Kept no more, each held until the card is past the submission named.
    retired: Vec<(u64, Kept)>,
    /// Scratch of builds, held as `retired`.
    scratch: Vec<(u64, ID3D12Resource)>,
    zeroed: bool,
    looked: u64,
    told: u64,
    /// Structures checked ([`checking`]).
    checked: HashSet<u64>,
    /// Box nodes to write back ([`repairing`]): title address, bytes.
    repairs: Vec<(u64, Vec<u8>)>,
    /// An instance of each bottom-level structure seen (by its address):
    /// its Transform and WorldToObject rows, for the reference check.
    instances: HashMap<u64, ([f32; 12], [f32; 12])>,
    /// Checked once, to check again: when.
    check_again: HashMap<u64, u64>,
    /// Structures to read back: root, start, after, bytes.
    to_fetch: Vec<(u64, u64, u64, u64)>,
    /// Being read back.
    fetching: Vec<Fetching>,
}

/// A structure's title memory being copied back from the card.
struct Fetching {
    root: u64,
    base: u64,
    after: u64,
    window: u64,
    first: u64,
    present: Vec<bool>,
    buffer: ID3D12Resource,
    /// The copies read from, held until the copy is done.
    _sources: Vec<ID3D12Resource>,
    /// The host fence's value once it is done.
    done_at: u64,
}

/// Read-back title memory: pages from `first` on.
struct Fetched {
    root: u64,
    base: u64,
    after: u64,
    window: u64,
    first: u64,
    present: Vec<bool>,
    data: Vec<u8>,
}

impl Fetched {
    /// `out.len()` bytes at title address `at`, if they were read back.
    fn read(&self, at: u64, out: &mut [u8]) -> bool {
        let Some(off) = at.checked_sub(self.first * PAGE) else { return false };
        let end = off + out.len() as u64;
        if end > self.data.len() as u64 {
            return false;
        }
        let (p0, p1) = ((off / PAGE) as usize, ((end - 1) / PAGE) as usize);
        if !(p0..=p1).all(|p| self.present.get(p).copied().unwrap_or(false)) {
            return false;
        }
        out.copy_from_slice(&self.data[off as usize..end as usize]);
        true
    }
}

struct Rt {
    table: ID3D12Resource,
    table_gpu: u64,
    zeros: ID3D12Resource,
    _notes: ID3D12Resource,
    notes: usize,
    notes_gpu: u64,
    jobs: Mutex<Sender<Fetched>>,
    state: Mutex<State>,
}

// SAFETY: D3D12 objects are free-threaded; the notes' mapping is read and
// written as atomics.
unsafe impl Send for Rt {}
// SAFETY: as above.
unsafe impl Sync for Rt {}

static RT: OnceLock<Option<Rt>> = OnceLock::new();

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

fn rt() -> Option<&'static Rt> {
    RT.get_or_init(|| {
        let host = crate::host::host()?;
        let bytes = u64::from(RT_TABLE_SLOTS) * 16;
        let table = host.uav_buffer(bytes)?;
        crate::host::name(&table, || "ray queries: the table".into());
        let (zeros, z) = host.upload_buffer(bytes)?;
        // SAFETY: a fresh mapping of `bytes` bytes.
        unsafe { std::ptr::write_bytes(z, 0, bytes as usize) };
        let (notes_buffer, notes) = host.shared_buffer(u64::from(RT_NOTES) * 16)?;
        crate::host::name(&notes_buffer, || "ray queries: the notes".into());
        // SAFETY: a fresh mapping.
        unsafe { std::ptr::write_bytes(notes, 0, RT_NOTES as usize * 16) };
        // SAFETY: live buffers.
        let (table_gpu, notes_gpu) = unsafe { (table.GetGPUVirtualAddress(), notes_buffer.GetGPUVirtualAddress()) };
        let (jobs, todo) = channel();
        std::thread::Builder::new().name("victory ray structures".into()).spawn(move || walker(todo)).ok()?;
        Some(Rt {
            table,
            table_gpu,
            zeros,
            _notes: notes_buffer,
            notes: notes as usize,
            notes_gpu,
            jobs: Mutex::new(jobs),
            state: Mutex::new(State {
                known: HashMap::new(),
                kept: Vec::new(),
                made: Vec::new(),
                table: vec![[0; 4]; RT_TABLE_SLOTS as usize],
                writes: Vec::new(),
                heaps: Vec::new(),
                lists: Vec::new(),
                retired: Vec::new(),
                scratch: Vec::new(),
                zeroed: false,
                looked: 0,
                told: 0,
                to_fetch: Vec::new(),
                checked: HashSet::new(),
                check_again: HashMap::new(),
                instances: HashMap::new(),
                repairs: Vec::new(),
                fetching: Vec::new(),
            }),
        })
    })
    .as_ref()
}

/// Writes a structure's two views at slot `view` of the heap at `cpu`.
fn write_views(cpu: usize, step: u32, view: u32, top: u64, side: &ID3D12Resource, triangles: u32) {
    let Some(host) = crate::host::host() else { return };
    let at = |slot: u32| D3D12_CPU_DESCRIPTOR_HANDLE { ptr: cpu + slot as usize * step as usize };
    let structure = D3D12_SHADER_RESOURCE_VIEW_DESC {
        Format: DXGI_FORMAT_UNKNOWN,
        ViewDimension: D3D12_SRV_DIMENSION_RAYTRACING_ACCELERATION_STRUCTURE,
        Shader4ComponentMapping: D3D12_DEFAULT_SHADER_4_COMPONENT_MAPPING,
        Anonymous: D3D12_SHADER_RESOURCE_VIEW_DESC_0 {
            RaytracingAccelerationStructure: D3D12_RAYTRACING_ACCELERATION_STRUCTURE_SRV { Location: top },
        },
    };
    let pointers = D3D12_SHADER_RESOURCE_VIEW_DESC {
        Format: DXGI_FORMAT_R32_TYPELESS,
        ViewDimension: D3D12_SRV_DIMENSION_BUFFER,
        Shader4ComponentMapping: D3D12_DEFAULT_SHADER_4_COMPONENT_MAPPING,
        Anonymous: D3D12_SHADER_RESOURCE_VIEW_DESC_0 {
            Buffer: D3D12_BUFFER_SRV { FirstElement: 0, NumElements: triangles.max(1), StructureByteStride: 0, Flags: D3D12_BUFFER_SRV_FLAG_RAW },
        },
    };
    // SAFETY: slots inside a live heap (`heap_made`; `heap_gone` takes it
    // out under the lock the callers hold).
    unsafe {
        host.device.CreateShaderResourceView(None, Some(&structure), at(view));
        host.device.CreateShaderResourceView(side, Some(&pointers), at(view + 1));
    }
}

/// What one walk of title memory found.
enum Walked {
    /// Triangles: three vertices each, and the node pointer of each.
    Bottom { vertices: Vec<[f32; 3]>, pointers: Vec<u32>, range: (u64, u64) },
    /// Instances: the root node of each one's structure, with its kind,
    /// and the structure's start.
    Top { roots: Vec<(u64, u64)> },
    /// A node past what was read back.
    Outside,
    Bad(&'static str),
}

fn dword(b: &[u8], at: usize) -> u32 {
    u32::from_le_bytes(b[at..at + 4].try_into().unwrap())
}

/// Walks the structure whose root node is at `root` (its kind in the low
/// three bits) and starts `base`.
fn walk(root: u64, base: u64, memory: &Fetched) -> Walked {
    fn read<const N: usize>(memory: &Fetched, at: u64) -> Option<[u8; N]> {
        let mut out = [0u8; N];
        memory.read(at, &mut out).then_some(out)
    }
    let kind = (root & 7) as u32;
    let root_at = root & !63;
    if root_at < base || (root_at - base) >> 6 > u64::from(u32::MAX >> 3) {
        return Walked::Bad("a root before its structure");
    }
    let root_pointer = (((root_at - base) >> 3) as u32) | kind;
    let mut todo = vec![root_pointer];
    let mut seen = HashSet::new();
    let (mut vertices, mut pointers, mut roots) = (Vec::new(), Vec::new(), Vec::new());
    let mut procedural = false;
    let (mut low, mut high) = (u64::MAX, 0u64);
    while let Some(p) = todo.pop() {
        if !seen.insert(p) {
            continue;
        }
        if seen.len() > MOST_NODES {
            return Walked::Bad("too many nodes");
        }
        let at = base + (u64::from(p >> 3) << 6);
        low = low.min(at);
        match p & 7 {
            4 | 5 => {
                let Some(node) = read::<16>(memory, at) else { return Walked::Outside };
                high = high.max(at + if p & 7 == 5 { 128 } else { 64 });
                for k in 0..4 {
                    let child = dword(&node, 4 * k);
                    if child != bvh::NONE {
                        todo.push(child);
                    }
                }
            }
            k @ 0..=3 => {
                let Some(node) = read::<64>(memory, at) else { return Walked::Outside };
                high = high.max(at + 64);
                let three = bvh::TRIANGLE_VERTICES[k as usize].map(|v| {
                    let o = 12 * v as usize;
                    [0, 1, 2].map(|c| f32::from_bits(dword(&node, o + 4 * c)))
                });
                // (Not finite or past any scene: inactive, as DXR has a
                // triangle with a NaN vertex.)
                let sane = three.iter().flatten().all(|c| c.is_finite() && c.abs() < 1.0e7);
                for v in three {
                    vertices.push(if sane { v } else { [f32::NAN; 3] });
                }
                pointers.push(p);
            }
            6 => {
                let Some(node) = read::<64>(memory, at) else { return Walked::Outside };
                high = high.max(at + 64);
                let mut whole = [0u8; 128];
                if memory.read(at, &mut whole) {
                    let f = |o: usize| f32::from_bits(dword(&whole, o));
                    let transform: [f32; 12] = std::array::from_fn(|i| f(4 * i));
                    let world_to_object: [f32; 12] = std::array::from_fn(|i| f(0x40 + 4 * i));
                    let blas = u64::from(dword(&whole, 0x3c)) << 8;
                    if let Some(rt) = RT.get().and_then(Option::as_ref) {
                        lock(&rt.state).instances.entry(blas).or_insert((transform, world_to_object));
                    }
                }
                let (pointer, structure) = (dword(&node, 56) & 0x3fff_ffff, dword(&node, 60));
                // Diagnostics: what instance nodes hold (the first few).
                static TOLD: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
                if TOLD.fetch_add(1, std::sync::atomic::Ordering::Relaxed) < 24 {
                    let mut whole = [0u8; 128];
                    let more = memory.read(at, &mut whole);
                    let words: Vec<String> = (0..if more { 32 } else { 16 }).map(|i| format!("{:08x}", dword(&whole[..], 4 * i).max(if more { 0 } else { dword(&node, 4 * i.min(15)) }))).collect();
                    tracing::info!(structure = format_args!("{base:#x}"), node = format_args!("{p:#x}"), bytes = words.join(" "), "ray queries: an instance node");
                }
                let start = u64::from(structure) << 8;
                let blas = start + (u64::from(pointer >> 3) << 6);
                roots.push((blas | u64::from(pointer & 7), start));
            }
            _ => procedural = true,
        }
    }
    if procedural && roots.is_empty() {
        // (The host structure would miss what the title's own code tests.)
        return Walked::Bad("a procedural primitive");
    }
    match (pointers.is_empty(), roots.is_empty()) {
        (false, true) => Walked::Bottom { vertices, pointers, range: (low, high) },
        (true, false) => Walked::Top { roots },
        (true, true) => Walked::Bad("no leaves"),
        (false, false) => Walked::Bad("triangles and instances"),
    }
}

/// A structure to walk: its root node (kind in the low bits), its start,
/// the submission its note or look came after, and whether a note named it.
/// A note's start is a guess (the root 128 bytes in), and the 64-bit form's
/// test for a root-shaped node passes one node in four: a note is taken
/// only for a top-level structure, whose instances say where their own
/// structures start and keep their roots, exactly.
/// The thread that walks structures read back and makes their host objects.
fn walker(todo: Receiver<Fetched>) {
    while let Ok(memory) = todo.recv() {
        let Some(rt) = rt() else { return };
        let (root, base, after) = (memory.root, memory.base, memory.after);
        instance_check(rt, base, &memory);
        // The root as the structure's header names it (rootNodePtr, 0x30).
        let root = {
            let mut h = [0u8; 64];
            if root != CHECK && memory.read(base, &mut h) && dword(&h, 0) & 0xffff == 6 {
                let p = dword(&h, 0x30);
                if matches!(p & 7, 4 | 5) { base + (u64::from(p >> 3) << 6) | u64::from(p & 7) } else { root }
            } else {
                root
            }
        };
        if root == CHECK {
            if checking() {
                check(&memory);
            }
            if repairing() {
                let runs = repair(&memory);
                if !runs.is_empty() {
                    lock(&rt.state).repairs.extend(runs);
                }
            }
            continue;
        }
        let walked = walk(root, base, &memory);
        if matches!(walked, Walked::Outside) {
            let mut s = lock(&rt.state);
            if memory.window < MOST_WINDOW {
                s.to_fetch.push((root, base, after, memory.window * 4));
            } else {
                tracing::info!(root = format_args!("{root:#x}"), "ray queries: a structure past what is read back, left to shader code");
                s.known.insert(root, Known::Not);
            }
            continue;
        }
        let made = match walked {
            Walked::Bottom { vertices, pointers, range } => {
                // What the card holds: sane or not (the first few hundred).
                static TOLD: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
                if TOLD.fetch_add(1, std::sync::atomic::Ordering::Relaxed) < 300 {
                    let broken = vertices.chunks(3).filter(|t| t[0][0].is_nan()).count();
                    let (mut lo, mut hi) = ([f32::MAX; 3], [f32::MIN; 3]);
                    for v in vertices.iter().filter(|v| !v[0].is_nan()) {
                        for c in 0..3 {
                            lo[c] = lo[c].min(v[c]);
                            hi[c] = hi[c].max(v[c]);
                        }
                    }
                    tracing::info!(root = format_args!("{root:#x}"), triangles = pointers.len(), broken, bytes = range.1 - range.0, ?lo, ?hi, "ray queries: a bottom-level structure read back");
                }
                // Walked again and the same: nothing to build.
                let hash = contents_hash(&vertices, &pointers);
                {
                    let s = lock(&rt.state);
                    if let Some(Known::Kept(i)) = s.known.get(&root)
                        && s.kept.get(*i).and_then(Option::as_ref).is_some_and(|k| k.hash == hash)
                    {
                        continue;
                    }
                }
                make(root, base, range, after, &vertices, &pointers).ok_or("no host objects")
            }
            Walked::Top { roots } => {
                let n = roots.len();
                // Roots the shaders take for one: 128 bytes into their structure.
                let shaped = roots.iter().filter(|(r, b)| (r & !63) == b + 128).count();
                let mut offsets: Vec<u64> = roots.iter().map(|(r, b)| (r & !7) - b).collect();
                offsets.sort_unstable();
                offsets.dedup();
                offsets.truncate(8);
                let mut s = lock(&rt.state);
                s.known.insert(root, Known::Not);
                let mut new = 0;
                for (r, b) in roots {
                    if !s.known.contains_key(&r) {
                        s.known.insert(r, Known::Walking);
                        s.to_fetch.push((r, b, after, FIRST_WINDOW));
                        new += 1;
                    }
                }
                place(&mut s, root, RT_NEGATIVE);
                tracing::info!(root = format_args!("{root:#x}"), instances = n, new, shaped, ?offsets, "ray queries: a top-level structure");
                continue;
            }
            Walked::Bad(why) => Err(why),
            Walked::Outside => unreachable!(),
        };
        let mut s = lock(&rt.state);
        match made {
            Ok(m) => s.made.push(m),
            Err(why) => {
                tracing::info!(root = format_args!("{root:#x}"), why, "ray queries: a structure left to shader code");
                s.known.insert(root, Known::Not);
            }
        }
    }
}

/// Checks a structure the builder unpacked, as the card holds it, and says
/// what it found.
fn check(memory: &Fetched) {
    static N: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
    let n = N.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let base = memory.base;
    let len = memory.window;
    let mut header = [0u8; 128];
    if !memory.read(base, &mut header) {
        tracing::info!(n, base = format_args!("{base:#x}"), "bvh check: header not on the card");
        return;
    }
    let d = |i: usize| dword(&header, 4 * i);
    if n < 12 {
        let words: Vec<String> = (0..32).map(|i| format!("{:08x}", d(i))).collect();
        tracing::info!(n, base = format_args!("{base:#x}"), header = words.join(" "), "bvh check: a header");
    }
    // The root: the pointer at byte 48 if it names a box, else a box at 128.
    let named = d(12) & 0x3fff_ffff;
    let root = if matches!(named & 7, 4 | 5) && u64::from(named >> 3) << 6 < len { named } else { 0x10 | 5 };
    #[derive(Default, Debug)]
    struct Found {
        boxes: u32,
        triangles: u32,
        instances: u32,
        procedural: u32,
        empty_boxes: u32,
        outside_structure: u32,
        not_read: u32,
        box_outside_parent: u32,
        triangle_outside_box: u32,
        not_finite: u32,
        zero_nodes: u32,
        repeated: u32,
    }
    let mut f = Found::default();
    let mut todo: Vec<(u32, Option<[f32; 6]>)> = vec![(root, None)];
    let mut seen = HashSet::new();
    let inside = |b: &[f32; 6], p: &[f32; 3]| {
        let size = (b[3] - b[0]).abs().max((b[4] - b[1]).abs()).max((b[5] - b[2]).abs());
        let reach = b.iter().chain(p.iter()).fold(0f32, |m, v| m.max(v.abs()));
        let slack = (1e-3 * size).max(2e-3 * reach).max(1e-4);
        (0..3).all(|c| p[c] >= b[c] - slack && p[c] <= b[c + 3] + slack)
    };
    while let Some((p, parent)) = todo.pop() {
        if !seen.insert(p) {
            f.repeated += 1;
            continue;
        }
        if seen.len() > 1 << 20 {
            break;
        }
        let offset = u64::from(p >> 3) << 6;
        if offset >= len {
            f.outside_structure += 1;
            continue;
        }
        let at = base + offset;
        let mut node = [0u8; 128];
        let size = if p & 7 == 5 { 128 } else { 64 };
        if !memory.read(at, &mut node[..size]) {
            f.not_read += 1;
            continue;
        }
        if node[..size].iter().all(|&b| b == 0) {
            f.zero_nodes += 1;
            continue;
        }
        match p & 7 {
            k @ (4 | 5) => {
                f.boxes += 1;
                let mut any = false;
                for c in 0..4 {
                    let child = dword(&node, 4 * c);
                    if child == bvh::NONE {
                        continue;
                    }
                    any = true;
                    let bx: [f32; 6] = if k == 5 {
                        std::array::from_fn(|i| f32::from_bits(dword(&node, 16 + 24 * c + 4 * i)))
                    } else {
                        std::array::from_fn(|i| {
                            let o = 16 + 12 * c + 2 * i;
                            bvh::half_to_f32(u16::from_le_bytes([node[o], node[o + 1]]))
                        })
                    };
                    if bx.iter().any(|v| !v.is_finite()) {
                        f.not_finite += 1;
                        if n < 8 && f.not_finite == 1 {
                            let words: Vec<String> = (0..size / 4).map(|i| format!("{:08x}", dword(&node, 4 * i))).collect();
                            tracing::info!(n, node = format_args!("{p:#x}"), child = c, bytes = words.join(" "), "bvh check: a box node with a box not finite");
                        }
                    }
                    if let Some(pb) = parent
                        && !(inside(&pb, &[bx[0], bx[1], bx[2]]) && inside(&pb, &[bx[3], bx[4], bx[5]]))
                    {
                        f.box_outside_parent += 1;
                    }
                    todo.push((child, Some(bx)));
                }
                if !any {
                    f.empty_boxes += 1;
                }
            }
            k @ 0..=3 => {
                f.triangles += 1;
                for &v in &bvh::TRIANGLE_VERTICES[k as usize] {
                    let o = 12 * v as usize;
                    let x = [0, 1, 2].map(|c| f32::from_bits(dword(&node, o + 4 * c)));
                    if x.iter().any(|c| !c.is_finite()) {
                        f.not_finite += 1;
                    } else if let Some(pb) = parent
                        && !inside(&pb, &x)
                    {
                        f.triangle_outside_box += 1;
                        break;
                    }
                }
            }
            6 => f.instances += 1,
            _ => f.procedural += 1,
        }
    }
    let bad = f.outside_structure + f.not_read + f.box_outside_parent + f.triangle_outside_box + f.not_finite + f.zero_nodes;
    let (lands, title_header) = crate::memory::packed_lands(base).unwrap_or((0, 0));
    tracing::info!(
        n,
        base = format_args!("{base:#x}"),
        len,
        version = format_args!("{:#x}", d(0)),
        nodes = d(1),
        root = format_args!("{root:#x}"),
        lands,
        title_header = format_args!("{title_header:#018x}"),
        sound = bad == 0 && f.triangles > 0,
        ?f,
        "bvh check: a structure the builder unpacked"
    );
}

/// Every 16-bit float that is not NaN, by value (for rounding outward).
fn halves() -> &'static [(f32, u16)] {
    static ALL: OnceLock<Vec<(f32, u16)>> = OnceLock::new();
    ALL.get_or_init(|| {
        let mut v: Vec<(f32, u16)> = (0..=u16::MAX).map(|h| (bvh::half_to_f32(h), h)).filter(|(f, _)| !f.is_nan()).collect();
        v.sort_by(|a, b| a.0.total_cmp(&b.0));
        v.dedup_by(|a, b| a.0 == b.0);
        v
    })
}

/// The largest 16-bit float not above `x` (`up`: the smallest not below).
fn half_out(x: f32, up: bool) -> u16 {
    let all = halves();
    if up {
        let i = all.partition_point(|(f, _)| *f < x);
        all.get(i).map_or(0x7c00, |e| e.1)
    } else {
        let i = all.partition_point(|(f, _)| *f <= x);
        if i == 0 { 0xfc00 } else { all[i - 1].1 }
    }
}

/// The box nodes of a structure read back whose boxes do not hold what is
/// under them (or hold far more), made again from the triangles: runs of
/// title memory (address, bytes) to write back ([`repairing`]).
fn repair(memory: &Fetched) -> Vec<(u64, Vec<u8>)> {
    let (base, len) = (memory.base, memory.window);
    let mut header = [0u8; 64];
    if !memory.read(base, &mut header) || dword(&header, 0) & 0xFFFF != 6 {
        return Vec::new();
    }
    let named = dword(&header, 48) & 0x3fff_ffff;
    let root = if matches!(named & 7, 4 | 5) && u64::from(named >> 3) << 6 < len { named } else { return Vec::new() };
    // Post-order: a box's children before it.
    let mut order = Vec::new();
    let mut todo = vec![(root, false)];
    let mut seen = HashSet::new();
    while let Some((p, expanded)) = todo.pop() {
        if expanded {
            order.push(p);
            continue;
        }
        if !seen.insert(p) || seen.len() > 1 << 20 || u64::from(p >> 3) << 6 >= len {
            continue;
        }
        todo.push((p, true));
        if matches!(p & 7, 4 | 5) {
            let mut node = [0u8; 16];
            if memory.read(base + (u64::from(p >> 3) << 6), &mut node) {
                for c in 0..4 {
                    let child = dword(&node, 4 * c);
                    if child != bvh::NONE {
                        todo.push((child, false));
                    }
                }
            }
        }
    }
    let mut bounds: HashMap<u32, [f32; 6]> = HashMap::new();
    let mut out = Vec::new();
    let union = |a: Option<[f32; 6]>, b: [f32; 6]| match a {
        None => b,
        Some(a) => [a[0].min(b[0]), a[1].min(b[1]), a[2].min(b[2]), a[3].max(b[3]), a[4].max(b[4]), a[5].max(b[5])],
    };
    let (mut boxes, mut mended) = (0u32, 0u32);
    for p in order {
        let at = base + (u64::from(p >> 3) << 6);
        match p & 7 {
            k @ 0..=3 => {
                let mut node = [0u8; 64];
                if !memory.read(at, &mut node) {
                    continue;
                }
                let mut b: Option<[f32; 6]> = None;
                for &v in &bvh::TRIANGLE_VERTICES[k as usize] {
                    let o = 12 * v as usize;
                    let x = [0, 1, 2].map(|c| f32::from_bits(dword(&node, o + 4 * c)));
                    if x.iter().all(|c| c.is_finite()) {
                        b = Some(union(b, [x[0], x[1], x[2], x[0], x[1], x[2]]));
                    }
                }
                if let Some(b) = b {
                    bounds.insert(p, b);
                }
            }
            k @ (4 | 5) => {
                boxes += 1;
                let size = if k == 5 { 128 } else { 64 };
                let mut node = [0u8; 128];
                if !memory.read(at, &mut node[..size]) {
                    continue;
                }
                let mut whole: Option<[f32; 6]> = None;
                let mut changed = false;
                for c in 0..4 {
                    let child = dword(&node, 4 * c);
                    if child == bvh::NONE {
                        continue;
                    }
                    let Some(&tight) = bounds.get(&child) else { continue };
                    whole = Some(union(whole, tight));
                    // What the node holds now, and whether it is right: it
                    // holds the child, and is not much bigger than it.
                    let held: [f32; 6] = if k == 5 {
                        std::array::from_fn(|i| f32::from_bits(dword(&node, 16 + 24 * c + 4 * i)))
                    } else {
                        std::array::from_fn(|i| {
                            let o = 16 + 12 * c + 2 * i;
                            bvh::half_to_f32(u16::from_le_bytes([node[o], node[o + 1]]))
                        })
                    };
                    let extent = (tight[3] - tight[0]).max(tight[4] - tight[1]).max(tight[5] - tight[2]).max(1e-6);
                    let reach = tight.iter().fold(0f32, |m, v| m.max(v.abs()));
                    let slack = (0.01 * extent).max(2e-3 * reach).max(1e-4);
                    let _ = slack;
                    let right = (0..3).all(|i| !held[i].is_nan() && held[i] <= tight[i]) && (3..6).all(|i| !held[i].is_nan() && held[i] >= tight[i]);
                    if right {
                        continue;
                    }
                    changed = true;
                    if k == 5 {
                        for i in 0..6 {
                            let o = 16 + 24 * c + 4 * i;
                            node[o..o + 4].copy_from_slice(&tight[i].to_bits().to_le_bytes());
                        }
                    } else {
                        for i in 0..6 {
                            let o = 16 + 12 * c + 2 * i;
                            let h = half_out(tight[i], i >= 3);
                            node[o..o + 2].copy_from_slice(&h.to_le_bytes());
                        }
                    }
                }
                if let Some(w) = whole {
                    bounds.insert(p, w);
                }
                if changed {
                    mended += 1;
                    let end = if k == 5 { 16 + 24 * 4 } else { 64 };
                    out.push((at + 16, node[16..end].to_vec()));
                }
            }
            _ => {}
        }
    }
    static TOLD: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
    if mended > 0 && TOLD.fetch_add(1, std::sync::atomic::Ordering::Relaxed) < 60 {
        tracing::info!(base = format_args!("{base:#x}"), boxes, mended, "bvh repair: a structure's boxes made again from its triangles");
    }
    // Runs that do not cross a page (each goes into its page's copy).
    out.into_iter()
        .flat_map(|(at, bytes)| {
            let split = ((at / PAGE + 1) * PAGE).saturating_sub(at) as usize;
            if split >= bytes.len() {
                vec![(at, bytes)]
            } else {
                vec![(at, bytes[..split].to_vec()), (at + split as u64, bytes[split..].to_vec())]
            }
        })
        .collect()
}

/// The reference rule for an instance of the structure at `base` (read
/// back in `memory`): WorldToObject = diag(scale) * inverse(Transform), the
/// scale 2^(e - 127) per axis from header 0x3c when its bit 31 is set
/// (xdxr.dll). One line each for the first structures.
fn instance_check(rt: &Rt, base: u64, memory: &Fetched) {
    static TOLD: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
    let Some((t, w)) = lock(&rt.state).instances.get(&base).copied() else { return };
    let mut header = [0u8; 64];
    if !memory.read(base, &mut header) {
        return;
    }
    let packed = dword(&header, 0x3c);
    let scale: [f32; 3] = if packed >> 31 != 0 {
        [0, 8, 16].map(|sh| f32::from_bits(((packed >> sh) & 0xff) << 23))
    } else {
        [1.0; 3]
    };
    // Inverse of the 3x4 affine [R | t]: [R^-1 | -R^-1 t].
    let m = |r: usize, c: usize| f64::from(t[4 * r + c]);
    let det = m(0, 0) * (m(1, 1) * m(2, 2) - m(1, 2) * m(2, 1)) - m(0, 1) * (m(1, 0) * m(2, 2) - m(1, 2) * m(2, 0))
        + m(0, 2) * (m(1, 0) * m(2, 1) - m(1, 1) * m(2, 0));
    if det.abs() < 1e-12 {
        return;
    }
    let mut inv = [[0f64; 3]; 3];
    for r in 0..3 {
        for c in 0..3 {
            let (r1, r2, c1, c2) = ((c + 1) % 3, (c + 2) % 3, (r + 1) % 3, (r + 2) % 3);
            inv[r][c] = (m(r1, c1) * m(r2, c2) - m(r1, c2) * m(r2, c1)) / det;
        }
    }
    let mut want = [0f64; 12];
    for r in 0..3 {
        let tr: f64 = -(0..3).map(|k| inv[r][k] * m(k, 3)).sum::<f64>();
        for c in 0..3 {
            want[4 * r + c] = inv[r][c] * f64::from(scale[r]);
        }
        want[4 * r + 3] = tr * f64::from(scale[r]);
    }
    let err = (0..12)
        .map(|i| (f64::from(w[i]) - want[i]).abs() / want[i].abs().max(1e-3))
        .fold(0f64, f64::max);
    let n = TOLD.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    if n < 60 {
        let row = |a: &[f64]| format!("{:.4} {:.4} {:.4} {:.4}", a[0], a[1], a[2], a[3]);
        let rowf = |a: &[f32]| format!("{:.4} {:.4} {:.4} {:.4}", a[0], a[1], a[2], a[3]);
        tracing::info!(
            base = format_args!("{base:#x}"),
            compressed = packed >> 31 != 0,
            ?scale,
            max_relative_error = err,
            card_row0 = rowf(&w[0..4]),
            reference_row0 = row(&want[0..4]),
            "instance check: WorldToObject against the console's rule"
        );
    }
}

/// Puts the entry of `root` into the table as the card will have it, and
/// among the writes of the next list. False if both its slots are taken.
fn place(s: &mut State, root: u64, view: u32) -> bool {
    let address = root & !63;
    let (low, high) = (address as u32, (address >> 32) as u32);
    let h = rt_hash(low, high);
    for slot in [h, (h + 1) & (RT_TABLE_SLOTS - 1)] {
        let e = s.table[slot as usize];
        if (e[0] == 0 && e[1] == 0) || (e[0] == low && e[1] == high) {
            let e = [low, high, view, 0];
            s.table[slot as usize] = e;
            s.writes.push((slot, e));
            return true;
        }
    }
    false
}

/// A buffer for an acceleration structure or a build's scratch.
fn card_buffer(device: &ID3D12Device, size: u64, state: D3D12_RESOURCE_STATES) -> Option<ID3D12Resource> {
    let heap = D3D12_HEAP_PROPERTIES { Type: D3D12_HEAP_TYPE_DEFAULT, ..Default::default() };
    let desc = D3D12_RESOURCE_DESC {
        Dimension: D3D12_RESOURCE_DIMENSION_BUFFER,
        Width: size.max(256),
        Height: 1,
        DepthOrArraySize: 1,
        MipLevels: 1,
        SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
        Layout: D3D12_TEXTURE_LAYOUT_ROW_MAJOR,
        Flags: D3D12_RESOURCE_FLAG_ALLOW_UNORDERED_ACCESS,
        ..Default::default()
    };
    let mut buffer: Option<ID3D12Resource> = None;
    // SAFETY: plain creation of a device-local buffer.
    unsafe { device.CreateCommittedResource(&heap, D3D12_HEAP_FLAG_NONE, &desc, state, None, &mut buffer) }.ok()?;
    buffer
}

/// An upload buffer holding `bytes`.
fn uploaded(bytes: &[u8]) -> Option<ID3D12Resource> {
    let host = crate::host::host()?;
    let (buffer, data) = host.upload_buffer(bytes.len() as u64)?;
    // SAFETY: a fresh mapping of at least `bytes.len()` bytes.
    unsafe { std::ptr::copy_nonoverlapping(bytes.as_ptr(), data, bytes.len()) };
    Some(buffer)
}

fn bottom_inputs(vertices: u64, count: u32, geometry: &D3D12_RAYTRACING_GEOMETRY_DESC) -> D3D12_BUILD_RAYTRACING_ACCELERATION_STRUCTURE_INPUTS {
    let _ = (vertices, count);
    D3D12_BUILD_RAYTRACING_ACCELERATION_STRUCTURE_INPUTS {
        Type: D3D12_RAYTRACING_ACCELERATION_STRUCTURE_TYPE_BOTTOM_LEVEL,
        Flags: D3D12_RAYTRACING_ACCELERATION_STRUCTURE_BUILD_FLAG_PREFER_FAST_TRACE,
        NumDescs: 1,
        DescsLayout: D3D12_ELEMENTS_LAYOUT_ARRAY,
        Anonymous: D3D12_BUILD_RAYTRACING_ACCELERATION_STRUCTURE_INPUTS_0 { pGeometryDescs: geometry },
    }
}

fn geometry(vertices: u64, count: u32) -> D3D12_RAYTRACING_GEOMETRY_DESC {
    D3D12_RAYTRACING_GEOMETRY_DESC {
        Type: D3D12_RAYTRACING_GEOMETRY_TYPE_TRIANGLES,
        Flags: D3D12_RAYTRACING_GEOMETRY_FLAG_NONE,
        Anonymous: D3D12_RAYTRACING_GEOMETRY_DESC_0 {
            Triangles: D3D12_RAYTRACING_GEOMETRY_TRIANGLES_DESC {
                Transform3x4: 0,
                IndexFormat: DXGI_FORMAT_UNKNOWN,
                VertexFormat: DXGI_FORMAT_R32G32B32_FLOAT,
                IndexCount: 0,
                VertexCount: count,
                IndexBuffer: 0,
                VertexBuffer: D3D12_GPU_VIRTUAL_ADDRESS_AND_STRIDE { StartAddress: vertices, StrideInBytes: 12 },
            },
        },
    }
}

fn top_inputs(instance: u64) -> D3D12_BUILD_RAYTRACING_ACCELERATION_STRUCTURE_INPUTS {
    D3D12_BUILD_RAYTRACING_ACCELERATION_STRUCTURE_INPUTS {
        Type: D3D12_RAYTRACING_ACCELERATION_STRUCTURE_TYPE_TOP_LEVEL,
        Flags: D3D12_RAYTRACING_ACCELERATION_STRUCTURE_BUILD_FLAG_PREFER_FAST_TRACE,
        NumDescs: 1,
        DescsLayout: D3D12_ELEMENTS_LAYOUT_ARRAY,
        Anonymous: D3D12_BUILD_RAYTRACING_ACCELERATION_STRUCTURE_INPUTS_0 { InstanceDescs: instance },
    }
}

/// The host objects of a bottom-level structure, not yet built.
fn contents_hash(vertices: &[[f32; 3]], pointers: &[u32]) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    for v in vertices {
        v.map(f32::to_bits).hash(&mut h);
    }
    pointers.hash(&mut h);
    h.finish()
}

fn make(root: u64, base: u64, range: (u64, u64), after: u64, vertices: &[[f32; 3]], pointers: &[u32]) -> Option<Made> {
    let host = crate::host::host()?;
    let device5: ID3D12Device5 = host.device.cast().ok()?;
    let count = vertices.len() as u32;
    let vertex_bytes: Vec<u8> = vertices.iter().flatten().flat_map(|f| f.to_le_bytes()).collect();
    let pointer_bytes: Vec<u8> = pointers.iter().flat_map(|p| p.to_le_bytes()).collect();
    let vertices_buffer = uploaded(&vertex_bytes)?;
    let side = uploaded(&pointer_bytes)?;
    // SAFETY: live buffers.
    let vertices_gpu = unsafe { vertices_buffer.GetGPUVirtualAddress() };
    let g = geometry(vertices_gpu, count);
    let inputs = bottom_inputs(vertices_gpu, count, &g);
    let mut info = D3D12_RAYTRACING_ACCELERATION_STRUCTURE_PREBUILD_INFO::default();
    // SAFETY: inputs that point at live data.
    unsafe { device5.GetRaytracingAccelerationStructurePrebuildInfo(&inputs, &mut info) };
    let bottom = card_buffer(&host.device, info.ResultDataMaxSizeInBytes, D3D12_RESOURCE_STATE_RAYTRACING_ACCELERATION_STRUCTURE)?;
    let mut scratch_size = info.ScratchDataSizeInBytes;
    // SAFETY: a live buffer.
    let bottom_gpu = unsafe { bottom.GetGPUVirtualAddress() };
    // The instance: identity transform, id 0, mask 0xff, culling off.
    let mut instance = Vec::with_capacity(64);
    for (k, v) in [1.0f32, 0.0, 0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 0.0, 1.0, 0.0].iter().enumerate() {
        let _ = k;
        instance.extend(v.to_le_bytes());
    }
    instance.extend((0xffu32 << 24).to_le_bytes());
    instance.extend((u32::try_from(D3D12_RAYTRACING_INSTANCE_FLAG_TRIANGLE_CULL_DISABLE.0).unwrap_or(1) << 24).to_le_bytes());
    instance.extend(bottom_gpu.to_le_bytes());
    let instance_buffer = uploaded(&instance)?;
    // SAFETY: a live buffer.
    let top_in = top_inputs(unsafe { instance_buffer.GetGPUVirtualAddress() });
    let mut top_info = D3D12_RAYTRACING_ACCELERATION_STRUCTURE_PREBUILD_INFO::default();
    // SAFETY: as above.
    unsafe { device5.GetRaytracingAccelerationStructurePrebuildInfo(&top_in, &mut top_info) };
    let top = card_buffer(&host.device, top_info.ResultDataMaxSizeInBytes, D3D12_RESOURCE_STATE_RAYTRACING_ACCELERATION_STRUCTURE)?;
    scratch_size = scratch_size.max(top_info.ScratchDataSizeInBytes);
    let scratch = card_buffer(&host.device, scratch_size, D3D12_RESOURCE_STATE_UNORDERED_ACCESS)?;
    crate::host::name(&bottom, || format!("ray queries: {root:#x} bottom"));
    Some(Made { root, base, hash: contents_hash(vertices, pointers), range, walked_after: after, bottom, top, scratch, vertices: vertices_buffer, side, instance: instance_buffer, triangles: pointers.len() as u32 })
}

/// The list to run first in submission `number` (`behind`: the host fence's
/// value behind it; `done`: its value now): it builds the structures made
/// since the last and writes their table entries. Also reads the notes and
/// looks for structures written again. `None`: nothing to do.
pub(crate) fn list(device: &ID3D12Device, number: u64, behind: u64, done: u64) -> Option<ID3D12CommandList> {
    walk_dump();
    if FIRST.get().is_none() && !checking() && !repairing() {
        return None;
    }
    let rt = rt()?;
    let mut s = lock(&rt.state);
    // Held no more once the card is past them.
    s.retired.retain(|(after, _)| *after > done);
    s.scratch.retain(|(after, _)| *after > done);
    notes(rt, &mut s, number);
    if (checking() || repairing()) && number.is_multiple_of(64) {
        let mut taken = 0;
        let again: Vec<u64> = s.check_again.iter().filter(|(_, at)| number >= **at + 320).map(|(a, _)| *a).collect();
        for address in again {
            s.check_again.remove(&address);
            if let Some((_, len)) = crate::memory::packed_structures().into_iter().find(|(a, _)| *a == address) {
                s.to_fetch.push((CHECK, address, number, len.next_multiple_of(PAGE)));
            }
        }
        for (address, len) in crate::memory::packed_structures() {
            if taken < 4 && s.checked.insert(address) {
                s.to_fetch.push((CHECK, address, number, len.next_multiple_of(PAGE)));
                s.check_again.insert(address, number);
                taken += 1;
            }
        }
    }
    if number >= s.looked + LOOK_EVERY {
        s.looked = number;
        look(rt, &mut s, number);
    }
    if number >= s.told + 2000 {
        s.told = number;
        let kept = s.kept.iter().flatten().count();
        let triangles: u64 = s.kept.iter().flatten().map(|k| u64::from(k.triangles)).sum();
        let walking = s.known.values().filter(|k| matches!(k, Known::Walking)).count();
        let not = s.known.values().filter(|k| matches!(k, Known::Not)).count();
        tracing::info!(kept, triangles, walking, not, "ray queries: host structures");
    }
    // Read back: to the walker.
    let (ready, waiting): (Vec<Fetching>, Vec<Fetching>) = std::mem::take(&mut s.fetching).into_iter().partition(|f| f.done_at <= done);
    s.fetching = waiting;
    {
        let jobs = lock(&rt.jobs);
        for f in ready {
            let len = (f.present.len() as u64 * PAGE) as usize;
            let mut data = vec![0u8; len];
            let mut p = std::ptr::null_mut();
            // SAFETY: a read back the card is done with, mapped for reading.
            let mapped = unsafe { f.buffer.Map(0, Some(&D3D12_RANGE { Begin: 0, End: len }), Some(&mut p)) };
            if mapped.is_ok() && !p.is_null() {
                // SAFETY: `len` bytes mapped.
                unsafe {
                    std::ptr::copy_nonoverlapping(p as *const u8, data.as_mut_ptr(), len);
                    f.buffer.Unmap(0, None);
                }
                let _ = jobs.send(Fetched { root: f.root, base: f.base, after: f.after, window: f.window, first: f.first, present: f.present, data });
            } else {
                s.known.insert(f.root, Known::Not);
            }
        }
    }
    let n = s.to_fetch.len().min(FETCHES);
    let fetch: Vec<(u64, u64, u64, u64)> = s.to_fetch.drain(..n).collect();
    let made = std::mem::take(&mut s.made);
    let repairs = std::mem::take(&mut s.repairs);
    if made.is_empty() && s.writes.is_empty() && s.zeroed && fetch.is_empty() && repairs.is_empty() {
        return None;
    }
    let i = match s.lists.iter().position(|l| l.2 <= done) {
        Some(i) => i,
        None => {
            // SAFETY: plain object creation.
            let made = unsafe {
                (|| -> windows::core::Result<(ID3D12CommandAllocator, ID3D12GraphicsCommandList4)> {
                    let allocator: ID3D12CommandAllocator = device.CreateCommandAllocator(D3D12_COMMAND_LIST_TYPE_DIRECT)?;
                    let list: ID3D12GraphicsCommandList4 = device.CreateCommandList(0, D3D12_COMMAND_LIST_TYPE_DIRECT, &allocator, None)?;
                    list.Close()?;
                    Ok((allocator, list))
                })()
            };
            let (allocator, list) = made.ok()?;
            s.lists.push((allocator, list, 0));
            s.lists.len() - 1
        }
    };
    s.lists[i].2 = behind;
    let (allocator, list) = (s.lists[i].0.clone(), s.lists[i].1.clone());
    // SAFETY: the list's last run is done; what it reads is held until the
    // card is past this submission.
    unsafe {
        allocator.Reset().ok()?;
        list.Reset(&allocator, None).ok()?;
        let mut copied = false;
        if !s.zeroed {
            list.CopyBufferRegion(&rt.table, 0, &rt.zeros, 0, u64::from(RT_TABLE_SLOTS) * 16);
            s.zeroed = true;
            copied = true;
        }
        // Mended box nodes, into the card's copies of their pages.
        if !repairs.is_empty() {
            let host = crate::host::host()?;
            let total: usize = repairs.iter().map(|r| r.1.len()).sum();
            if let Some((upload, data)) = host.upload_buffer(total as u64) {
                let mut at = 0usize;
                let mut wrote = 0;
                for (address, bytes) in &repairs {
                    std::ptr::copy_nonoverlapping(bytes.as_ptr(), data.add(at), bytes.len());
                    let page = address / PAGE;
                    if let Some((_, buffer, byte)) = crate::memory::card_pages(page, 1).into_iter().next() {
                        list.CopyBufferRegion(&buffer, byte + (address - page * PAGE), &upload, at as u64, bytes.len() as u64);
                        wrote += 1;
                    }
                    at += bytes.len();
                }
                s.scratch.push((behind, upload));
                static TOLD: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
                if TOLD.fetch_add(1, std::sync::atomic::Ordering::Relaxed) < 40 {
                    tracing::info!(runs = repairs.len(), wrote, bytes = total, "bvh repair: box nodes written back");
                }
            }
        }
        // Structures' title memory, back from the card's copies.
        for (root, base, after, window) in fetch {
            let first = base / PAGE;
            let count = (base + window).div_ceil(PAGE) - first;
            let pages = crate::memory::card_pages(first, count);
            let host = crate::host::host()?;
            let buffer = if pages.is_empty() { None } else { host.read_back_buffer(count * PAGE) };
            let Some(buffer) = buffer else {
                s.known.insert(root, Known::Not);
                continue;
            };
            let mut present = vec![false; count as usize];
            let mut sources = Vec::with_capacity(pages.len());
            for (page, source, byte) in pages {
                list.CopyBufferRegion(&buffer, (page - first) * PAGE, &source, byte, PAGE);
                present[(page - first) as usize] = true;
                sources.push(source);
            }
            s.fetching.push(Fetching { root, base, after, window, first, present, buffer, _sources: sources, done_at: behind });
        }
        let mut built = Vec::new();
        for m in made {
            let Some(view) = free_view(&s) else {
                tracing::warn!(most = MOST, "ray queries: no view slots left; a structure left to shader code");
                s.known.insert(m.root, Known::Not);
                continue;
            };
            let g = geometry(m.vertices.GetGPUVirtualAddress(), m.triangles * 3);
            let bottom = D3D12_BUILD_RAYTRACING_ACCELERATION_STRUCTURE_DESC {
                DestAccelerationStructureData: m.bottom.GetGPUVirtualAddress(),
                Inputs: bottom_inputs(0, 0, &g),
                SourceAccelerationStructureData: 0,
                ScratchAccelerationStructureData: m.scratch.GetGPUVirtualAddress(),
            };
            list.BuildRaytracingAccelerationStructure(&bottom, None);
            built.push((view, m));
        }
        if !built.is_empty() {
            // The bottom-level structures, before the top-level builds read
            // them and their scratch is used again.
            list.ResourceBarrier(&[uav_barrier()]);
            for (_, m) in &built {
                let top = D3D12_BUILD_RAYTRACING_ACCELERATION_STRUCTURE_DESC {
                    DestAccelerationStructureData: m.top.GetGPUVirtualAddress(),
                    Inputs: top_inputs(m.instance.GetGPUVirtualAddress()),
                    SourceAccelerationStructureData: 0,
                    ScratchAccelerationStructureData: m.scratch.GetGPUVirtualAddress(),
                };
                list.BuildRaytracingAccelerationStructure(&top, None);
            }
            list.ResourceBarrier(&[uav_barrier()]);
        }
        let heaps = s.heaps.clone();
        for (view, m) in built {
            let top_gpu = m.top.GetGPUVirtualAddress();
            for &(cpu, step) in &heaps {
                write_views(cpu, step, view, top_gpu, &m.side, m.triangles);
            }
            let kept = Kept {
                root: m.root,
                base: m.base,
                view,
                hash: m.hash,
                range: m.range,
                walked_after: m.walked_after,
                top_gpu,
                side: m.side.clone(),
                triangles: m.triangles,
                _held: vec![m.bottom, m.top.clone(), m.vertices, m.side, m.instance],
            };
            s.scratch.push((behind, m.scratch));
            let index = match s.known.get(&m.root) {
                Some(Known::Kept(i)) => {
                    let i = *i;
                    if let Some(old) = s.kept[i].take() {
                        s.retired.push((behind, old));
                    }
                    i
                }
                _ => {
                    s.kept.push(None);
                    s.kept.len() - 1
                }
            };
            s.kept[index] = Some(kept);
            s.known.insert(m.root, Known::Kept(index));
            if !place(&mut s, m.root, view) {
                tracing::info!(root = format_args!("{:#x}", m.root), "ray queries: no table slot; a structure left to shader code");
            }
        }
        let writes = std::mem::take(&mut s.writes);
        for (slot, e) in &writes {
            let at: Vec<D3D12_WRITEBUFFERIMMEDIATE_PARAMETER> = (0..4)
                .map(|k| D3D12_WRITEBUFFERIMMEDIATE_PARAMETER { Dest: rt.table_gpu + u64::from(*slot) * 16 + 4 * k as u64, Value: e[k] })
                .collect();
            list.WriteBufferImmediate(4, at.as_ptr(), None);
        }
        // The shaders that follow read the table.
        if copied || !writes.is_empty() {
        list.ResourceBarrier(&[D3D12_RESOURCE_BARRIER {
            Type: D3D12_RESOURCE_BARRIER_TYPE_TRANSITION,
            Flags: D3D12_RESOURCE_BARRIER_FLAG_NONE,
            Anonymous: D3D12_RESOURCE_BARRIER_0 {
                Transition: std::mem::ManuallyDrop::new(D3D12_RESOURCE_TRANSITION_BARRIER {
                    pResource: std::mem::ManuallyDrop::new(Some(rt.table.clone())),
                    Subresource: D3D12_RESOURCE_BARRIER_ALL_SUBRESOURCES,
                    StateBefore: D3D12_RESOURCE_STATE_COPY_DEST,
                    StateAfter: D3D12_RESOURCE_STATE_UNORDERED_ACCESS,
                }),
            },
        }]);
        }
        list.Close().ok()?;
        list.cast().ok()
    }
}

fn uav_barrier() -> D3D12_RESOURCE_BARRIER {
    D3D12_RESOURCE_BARRIER {
        Type: D3D12_RESOURCE_BARRIER_TYPE_UAV,
        Flags: D3D12_RESOURCE_BARRIER_FLAG_NONE,
        Anonymous: D3D12_RESOURCE_BARRIER_0 {
            UAV: std::mem::ManuallyDrop::new(D3D12_RESOURCE_UAV_BARRIER { pResource: std::mem::ManuallyDrop::new(None) }),
        },
    }
}

/// A view slot no kept structure uses (two slots from it).
fn free_view(s: &State) -> Option<u32> {
    let first = *FIRST.get()?;
    let used: HashSet<u32> = s.kept.iter().flatten().map(|k| k.view).chain(s.retired.iter().map(|r| r.1.view)).collect();
    (0..MOST).map(|i| first + 2 * i).find(|v| !used.contains(v))
}

/// Takes the notes shaders left, and walks the structures they name that
/// are not known yet.
fn notes(rt: &Rt, s: &mut State, number: u64) {
    use std::sync::atomic::{AtomicU32, Ordering};
    let mut new = 0;
    for i in 0..RT_NOTES as usize {
        // SAFETY: the mapping, read as atomics (the card may be writing).
        let (lo, hi, blo, bhi) = unsafe {
            let p = (rt.notes as *const AtomicU32).add(4 * i);
            (&*p, &*p.add(1), &*p.add(2), &*p.add(3))
        };
        let (low, high) = (lo.swap(0, Ordering::AcqRel), hi.swap(0, Ordering::AcqRel));
        let base = u64::from(bhi.swap(0, Ordering::AcqRel)) << 32 | u64::from(blo.swap(0, Ordering::AcqRel));
        if low == 0 && high == 0 {
            continue;
        }
        let root = u64::from(high) << 32 | u64::from(low);
        if base == 0 || (root & !63) < base || s.known.contains_key(&root) {
            continue;
        }
        s.known.insert(root, Known::Walking);
        s.to_fetch.push((root, base, number, FIRST_WINDOW));
        new += 1;
    }
    if new > 0 {
        tracing::debug!(new, "ray queries: structures noted");
    }
}

/// Walks again the kept structures a submission since their walk wrote.
fn look(_rt: &Rt, s: &mut State, number: u64) {
    let mut again = Vec::new();
    for k in s.kept.iter_mut().flatten() {
        let (low, high) = k.range;
        if !crate::marks::untouched_since(low as usize, (high - low) as usize, k.walked_after.max(1)) {
            // (Walked after this submission from now on: one write, one walk.)
            k.walked_after = number;
            again.push((k.root, k.base, number, FIRST_WINDOW));
        }
    }
    s.to_fetch.extend(again);
}
