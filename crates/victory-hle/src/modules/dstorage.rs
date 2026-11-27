//! The console's DirectStorage (`XCRDAPI.dll`): a factory that opens files
//! and creates request queues and status arrays the title polls. Its
//! interfaces have grown since 2020 (new interface ids, new slots); the
//! slots here are the ones the title calls, read from its call
//! sites. Slots nobody has needed yet log their offset.
//!
//! A request starts once it is submitted, not before: the title maps the
//! pages a batch is read into, and fills in their page lists, in its own
//! submit (`submit`).
//!
//! Submitted requests of every queue run on one pool of workers, as the
//! console's hardware runs them: in parallel, started in queue order,
//! queues served by weighted round robin over their priorities (about 10x
//! per level, counted in bytes), with at most the staging buffer's worth of
//! compressed source in flight. Each request reads its source (a file or
//! memory), inflates zlib data and/or unpacks BCPack data, and writes the
//! destination (an address, or physical pages). Completion is what stays
//! in order: a status entry is written once every request enqueued before
//! it on its queue has finished, with the first failure (in queue order)
//! since the previous status entry, or success.
#![allow(non_snake_case)]
#![allow(clippy::not_unsafe_ptr_arg_deref)]

use std::borrow::Cow;
use std::collections::{BTreeMap, BTreeSet, HashMap, VecDeque};
use std::ffi::{CStr, c_char, c_void};
use std::os::windows::fs::FileExt;
use std::os::windows::io::FromRawHandle;
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicU32, AtomicU64, Ordering};
use std::sync::{Condvar, Mutex, MutexGuard, OnceLock};

use victory_formats::bcpack::{self, Tables};

use super::xasync::{E_INVALIDARG, E_NOTIMPL, E_PENDING};
use super::xgameruntime::{Guid, HRESULT, Methods, Object, S_OK, add_ref, missing, put};
use crate::{HleModule, hle_export};

/// The factory interface the title asks for.
pub(crate) const FACTORY: Guid = Guid::new(
    0x6295_9d11,
    0xd45f,
    0x4884,
    [0xa0, 0xa3, 0xf7, 0x86, 0x2e, 0xa2, 0x43, 0xae],
);
pub(crate) const IUNKNOWN: Guid = Guid::new(0, 0, 0, [0xc0, 0, 0, 0, 0, 0, 0, 0x46]);
// The interfaces each object answers to, as `XCRDAPI.dll`'s QueryInterface
// methods list them (IUnknown included).
pub(crate) const FACTORY_IIDS: [Guid; 4] = [
    Guid::new(0xfb3b_54ac, 0xbf82, 0x463a, [0xb7, 0x77, 0x73, 0x19, 0x58, 0x37, 0x86, 0x4d]),
    Guid::new(0xc74c_2a5b, 0x1e43, 0x46a5, [0xa2, 0x24, 0x91, 0x16, 0x35, 0x8e, 0x11, 0x18]),
    FACTORY,
    IUNKNOWN,
];
pub(crate) const FILE_IIDS: [Guid; 2] = [
    Guid::new(0x4370_4f9f, 0x27c6, 0x4a9f, [0x88, 0x15, 0xd1, 0x65, 0x27, 0x89, 0x4d, 0xf4]),
    IUNKNOWN,
];
pub(crate) const QUEUE_IIDS: [Guid; 4] = [
    Guid::new(0xa3f6_649a, 0x8c2c, 0x4b3a, [0x8b, 0x6e, 0x10, 0x18, 0x6a, 0x04, 0xb9, 0xbc]),
    // The title's.
    Guid::new(0x3c02_cb0c, 0xc050, 0x4195, [0x99, 0xa1, 0xa1, 0xce, 0x94, 0xba, 0x33, 0xc5]),
    Guid::new(0x3af5_c948, 0x6297, 0x426c, [0xbf, 0xfc, 0x8f, 0x18, 0x95, 0x9f, 0xb8, 0x7b]),
    IUNKNOWN,
];
pub(crate) const STATUS_IIDS: [Guid; 2] = [
    Guid::new(0xcd66_d752, 0x2786, 0x44f7, [0x90, 0x70, 0xe2, 0xa6, 0xfd, 0xad, 0x3f, 0x1b]),
    IUNKNOWN,
];
/// `ID3D12Fence1`, which `EnqueueSignal` asks its fence for.
const FENCE1: Guid = Guid::new(0x4336_85fe, 0xe22b, 0x4ca0, [0xa8, 0xdb, 0xb5, 0xb4, 0xf4, 0xdd, 0x0e, 0x4a]);

fn is_one_of(iid: *const Guid, iids: &[Guid]) -> bool {
    // SAFETY: the title passes a GUID (null tolerated).
    unsafe { iid.as_ref() }.is_some_and(|iid| iids.contains(iid))
}

/// `QueryInterface` of one of our objects with a reference count.
fn query_instance<T>(this: *mut Instance<T>, iid: *const Guid, out: Out, iids: &[Guid]) -> HRESULT {
    if !out.is_null() {
        // SAFETY: checked non-null.
        unsafe { out.write(std::ptr::null_mut()) };
    }
    if !is_one_of(iid, iids) {
        tracing::debug!(iid = iid_text(iid), "dstorage: QueryInterface: unknown interface");
        return E_NOINTERFACE;
    }
    add_ref_instance(this);
    put(out, this.cast())
}

extern "system" fn query_file(this: *mut Instance<File>, iid: *const Guid, out: Out) -> HRESULT {
    query_instance(this, iid, out, &FILE_IIDS)
}
extern "system" fn query_queue(this: *mut Instance<Queue>, iid: *const Guid, out: Out) -> HRESULT {
    query_instance(this, iid, out, &QUEUE_IIDS)
}
extern "system" fn query_status(this: *mut Instance<StatusArray>, iid: *const Guid, out: Out) -> HRESULT {
    query_instance(this, iid, out, &STATUS_IIDS)
}
const E_NOINTERFACE: HRESULT = 0x8000_4002_u32 as i32;
const E_OUTOFMEMORY: HRESULT = 0x8007_000E_u32 as i32;
/// `HRESULT_FROM_WIN32(ERROR_HANDLE_EOF)`: the source ends early.
const END_OF_FILE: HRESULT = 0x8007_0026_u32 as i32;
/// `HRESULT_FROM_WIN32(ERROR_INVALID_DATA)`: a bad compressed stream.
const INVALID_DATA: HRESULT = 0x8007_000D_u32 as i32;
// The console's own DirectStorage errors (facility 0x924), as `XCRDAPI.dll`
// returns and logs them.
/// A queue used after `Close` (every queue method checks first).
const QUEUE_CLOSED: HRESULT = 0x8924_0010_u32 as i32;
/// A queue priority outside low (-1) to real-time (2).
const INVALID_QUEUE_PRIORITY: HRESULT = 0x8924_0013_u32 as i32;
/// A status entry past the array's capacity.
const INDEX_OUT_OF_RANGE: HRESULT = 0x8924_0015_u32 as i32;

type Out = *mut *mut c_void;

static FACTORY_OBJECT: Object = Object::new("dstorage factory", &FACTORY_VTABLE);

/// As the console's: the one factory's QueryInterface.
pub extern "system" fn DStorageGetFactory(iid: *const Guid, out: Out) -> HRESULT {
    let hr = query_factory(&FACTORY_OBJECT, iid, out);
    if hr != S_OK {
        tracing::warn!(iid = iid_text(iid), "DStorageGetFactory: unknown interface");
    }
    hr
}

extern "system" fn query_factory(this: Factory, iid: *const Guid, out: Out) -> HRESULT {
    if !out.is_null() {
        // SAFETY: checked non-null.
        unsafe { out.write(std::ptr::null_mut()) };
    }
    if !is_one_of(iid, &FACTORY_IIDS) {
        return E_NOINTERFACE;
    }
    put(out, this.cast_mut().cast())
}

/// One of our objects with state: the runtime's object header (method
/// table first), a reference count, then `T`.
#[repr(C)]
struct Instance<T> {
    base: Object,
    refs: AtomicU32,
    state: T,
}

impl<T> Instance<T> {
    fn create(base: Object, state: T) -> *mut c_void {
        Box::into_raw(Box::new(Instance {
            base,
            refs: AtomicU32::new(1),
            state,
        }))
        .cast()
    }
}

extern "system" fn add_ref_instance<T>(this: *const Instance<T>) -> u32 {
    // SAFETY: an instance we created and the title still holds.
    unsafe { &*this }.refs.fetch_add(1, Ordering::Relaxed) + 1
}

extern "system" fn release<T>(this: *mut Instance<T>) -> u32 {
    // SAFETY: as above.
    let left = unsafe { &*this }.refs.fetch_sub(1, Ordering::AcqRel) - 1;
    if left == 0 {
        // SAFETY: the last reference; created by `Instance::create`.
        drop(unsafe { Box::from_raw(this) });
    }
    left
}

fn iid_text(iid: *const Guid) -> String {
    // SAFETY: the title passes a GUID (null tolerated).
    unsafe { iid.as_ref() }.map_or_else(|| "null".into(), Guid::to_string)
}

/// `HRESULT_FROM_WIN32` of a host error.
fn from_win32(error: &std::io::Error) -> HRESULT {
    error
        .raw_os_error()
        .map_or(E_INVALIDARG, |e| (0x8007_0000 | (e as u32 & 0xffff)) as i32)
}

// The factory.

type Factory = *const Object;

/// Opens `path` for reading; the file object owns the handle.
extern "system" fn open_file(_: Factory, path: *const u16, iid: *const Guid, out: Out) -> HRESULT {
    const GENERIC_READ: u32 = 0x8000_0000;
    const FILE_SHARE_READ: u32 = 1;
    const OPEN_EXISTING: u32 = 3;
    if path.is_null() {
        return E_INVALIDARG;
    }
    if !iid.is_null() && !is_one_of(iid, &FILE_IIDS) {
        return E_NOINTERFACE;
    }
    // Goes through `files`, so paths on our virtual drives work too.
    let handle = super::files::CreateFileW(
        path,
        GENERIC_READ,
        FILE_SHARE_READ,
        std::ptr::null(),
        OPEN_EXISTING,
        0,
        std::ptr::null_mut(),
    );
    // SAFETY: the title passes a NUL-terminated path.
    let name = unsafe {
        let len = (0..).take_while(|&i| *path.add(i) != 0).count();
        String::from_utf16_lossy(std::slice::from_raw_parts(path, len))
    };
    if handle as isize == -1 {
        let error = std::io::Error::last_os_error();
        tracing::warn!(path = name, %error, "dstorage: opening a file failed");
        return from_win32(&error);
    }
    let kept = name.clone();
    tracing::info!(path = name, iid = iid_text(iid), "dstorage: file opened");
    // SAFETY: a handle we just opened and nobody else owns.
    let file = unsafe { std::fs::File::from_raw_handle(handle) };
    let files = Files::new(file, Io::chosen());
    let file = Instance::create(
        Object::new("dstorage file", &FILE_VTABLE),
        File {
            files,
            path: kept.clone(),
            closed: AtomicBool::new(false),
        },
    );
    if stream_log::on() {
        stream_log::line(format_args!("F\t{:x}\t{kept}", file as usize));
    }
    WALLS_FILES.lock().unwrap_or_else(|e| e.into_inner()).push((file as usize, kept));
    put(out, file)
}

/// `(capacity, name, iid, out)`.
extern "system" fn create_status_array(
    _: Factory,
    capacity: u32,
    name: *const c_char,
    iid: *const Guid,
    out: Out,
) -> HRESULT {
    tracing::info!(
        capacity,
        name = text(name),
        iid = iid_text(iid),
        "dstorage: status array"
    );
    if !iid.is_null() && !is_one_of(iid, &STATUS_IIDS) {
        return E_NOINTERFACE;
    }
    // Every entry starts pending, as the console's do: one nothing was
    // enqueued for is not complete.
    let entries = (0..capacity).map(|_| AtomicI32::new(E_PENDING)).collect();
    let written = (0..capacity).map(|_| AtomicU64::new(0)).collect();
    let array = Instance::create(
        Object::new("dstorage status array", &STATUS_VTABLE),
        StatusArray { entries, written },
    );
    put(out, array)
}

/// The queue's description for `CreateQueue1` (`DSTORAGE_QUEUE_DESC1`): as
/// in 2020 (source type, capacity, priority, name), plus options. The
/// 2020 `CreateQueue` (slot 0x18) takes the same without the options.
#[repr(C)]
struct QueueDesc {
    source_type: u64,
    capacity: u16,
    priority: i8,
    name: *const c_char,
    /// Bit 0: the queue does not submit itself once half of its capacity
    /// has been enqueued since the last submit ([`MANUAL_SUBMIT`]); the
    /// other bits are reserved and must be 0. The title sets 1.
    options: u64,
}

/// `QueueDesc::options` bit 0.
const MANUAL_SUBMIT: u64 = 1;

/// `CreateQueue1` (slot 0x50), the one the title calls.
extern "system" fn create_queue(
    _: Factory,
    desc: *const QueueDesc,
    iid: *const Guid,
    out: Out,
) -> HRESULT {
    // SAFETY: the title passes its description (null tolerated).
    let Some(desc) = (unsafe { desc.as_ref() }) else {
        return E_INVALIDARG;
    };
    if desc.options > MANUAL_SUBMIT {
        tracing::warn!(options = desc.options, "dstorage: queue options' reserved bits are not 0");
        return E_INVALIDARG;
    }
    new_queue(desc, desc.options & MANUAL_SUBMIT != 0, iid, out)
}

/// The 2020 `CreateQueue` (slot 0x18): a queue that submits itself at half
/// capacity.
extern "system" fn create_queue_2020(
    _: Factory,
    desc: *const QueueDesc2020,
    iid: *const Guid,
    out: Out,
) -> HRESULT {
    // SAFETY: the title passes its description (null tolerated).
    let Some(desc) = (unsafe { desc.as_ref() }) else {
        return E_INVALIDARG;
    };
    let desc = QueueDesc {
        source_type: desc.source_type,
        capacity: desc.capacity,
        priority: desc.priority,
        name: desc.name,
        options: 0,
    };
    new_queue(&desc, false, iid, out)
}

/// `DSTORAGE_QUEUE_DESC`, 0x18 bytes.
#[repr(C)]
struct QueueDesc2020 {
    source_type: u64,
    capacity: u16,
    priority: i8,
    name: *const c_char,
}

fn new_queue(desc: &QueueDesc, manual: bool, iid: *const Guid, out: Out) -> HRESULT {
    let name = text(desc.name);
    tracing::info!(
        source = desc.source_type,
        capacity = desc.capacity,
        priority = desc.priority,
        name,
        manual,
        iid = iid_text(iid),
        "dstorage: queue"
    );
    if !(-1..=2).contains(&desc.priority) {
        return INVALID_QUEUE_PRIORITY;
    }
    if !iid.is_null() && !is_one_of(iid, &QUEUE_IIDS) {
        return E_NOINTERFACE;
    }
    let Some(scheduler) = scheduler() else {
        return E_OUTOFMEMORY;
    };
    let mut lane = Lane::new(desc.priority, desc.capacity, manual);
    // As the console keeps them for `Query`: the description's source type
    // (32 bits), and its name pointer as the title passed it.
    lane.source_type = desc.source_type as u32;
    lane.name = desc.name;
    // The error event: auto-reset, set at the queue's first failure.
    // SAFETY: a plain unnamed event.
    lane.errors.event = unsafe {
        windows_sys::Win32::System::Threading::CreateEventW(std::ptr::null(), 0, 0, std::ptr::null())
    } as usize;
    let mut lanes = scheduler.lock();
    let id = lanes.next_id;
    lanes.next_id += 1;
    lanes.lanes.insert(id, lane);
    drop(lanes);
    put(
        out,
        Instance::create(Object::new("dstorage queue", &QUEUE_VTABLE), Queue { id }),
    )
}

/// The title asks for 32 MiB and stops if this fails. Give PC worker threads
/// enough staging headroom (default 256 MiB) so multi-MB RPF chunks don't
/// starve the remaining cores.
extern "system" fn set_staging_buffer_size(_: Factory, size: u32) -> HRESULT {
    static FLOOR_MB: std::sync::OnceLock<u64> = std::sync::OnceLock::new();
    let floor = *FLOOR_MB.get_or_init(|| {
        std::env::var("VICTORY_DSTORAGE_STAGING_MB")
            .ok()
            .and_then(|v| v.trim().parse::<u64>().ok())
            .unwrap_or(256)
    }) << 20;
    tracing::info!(size, floor, "dstorage: staging buffer size");
    if let Some(scheduler) = scheduler() {
        scheduler.lock().staging = u64::from(size).max(floor);
    }
    S_OK
}

/// The CPU cores the console's storage host may use for this title; ours
/// are the host's threads, so it is noted and accepted.
extern "system" fn set_cpu_affinity(_: Factory, mask: u64) -> HRESULT {
    tracing::info!(mask = format_args!("{mask:#x}"), "dstorage: CPU affinity (host threads keep theirs)");
    S_OK
}

/// The debug flags (`SetDebugFlags` +0x38, read back by +0x60): on the
/// console they live in the first control page (0x180010c60/0x1800106e0),
/// where the host reads them; bit 0 prints the library's errors, bit 1
/// breaks into the debugger on them. Kept and handed back (in the page
/// too, when one is mapped); our checks always run.
static DEBUG_FLAGS: AtomicU32 = AtomicU32::new(0);
extern "system" fn set_debug_flags(_: Factory, flags: u32) {
    tracing::info!(flags = format_args!("{flags:#x}"), "dstorage: debug flags");
    DEBUG_FLAGS.store(flags, Ordering::Relaxed);
    if let Some([flags_page, _]) = super::xcrd::control::pages() {
        // SAFETY: the mapped control page, its first word the flags.
        unsafe { (flags_page as *mut u32).write_volatile(flags) };
    }
}
extern "system" fn debug_flags(_: Factory) -> u32 {
    match super::xcrd::control::pages() {
        // SAFETY: as above.
        Some([flags_page, _]) => unsafe { (flags_page as *const u32).read_volatile() },
        None => DEBUG_FLAGS.load(Ordering::Relaxed),
    }
}

/// Where slot +0x48 (0x1800106f0) takes each statistic from in the second
/// control page: `(to, from, bytes)`. Four 0x30-byte blocks (per priority,
/// last first) of seven counts and two totals, then single counts; the
/// bytes between are left as the title had them.
const STATISTICS: [(usize, usize, usize); 59] = {
    let mut map = [(0, 0, 0); 59];
    let single = [(0xc0, 0x606, 1), (0xc1, 0x607, 1), (0xc4, 0x608, 4), (0xc8, 0x60c, 4), (0xcc, 0x610, 4), (0xd0, 0x614, 4), (0xd4, 0x618, 4), (0xd8, 0x628, 4), (0xdc, 0x62c, 4), (0xe0, 0x630, 4), (0xe4, 0x63c, 4)];
    let mut n = 0;
    while n < single.len() {
        map[n] = single[n];
        n += 1;
    }
    let blocks = [0x90, 0x60, 0x30, 0];
    let mut b = 0;
    while b < 4 {
        let (to, from) = (blocks[b], 0x200 + 0x80 * b);
        let fields = [(0, 0, 4), (4, 4, 4), (8, 8, 4), (0xc, 0xc, 4), (0x10, 0x10, 4), (0x14, 0x14, 4), (0x18, 0x18, 4), (0x20, 0x28, 8), (0x28, 0x30, 8)];
        let mut f = 0;
        while f < fields.len() {
            map[n] = (to + fields[f].0, from + fields[f].1, fields[f].2);
            n += 1;
            f += 1;
        }
        b += 1;
    }
    let tail = [(0xe8, 0x400), (0xec, 0x404), (0xf0, 0x408), (0xf4, 0x40c), (0xf8, 0x410), (0xfc, 0x414)];
    let mut t = 0;
    while t < tail.len() {
        map[n] = (tail[t].0, tail[t].1, 4);
        n += 1;
        t += 1;
    }
    map
};

/// Slot +0x48: the storage host's counters, from the second control page.
/// With no page mapped (the layer's case) they are all 0.
extern "system" fn statistics(_: Factory, out: *mut u8) {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| tracing::info!("dstorage: statistics asked for (all 0 here)"));
    if out.is_null() {
        return;
    }
    let page = super::xcrd::control::pages().map(|[_, counters]| counters as *const u8);
    for (to, from, bytes) in STATISTICS {
        for i in 0..bytes {
            // SAFETY: the mapped page's counters, or 0; the title's 0x100-byte block.
            unsafe { *out.add(to + i) = page.map_or(0, |p| p.add(from + i).read_volatile()) };
        }
    }
}

/// Slot +0x58, the factory's own start (DStorageGetFactory calls it on the
/// console); ours is ready.
extern "system" fn start(_: Factory) -> HRESULT {
    S_OK
}

fn text(name: *const c_char) -> String {
    if name.is_null() {
        return String::new();
    }
    // SAFETY: a NUL-terminated name from the title.
    unsafe { CStr::from_ptr(name) }
        .to_string_lossy()
        .into_owned()
}

/// `XCRDAPI.dll`'s 13 slots (the factory is one static object; AddRef and
/// Release count nothing).
static FACTORY_VTABLE: Methods<13> = Methods([
    query_factory as *const (),
    add_ref as *const (),
    add_ref as *const (),
    create_queue_2020 as *const (),       // 0x18
    open_file as *const (),               // 0x20
    create_status_array as *const (),     // 0x28
    set_cpu_affinity as *const (),        // 0x30
    set_debug_flags as *const (),         // 0x38
    set_staging_buffer_size as *const (), // 0x40
    statistics as *const (),              // 0x48
    create_queue as *const (),            // 0x50
    start as *const (),                   // 0x58
    debug_flags as *const (),             // 0x60
]);

// Files.

struct File {
    files: Files,
    /// As opened, for error records.
    path: String,
    /// After `Close` (+0x18): requests that read it fail as on the console.
    closed: AtomicBool,
}

/// A request whose file was closed (the console's file id is -1 then).
const FILE_NOT_OPEN: HRESULT = 0x8924_0017_u32 as i32;

/// `Close` (+0x18): the console takes the file from its storage host and
/// closes the handle. Ours stops reads of it (the handle goes with the last
/// reference, so a read already running finishes).
extern "system" fn close_file(this: *mut Instance<File>) {
    // SAFETY: a file we created and the title still holds.
    let file = &unsafe { &*this }.state;
    if !file.closed.swap(true, Ordering::AcqRel) {
        tracing::debug!(path = file.path, "dstorage: file closed");
    }
}

/// +0x20: a duplicate of the file's handle (the caller closes it), or null
/// once the file is closed or if the host gives none.
extern "system" fn file_handle(this: *mut Instance<File>) -> *mut c_void {
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::Foundation::{DUPLICATE_SAME_ACCESS, DuplicateHandle};
    use windows_sys::Win32::System::Threading::GetCurrentProcess;
    // SAFETY: as above.
    let file = &unsafe { &*this }.state;
    if file.closed.load(Ordering::Acquire) {
        return std::ptr::null_mut();
    }
    let mut out = std::ptr::null_mut();
    // SAFETY: our own handle, duplicated into this process.
    let ok = unsafe {
        DuplicateHandle(
            GetCurrentProcess(),
            file.files.first.as_raw_handle(),
            GetCurrentProcess(),
            &mut out,
            0,
            0,
            DUPLICATE_SAME_ACCESS,
        )
    };
    if ok == 0 { std::ptr::null_mut() } else { out }
}

/// How files are read (`VICTORY_DSTORAGE_IO`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Io {
    /// `one`: all workers through the one handle, as before. The host does
    /// the reads of a handle one after the other.
    One,
    /// `handles`: a handle for each worker.
    Handles,
    /// `overlapped`: one handle, whose reads the host does side by side.
    Overlapped,
    /// `direct`: a handle for each worker, past the host's file cache.
    Direct,
}

impl Io {
    fn chosen() -> Io {
        static CHOSEN: OnceLock<Io> = OnceLock::new();
        *CHOSEN.get_or_init(|| {
            let io = match std::env::var("VICTORY_DSTORAGE_IO").as_deref().map(str::trim) {
                Ok("one") => Io::One,
                Ok("handles") => Io::Handles,
                Ok("overlapped") => Io::Overlapped,
                Ok("direct") => Io::Direct,
                _ => Io::DEFAULT,
            };
            tracing::info!(?io, "dstorage: how files are read (VICTORY_DSTORAGE_IO)");
            io
        })
    }

    const DEFAULT: Io = Io::Handles;
}

/// A file as the workers read it: the handle the title opened, and those
/// made from it when a worker first reads.
struct Files {
    first: std::fs::File,
    others: Box<[OnceLock<Option<std::fs::File>>]>,
    io: Io,
}

impl Files {
    fn new(first: std::fs::File, io: Io) -> Files {
        let workers = std::thread::available_parallelism().map_or(4, |n| n.get());
        let others = match io {
            Io::One => 0,
            Io::Overlapped => 1,
            Io::Handles | Io::Direct => workers,
        };
        Files { first, others: (0..others).map(|_| OnceLock::new()).collect(), io }
    }

    #[cfg(test)]
    fn open_for_test(path: &str, io: Io) -> Files {
        Files::new(std::fs::File::open(path).unwrap(), io)
    }

    /// Reads at `offset`; as [`FileExt::seek_read`].
    fn read_at(&self, out: &mut [u8], offset: u64) -> std::io::Result<usize> {
        use windows_sys::Win32::Storage::FileSystem::{FILE_FLAG_NO_BUFFERING, FILE_FLAG_OVERLAPPED};
        let flags = match self.io {
            Io::One | Io::Handles => 0,
            Io::Overlapped => FILE_FLAG_OVERLAPPED,
            Io::Direct => FILE_FLAG_OVERLAPPED | FILE_FLAG_NO_BUFFERING,
        };
        let other = match self.others.len() {
            0 => None,
            n => self.others[this_thread() % n].get_or_init(|| reopen(&self.first, flags)).as_ref(),
        };
        match (other, self.io) {
            (Some(file), Io::Overlapped) => read_overlapped(file, out, offset),
            (Some(file), Io::Direct) => read_direct(file, out, offset),
            (file, _) => file.unwrap_or(&self.first).seek_read(out, offset),
        }
    }
}

/// A number for this thread: 0 for the first that asks, and so on.
fn this_thread() -> usize {
    static NEXT: AtomicU32 = AtomicU32::new(0);
    victory_core::fast_local! {
        static NUMBER: usize = NEXT.fetch_add(1, Ordering::Relaxed) as usize;
    }
    NUMBER.with(|n| *n)
}

/// Another handle to `file`, for reading; `None` if the host gives none
/// (then the first one is used).
fn reopen(file: &std::fs::File, flags: u32) -> Option<std::fs::File> {
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::Foundation::{GENERIC_READ, INVALID_HANDLE_VALUE};
    use windows_sys::Win32::Storage::FileSystem::{FILE_SHARE_READ, ReOpenFile};
    // SAFETY: a file we hold open.
    let handle = unsafe { ReOpenFile(file.as_raw_handle(), GENERIC_READ, FILE_SHARE_READ, flags) };
    if handle == INVALID_HANDLE_VALUE {
        tracing::warn!(error = %std::io::Error::last_os_error(), flags, "dstorage: a file could not be opened again");
        return None;
    }
    // SAFETY: a handle just opened, ours alone.
    Some(unsafe { std::fs::File::from_raw_handle(handle) })
}

/// A read through a handle opened for overlapped reads, waited for.
fn read_overlapped(file: &std::fs::File, out: &mut [u8], offset: u64) -> std::io::Result<usize> {
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::Foundation::{CloseHandle, ERROR_HANDLE_EOF, ERROR_IO_PENDING, HANDLE};
    use windows_sys::Win32::Storage::FileSystem::ReadFile;
    use windows_sys::Win32::System::IO::{GetOverlappedResult, OVERLAPPED};
    use windows_sys::Win32::System::Threading::CreateEventW;
    /// Set when this thread's read is done.
    struct Event(HANDLE);
    impl Drop for Event {
        fn drop(&mut self) {
            // SAFETY: our own event.
            unsafe { CloseHandle(self.0) };
        }
    }
    victory_core::fast_local! {
        static DONE: Event =
            // SAFETY: a plain manual-reset event.
            Event(unsafe { CreateEventW(std::ptr::null(), 1, 0, std::ptr::null()) });
    }
    DONE.with(|done| {
        if done.0.is_null() {
            return Err(std::io::Error::last_os_error());
        }
        let mut overlapped = OVERLAPPED { hEvent: done.0, ..Default::default() };
        overlapped.Anonymous.Anonymous.Offset = offset as u32;
        overlapped.Anonymous.Anonymous.OffsetHigh = (offset >> 32) as u32;
        let len = out.len().min(1 << 30) as u32;
        let mut n = 0;
        // SAFETY: `out` and `overlapped` stay until the read is done, which
        // is waited for.
        let mut read = unsafe {
            ReadFile(file.as_raw_handle(), out.as_mut_ptr(), len, std::ptr::null_mut(), &mut overlapped)
        } != 0;
        let mut error = std::io::Error::last_os_error();
        if read || error.raw_os_error() == Some(ERROR_IO_PENDING as i32) {
            // SAFETY: as above.
            read = unsafe { GetOverlappedResult(file.as_raw_handle(), &overlapped, &mut n, 1) } != 0;
            error = std::io::Error::last_os_error();
        }
        match error.raw_os_error() {
            _ if read => Ok(n as usize),
            Some(code) if code == ERROR_HANDLE_EOF as i32 => Ok(0),
            _ => Err(error),
        }
    })
}

/// A read past the file cache: of the whole sectors around the bytes, into
/// a buffer of this thread's, unless `out` takes them as they are.
fn read_direct(file: &std::fs::File, out: &mut [u8], offset: u64) -> std::io::Result<usize> {
    /// Every disk's sectors are at most this; offsets, sizes and addresses
    /// are multiples of it.
    const SECTOR: usize = 4096;
    /// Read at a time.
    const MOST: usize = 1 << 20;
    let sector = SECTOR as u64;
    if offset.is_multiple_of(sector)
        && out.len().is_multiple_of(SECTOR)
        && (out.as_ptr() as usize).is_multiple_of(SECTOR)
    {
        return read_overlapped(file, out, offset);
    }
    victory_core::fast_local! {
        static BUFFER: std::cell::RefCell<Vec<u8>> = std::cell::RefCell::new(vec![0; MOST + SECTOR]);
    }
    BUFFER.with_borrow_mut(|buffer| {
        let skip = buffer.as_ptr().align_offset(SECTOR);
        let before = (offset % sector) as usize;
        let len = (before + out.len()).next_multiple_of(SECTOR).min(MOST);
        let aligned = &mut buffer[skip..skip + len];
        let read = read_overlapped(file, aligned, offset - before as u64)?;
        let n = read.saturating_sub(before).min(out.len());
        out[..n].copy_from_slice(&aligned[before..before + n]);
        Ok(n)
    })
}

/// Slots 5-7 are the console's own (destructor, open, file id).
static FILE_VTABLE: Methods<8> = Methods([
    query_file as *const (),
    add_ref_instance::<File> as *const (),
    release::<File> as *const (),
    close_file as *const (),  // 0x18
    file_handle as *const (), // 0x20
    missing::<5> as *const (),
    missing::<6> as *const (),
    missing::<7> as *const (),
]);

// Status arrays.

struct StatusArray {
    entries: Box<[AtomicI32]>,
    /// For the log: when each entry was written ([`stream_log::now`]); 0
    /// once the title has seen it.
    written: Box<[AtomicU64]>,
}

type Array = *mut Instance<StatusArray>;

/// The result of the requests before entry `index`: pending, the first
/// failure, or success; past the end, the console's out-of-range error.
extern "system" fn status_result(this: Array, index: u32) -> HRESULT {
    // SAFETY: an array we created and the title still holds.
    let entries = &unsafe { &*this }.state.entries;
    entries.get(index as usize).map_or_else(
        || {
            tracing::warn!(index, capacity = entries.len(), "dstorage: status entry out of range");
            INDEX_OUT_OF_RANGE
        },
        |e| e.load(Ordering::Acquire),
    )
}

/// The title polls this (entry 0, a `Sleep(1)` loop) and never
/// reads the result itself. An entry past the end is never complete.
extern "system" fn is_complete(this: Array, index: u32) -> bool {
    // SAFETY: an array we created and the title still holds.
    let Some(entry) = unsafe { &*this }.state.entries.get(index as usize) else {
        return false;
    };
    let done = entry.load(Ordering::Acquire) != E_PENDING;
    if done && stream_log::on() {
        // SAFETY: as above.
        let written = &unsafe { &*this }.state.written;
        if let Some(at) = written.get(index as usize).map(|w| w.swap(0, Ordering::Relaxed)).filter(|&at| at != 0) {
            stream_log::line(format_args!("P\t{:x}\t{at}\t{}", this as usize, stream_log::now()));
        }
    }
    done
}

static STATUS_VTABLE: Methods<5> = Methods([
    query_status as *const (),
    add_ref_instance::<StatusArray> as *const (),
    release::<StatusArray> as *const (),
    is_complete as *const (),   // 0x18
    status_result as *const (), // 0x20
]);

// Queues.

/// A queue object: its lane in the scheduler.
struct Queue {
    id: u64,
}

impl Drop for Queue {
    fn drop(&mut self) {
        close_lane(self.id);
    }
}

type QueueRef = *const Instance<Queue>;

/// Every queue's requests, run by a pool of workers.
struct Scheduler {
    lanes: Mutex<Lanes>,
    /// Requests to start, or staging space freed.
    work: Condvar,
    /// A request finished (for full queues and `close`).
    finished: Condvar,
}

/// Priority weights per class (low, normal, high, real-time): about 10x per
/// level, counted against request sizes. The console keeps the four
/// classes apart (`XCRDAPI.dll` hands its storage host 3, 2, 1 and 0 for
/// them); how its host weighs them is not in the library.
const COST: [u64; 4] = [1000, 100, 10, 1];
const CLASSES: usize = COST.len();
/// The console's default staging buffer.
const STAGING: u64 = 32 << 20;

struct Lanes {
    lanes: HashMap<u64, Lane>,
    next_id: u64,
    /// Submission order over all queues.
    next_seq: u64,
    /// Bytes of staging held by running requests, and the limit.
    staged: u64,
    staging: u64,
    /// A start waited for staging space since the last finish.
    starved: bool,
    /// Weighted round robin: each class's virtual time (bytes × cost), and
    /// the time of the last start; within a class, the lane clock of the
    /// last start.
    clock: [u64; CLASSES],
    now: u64,
    lane_now: [u64; CLASSES],
    /// Since the last debug summary: requests, bytes written, time spent
    /// running them.
    done: (u64, u64, std::time::Duration),
    refused: u64,
    /// BCPack bytes written and time spent on them.
    bcpack: (u64, std::time::Duration),
    since: Option<std::time::Instant>,
}

/// A queue's requests in the scheduler.
struct Lane {
    class: usize,
    capacity: usize,
    /// Made with `MANUAL_SUBMIT`: runs only what the title submits (see
    /// [`queue`]); otherwise it also submits itself at half capacity.
    manual: bool,
    /// Bytes started, for round robin among the lanes of a class.
    clock: u64,
    /// Enqueued, not yet submitted.
    unsubmitted: Vec<Command>,
    /// Submitted, by sequence number: reads not started, status entries
    /// not written, reads running, and failures not yet reported.
    reads: VecDeque<(u64, Read)>,
    statuses: VecDeque<(u64, Mark)>,
    running: BTreeSet<u64>,
    failures: BTreeMap<u64, HRESULT>,
    closed: bool,
    /// For `Query`: the description's source type and name pointer.
    source_type: u32,
    name: *const c_char,
    /// For `GetErrorEvent` and `RetrieveErrorRecord`.
    errors: Errors,
}

/// A queue's failures since it was made, as its error record counts them.
#[derive(Default)]
struct Errors {
    count: u32,
    /// The first: its result, its request, and (taken when it failed) its
    /// file's path and its name.
    first: Option<(HRESULT, Request, String, String)>,
    /// Set at the first failure (auto-reset); 0 without one.
    event: usize,
}

impl Drop for Errors {
    fn drop(&mut self) {
        if self.event != 0 {
            // SAFETY: our own event.
            unsafe { windows_sys::Win32::Foundation::CloseHandle(self.event as *mut c_void) };
        }
    }
}

// SAFETY: requests point into title memory and our own objects, which stay
// valid until the title has seen them complete; status arrays count their
// references atomically.
unsafe impl Send for Lane {}

/// A read a worker runs.
struct Job {
    lane: u64,
    seq: u64,
    request: Request,
    note: Note,
    staging: u64,
}

// SAFETY: as for `Lane`.
unsafe impl Send for Job {}

/// The scheduler, with its workers started on first use; `None` if not
/// even one worker could start.
fn scheduler() -> Option<&'static Scheduler> {
    static SCHEDULER: OnceLock<Option<Scheduler>> = OnceLock::new();
    static STARTED: std::sync::Once = std::sync::Once::new();
    let scheduler = SCHEDULER
        .get_or_init(|| {
            Some(Scheduler {
                lanes: Mutex::new(Lanes {
                    lanes: HashMap::new(),
                    next_id: 0,
                    next_seq: 0,
                    staged: 0,
                    staging: STAGING,
                    starved: false,
                    clock: [0; CLASSES],
                    now: 0,
                    lane_now: [0; CLASSES],
                    done: Default::default(),
                    refused: 0,
                    bcpack: Default::default(),
                    since: None,
                }),
                work: Condvar::new(),
                finished: Condvar::new(),
            })
        })
        .as_ref()?;
    STARTED.call_once(|| {
        let workers = std::thread::available_parallelism().map_or(4, |n| n.get());
        let started = (0..workers)
            .filter(|i| {
                std::thread::Builder::new()
                    .name(format!("dstorage {i}"))
                    .spawn(|| work(scheduler))
                    .is_ok()
            })
            .count();
        tracing::info!(workers = started, "dstorage: workers");
    });
    Some(scheduler)
}

impl Scheduler {
    fn lock(&self) -> MutexGuard<'_, Lanes> {
        self.lanes.lock().unwrap_or_else(|e| e.into_inner())
    }
}

/// The walls trace (`VICTORY_WALLS_TRACE`): 0 or unset: off; 1: a line for
/// every read request that fails, with its file; 2: for every request (the
/// list of a launch's reads, by file and offset, is what a launch with the
/// first room's walls and one without are compared by).
fn walls_trace() -> u8 {
    static ON: OnceLock<u8> = OnceLock::new();
    *ON.get_or_init(|| {
        std::env::var("VICTORY_WALLS_TRACE").ok().and_then(|v| v.trim().parse::<u8>().ok()).unwrap_or(0)
    })
}

/// The files opened, by the address of our file object, with their paths
/// (for the walls trace: a request names its file by that address).
static WALLS_FILES: Mutex<Vec<(usize, String)>> = Mutex::new(Vec::new());

/// The walls trace's line for a request that is done.
fn walls_request(request: &Request, result: &Result<(), HRESULT>, started: std::time::Instant) {
    let level = walls_trace();
    if level == 0 || (level == 1 && result.is_ok()) {
        return;
    }
    let file = if request.options & MEMORY_SOURCE != 0 {
        "memory".to_string()
    } else {
        let files = WALLS_FILES.lock().unwrap_or_else(|e| e.into_inner());
        let source = request.source as usize;
        files.iter().find(|f| f.0 == source).map_or_else(|| "?".to_string(), |f| f.1.clone())
    };
    let hr = match result {
        Ok(()) => 0,
        Err(hr) => *hr as u32,
    };
    tracing::info!(
        file,
        offset = request.offset,
        source_size = request.source_size,
        destination_size = request.destination_size,
        to_pages = request.options & PAGE_DESTINATION != 0,
        options = format_args!("{:#x}", request.options),
        ms = started.elapsed().as_millis() as u64,
        ok = result.is_ok(),
        hr = format_args!("{hr:#010x}"),
        "walls trace: a read request is done"
    );
}

/// The streaming log (`VICTORY_DSTORAGE_LOG=<file>`): a line for every
/// request (`R`: queue, number, file, offset, source bytes, destination
/// bytes, options, when enqueued, submitted, started, done, result), every
/// status entry written (`S`: array, number, when enqueued, when written),
/// the title's first look at it after (`P`: array, when written, when
/// seen), every file opened (`F`), and each second what the title's mapping
/// calls cost (`M`, `xmem::map_costs`). Times are milliseconds since the
/// Unix epoch.
pub(crate) mod stream_log {
    use super::*;
    use std::io::Write;

    type Log = Mutex<std::io::BufWriter<std::fs::File>>;

    fn log() -> Option<&'static Log> {
        static LOG: OnceLock<Option<Log>> = OnceLock::new();
        LOG.get_or_init(|| {
            let path = std::env::var_os("VICTORY_DSTORAGE_LOG")?;
            let file = std::fs::File::create(&path).ok()?;
            std::thread::spawn(|| {
                loop {
                    std::thread::sleep(std::time::Duration::from_secs(1));
                    if let Some(costs) = super::super::xmem::map_costs() {
                        line(format_args!("M\t{}\t{costs}", now()));
                    }
                    if let Some(log) = log() {
                        let _ = log.lock().unwrap_or_else(|e| e.into_inner()).flush();
                    }
                }
            });
            Some(Mutex::new(std::io::BufWriter::with_capacity(1 << 20, file)))
        })
        .as_ref()
    }

    pub(crate) fn on() -> bool {
        static ON: OnceLock<bool> = OnceLock::new();
        *ON.get_or_init(|| std::env::var_os("VICTORY_DSTORAGE_LOG").is_some())
    }

    /// Milliseconds since the Unix epoch.
    pub(super) fn now() -> u64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |t| t.as_millis() as u64)
    }

    pub(super) fn line(text: std::fmt::Arguments) {
        if let Some(log) = log() {
            let _ = writeln!(log.lock().unwrap_or_else(|e| e.into_inner()), "{text}");
        }
    }

    pub(super) fn request(job: &Job, started: std::time::Instant, result: &Result<(), HRESULT>) {
        if !on() {
            return;
        }
        let now = now();
        let ago = |t: Option<std::time::Instant>| now - t.map_or(0, |t| t.elapsed().as_millis() as u64);
        let r = &job.request;
        line(format_args!(
            "R\t{}\t{}\t{:x}\t{}\t{}\t{}\t{:x}\t{}\t{}\t{}\t{now}\t{:x}",
            job.lane,
            job.seq,
            r.source as usize,
            r.offset,
            r.source_size,
            r.destination_size,
            r.options,
            ago(job.note.enqueued),
            ago(job.note.submitted),
            ago(Some(started)),
            result.err().unwrap_or(0)
        ));
    }
}

/// A worker: runs the next read of the most deserving queue.
fn work(scheduler: &'static Scheduler) {
    let mut lanes = scheduler.lock();
    loop {
        let Some(job) = lanes.start() else {
            lanes = scheduler.work.wait(lanes).unwrap_or_else(|e| e.into_inner());
            continue;
        };
        drop(lanes);
        let started = std::time::Instant::now();
        let traced = Traced::start(&job);
        let reading = victory_gpu::timeline::span("streaming: a request");
        let result = execute(&job.request);
        reading.end();
        walls_request(&job.request, &result, started);
        stream_log::request(&job, started, &result);
        if let Some(traced) = traced {
            traced.end(&job, &result);
        }
        lanes = scheduler.lock();
        lanes.finish(&job, result);
        lanes.count(&job, started);
        if std::mem::take(&mut lanes.starved) {
            scheduler.work.notify_all();
        }
        scheduler.finished.notify_all();
    }
}

impl Lanes {
    /// Counts a finished request; every 5 s a debug line sums them up.
    fn count(&mut self, job: &Job, started: std::time::Instant) {
        let (requests, bytes, busy) = &mut self.done;
        *requests += 1;
        *bytes += u64::from(job.request.destination_size);
        *busy += started.elapsed();
        if job.request.options & BCPACK != 0 {
            self.bcpack.0 += u64::from(job.request.destination_size);
            self.bcpack.1 += started.elapsed();
        }
        let since = *self.since.get_or_insert(started);
        let elapsed = since.elapsed();
        if elapsed.as_secs() >= 5 {
            let waiting: usize = self.lanes.values().map(|l| l.reads.len()).sum();
            let reading = READING.swap(0, Ordering::Relaxed) as f64 / 1e9;
            tracing::debug!(
                requests = *requests,
                mb_per_s = format_args!("{:.1}", *bytes as f64 / elapsed.as_secs_f64() / 1e6),
                busy_workers = format_args!("{:.2}", busy.as_secs_f64() / elapsed.as_secs_f64()),
                reading_workers = format_args!("{:.2}", reading / elapsed.as_secs_f64()),
                staging_waits = std::mem::take(&mut self.refused),
                bcpack_mb = self.bcpack.0 / 1_000_000,
                bcpack_workers = format_args!("{:.2}", self.bcpack.1.as_secs_f64() / elapsed.as_secs_f64()),
                waiting,
                "dstorage: last {:.1} s",
                elapsed.as_secs_f64()
            );
            self.done = Default::default();
            self.bcpack = Default::default();
            self.since = None;
        }
    }

    /// Starts the next read of the class with the least virtual time, from
    /// its lane that has started the fewest bytes, if the staging buffer
    /// has room for it (or nothing else holds any).
    fn start(&mut self) -> Option<Job> {
        let busy = || self.lanes.iter().filter(|(_, l)| !l.reads.is_empty());
        let (_, class) = busy().map(|(_, l)| (self.clock[l.class], l.class)).min()?;
        let (_, _, id) = busy()
            .filter(|(_, l)| l.class == class)
            .filter_map(|(&id, l)| Some((l.clock, l.reads.front()?.0, id)))
            .min()?;
        let lane = self.lanes.get_mut(&id)?;
        let &(seq, Read { request, note, .. }) = lane.reads.front()?;
        let staging = staging_bytes(&request);
        if self.staged > 0 && self.staged + staging > self.staging {
            self.starved = true;
            self.refused += 1;
            return None;
        }
        lane.reads.pop_front();
        lane.running.insert(seq);
        self.staged += staging;
        let size = u64::from(request.destination_size.max(request.source_size)).max(1);
        self.now = self.clock[class];
        self.clock[class] += size * COST[class];
        self.lane_now[class] = lane.clock;
        lane.clock += size;
        Some(Job {
            lane: id,
            seq,
            request,
            note,
            staging,
        })
    }

    fn finish(&mut self, job: &Job, result: Result<(), HRESULT>) {
        self.staged -= job.staging;
        if let Some(lane) = self.lanes.get_mut(&job.lane) {
            lane.running.remove(&job.seq);
            if let Err(hr) = result {
                lane.failures.insert(job.seq, hr);
                lane.errors.failed(hr, &job.request);
            }
            lane.flush();
        }
    }

    /// Moves a queue's enqueued commands to the workers; `early` when the
    /// title has not asked for it ([`queue`]).
    fn submit(&mut self, id: u64, early: bool) {
        let Some(class) = self.lanes.get(&id).map(|l| l.class) else {
            return;
        };
        // A class that had nothing waiting starts from now, not from the
        // credit it left behind.
        if !self
            .lanes
            .values()
            .any(|l| l.class == class && !l.reads.is_empty())
        {
            self.clock[class] = self.clock[class].max(self.now);
        }
        let Some(lane) = self.lanes.get_mut(&id) else {
            return;
        };
        if lane.reads.is_empty() {
            lane.clock = lane.clock.max(self.lane_now[class]);
        }
        trace_batch(id, &lane.unsubmitted, early);
        for command in std::mem::take(&mut lane.unsubmitted) {
            let seq = self.next_seq;
            self.next_seq += 1;
            match command {
                // Cancelled before it was submitted: never runs.
                Command::Read(read) if read.cancelled => {
                    lane.failures.insert(seq, CANCELLED);
                }
                Command::Read(mut read) => {
                    read.note.submitted(&read.request, early);
                    lane.reads.push_back((seq, read));
                }
                Command::Status(status) => lane.statuses.push_back((seq, Mark::Status(status))),
                Command::Signal(signal) => lane.statuses.push_back((seq, Mark::Signal(signal))),
                Command::Event(event) => lane.statuses.push_back((seq, Mark::Event(event))),
            }
        }
        lane.flush();
    }
}

impl Lane {
    fn new(priority: i8, capacity: u16, manual: bool) -> Lane {
        Lane {
            // Low (-1) to real-time (2): classes 0 to 3.
            class: (priority.clamp(-1, 2) + 1) as usize,
            capacity: usize::from(capacity),
            manual,
            clock: 0,
            unsubmitted: Vec::new(),
            reads: VecDeque::new(),
            statuses: VecDeque::new(),
            running: BTreeSet::new(),
            failures: BTreeMap::new(),
            closed: false,
            source_type: 0,
            name: std::ptr::null(),
            errors: Errors::default(),
        }
    }

    /// Requests and status entries held, submitted or not.
    fn entries(&self) -> usize {
        self.unsubmitted.len() + self.reads.len() + self.statuses.len() + self.running.len()
    }

    /// Writes the status entries, signals the fences and sets the events
    /// whose earlier reads have all finished. Only status entries take the
    /// failures before them.
    fn flush(&mut self) {
        while let Some(&(seq, _)) = self.statuses.front() {
            let waiting = self.reads.front().map(|r| r.0);
            if waiting.into_iter().chain(self.running.first().copied()).any(|s| s < seq) {
                return;
            }
            let Some((_, mark)) = self.statuses.pop_front() else {
                return;
            };
            let status = match mark {
                Mark::Status(status) => status,
                Mark::Signal(signal) => {
                    signal.signal();
                    continue;
                }
                Mark::Event(event) => {
                    // SAFETY: the title's event handle.
                    unsafe { windows_sys::Win32::System::Threading::SetEvent(event as *mut c_void) };
                    continue;
                }
            };
            let later = self.failures.split_off(&seq);
            let first = std::mem::replace(&mut self.failures, later)
                .into_values()
                .next();
            if stream_log::on() {
                let at = stream_log::now();
                stream_log::line(format_args!("S\t{:x}\t{seq}\t{}\t{at}", status.array as usize, status.enqueued));
                status.written().store(at, Ordering::Relaxed);
            }
            status
                .entry()
                .store(first.unwrap_or(S_OK), Ordering::Release);
        }
    }
}

/// Staging bytes a request holds while it runs: what it buffers between
/// the file and the destination. Raw reads go straight to their
/// destination, memory sources need none (as on the console).
fn staging_bytes(request: &Request) -> u64 {
    let buffered = request.options & (ZLIB | BCPACK | PAGE_DESTINATION) != 0;
    if buffered && request.options & MEMORY_SOURCE == 0 {
        u64::from(request.source_size)
    } else {
        0
    }
}

/// Ends a queue: what was not started is dropped, and this waits for the
/// reads already running (they write title memory).
fn close_lane(id: u64) {
    let Some(scheduler) = scheduler() else {
        return;
    };
    let mut lanes = scheduler.lock();
    let Some(lane) = lanes.lanes.get_mut(&id) else {
        return;
    };
    lane.closed = true;
    lane.unsubmitted.clear();
    lane.reads.clear();
    lane.statuses.clear(); // releases their arrays
    while lanes.lanes.get(&id).is_some_and(|l| !l.running.is_empty()) {
        lanes = scheduler
            .finished
            .wait(lanes)
            .unwrap_or_else(|e| e.into_inner());
    }
    lanes.lanes.remove(&id);
    scheduler.finished.notify_all();
}

/// A read request as the title builds it, 0x48 bytes.
#[repr(C)]
#[derive(Clone, Copy)]
struct Request {
    options: u64,
    /// An address, or with `PAGE_DESTINATION` an array of 64 KiB page
    /// numbers.
    destination: *mut c_void,
    /// With `PAGE_DESTINATION`, the low 16 bits are where in the first page
    /// the destination starts.
    page_offset: u64,
    destination_size: u32,
    /// Our file object, or with `MEMORY_SOURCE` an address.
    source: *const c_void,
    offset: u64,
    source_size: u32,
    /// Compressed size between two decompression stages; unused here.
    intermediate_size: u32,
    /// Matched by cancellation; the title leaves it 0.
    tag: u64,
    name: *const c_char,
}

// Request options.
/// Inflate zlib (RFC 1950) data.
const ZLIB: u64 = 1;
/// Bits 1-4: decode BCPack data of this BC format number (0: none).
const BCPACK: u64 = 0xf << 1;
const PAGE_DESTINATION: u64 = 1 << 21;
const MEMORY_SOURCE: u64 = 1 << 22;
// Bits 5-20 select a further transform, bit 23 a source in physical pages;
// The title's packages need neither, so requests with them fail.

const PAGE: usize = 64 * 1024;

enum Command {
    Read(Read),
    Status(Status),
    /// `EnqueueSignal`: a fence to move to a value.
    Signal(Signal),
    /// `EnqueueSetEvent`: an event handle to set.
    Event(usize),
}

/// What a queue does once every command before it has finished.
enum Mark {
    Status(Status),
    Signal(Signal),
    Event(usize),
}

/// What a request cancelled before it started reports
/// (`CancelRequestsWithTag`): the console's storage host decides this, not
/// its library; `E_ABORT` here.
const CANCELLED: HRESULT = 0x8000_4004_u32 as i32;

/// A fence to signal, holding the reference its `QueryInterface` gave.
struct Signal {
    fence: *mut c_void,
    value: u64,
}

impl Signal {
    /// `ID3D12Fence::Signal` (+0x50) from the CPU, as the console's host
    /// writes the fence's value.
    fn signal(&self) {
        // SAFETY: a fence object (ours, from victory-gpu) the title passed.
        let f: extern "system" fn(*mut c_void, u64) -> HRESULT = unsafe { com(self.fence, 10) };
        f(self.fence, self.value);
    }
}

impl Drop for Signal {
    fn drop(&mut self) {
        // SAFETY: as above; the reference is ours.
        let release: extern "system" fn(*mut c_void) -> u32 = unsafe { com(self.fence, 2) };
        release(self.fence);
    }
}

/// Method `slot` of a COM object.
///
/// # Safety
/// `object` is a live COM object and `F` the method's signature.
unsafe fn com<F: Copy>(object: *mut c_void, slot: usize) -> F {
    // SAFETY: by the caller.
    unsafe {
        let table = *object.cast::<*const *const c_void>();
        std::mem::transmute_copy(&*table.add(slot))
    }
}

impl Errors {
    fn failed(&mut self, hr: HRESULT, request: &Request) {
        self.count = self.count.saturating_add(1);
        if self.first.is_none() {
            let path = if request.options & MEMORY_SOURCE == 0 && !request.source.is_null() {
                // SAFETY: a file object we created, held by the title until completion.
                unsafe { &*request.source.cast::<Instance<File>>() }.state.path.clone()
            } else {
                String::new()
            };
            self.first = Some((hr, *request, path, text(request.name)));
            if self.event != 0 {
                // SAFETY: our own event.
                unsafe { windows_sys::Win32::System::Threading::SetEvent(self.event as *mut c_void) };
            }
        }
    }
}

/// An enqueued request, and what the trace knows of it.
#[derive(Clone, Copy)]
struct Read {
    request: Request,
    note: Note,
    /// Cancelled while not yet submitted (`CancelRequestsWithTag`).
    cancelled: bool,
}

/// The streaming trace (`VICTORY_DSTORAGE_TRACE`): 0 or unset: off; 1: a
/// line for every batch, and for every request whose page list, or whose
/// pages' mapping, changed when it should not have, or whose bytes another
/// running request writes too; 2: a line for every request.
fn trace() -> u8 {
    static ON: OnceLock<u8> = OnceLock::new();
    *ON.get_or_init(|| {
        std::env::var("VICTORY_DSTORAGE_TRACE").ok().and_then(|v| v.trim().parse().ok()).unwrap_or(0)
    })
}

/// When a request was enqueued and submitted, and what its page list held
/// then (a hash; 0 without a list). Only filled in for the trace, but for
/// `early`.
#[derive(Clone, Copy, Default)]
struct Note {
    enqueued: Option<std::time::Instant>,
    at_enqueue: u64,
    submitted: Option<std::time::Instant>,
    at_submit: u64,
    /// Started before the title's submit.
    early: bool,
}

impl Note {
    fn enqueued(request: &Request) -> Note {
        match trace() {
            0 if stream_log::on() => Note { enqueued: Some(std::time::Instant::now()), ..Note::default() },
            0 => Note::default(),
            _ => Note {
                enqueued: Some(std::time::Instant::now()),
                at_enqueue: list_hash(request),
                ..Note::default()
            },
        }
    }

    fn submitted(&mut self, request: &Request, early: bool) {
        self.early = early;
        if trace() > 0 || stream_log::on() {
            self.submitted = Some(std::time::Instant::now());
        }
        if trace() > 0 {
            self.at_submit = list_hash(request);
        }
    }
}

/// A hash of the page numbers a request's list holds now; 0 without one.
fn list_hash(request: &Request) -> u64 {
    list_of(request).map_or(0, |pages| {
        // FNV-1a; never 0.
        pages.iter().fold(0xcbf2_9ce4_8422_2325_u64, |h, &p| (h ^ p as u64).wrapping_mul(0x100_0000_01b3)) | 1
    })
}

/// The page numbers a request's list holds now.
fn list_of(request: &Request) -> Option<Vec<usize>> {
    (request.options & PAGE_DESTINATION != 0 && !request.destination.is_null()).then(|| {
        let (offset, len) = Traced::bytes(request);
        read_list(request.destination.cast(), offset, len)
    })
}

/// The trace's line for a batch: how much of it goes to pages, and how many
/// of those lists the title filled in since it enqueued them (its protocol:
/// most of them; a batch started `early` runs on what they held before).
fn trace_batch(queue: u64, commands: &[Command], early: bool) {
    if trace() == 0 {
        return;
    }
    let reads = || {
        commands.iter().filter_map(|c| match c {
            Command::Read(read) => Some(read),
            _ => None,
        })
    };
    let (mut to_pages, mut filled, mut empty) = (0, 0, 0);
    for read in reads().filter(|r| r.request.options & PAGE_DESTINATION != 0) {
        to_pages += 1;
        filled += usize::from(list_hash(&read.request) != read.note.at_enqueue);
        empty += usize::from(list_of(&read.request).is_some_and(|l| l.contains(&0)));
    }
    tracing::info!(
        queue,
        early,
        commands = commands.len(),
        to_pages,
        lists_filled_since_enqueued = filled,
        lists_not_filled = empty,
        oldest_ms = reads().next().and_then(|r| r.note.enqueued).map_or(0, |t| t.elapsed().as_millis() as u64),
        "dstorage trace: a batch"
    );
}

/// The bytes of pages that running requests write, for the trace: by page
/// number, the request and its bytes in the page.
type Writers = HashMap<usize, Vec<(u64, usize, usize)>>;
static WRITERS: Mutex<Option<Writers>> = Mutex::new(None);

/// A running request in the trace.
struct Traced {
    pages: Vec<usize>,
    at_start: u64,
    shown: Vec<(usize, usize)>,
}

impl Traced {
    fn start(job: &Job) -> Option<Traced> {
        if trace() == 0 {
            return None;
        }
        let at_start = list_hash(&job.request);
        let pages = list_of(&job.request).unwrap_or_default();
        let (offset, len) = Traced::bytes(&job.request);
        {
            let mut writers = WRITERS.lock().unwrap_or_else(|e| e.into_inner());
            let writers = writers.get_or_insert_with(Writers::new);
            for (i, &page) in pages.iter().enumerate().filter(|(_, p)| **p != 0) {
                let from = offset.max(i * PAGE) - i * PAGE;
                let to = (offset + len).min((i + 1) * PAGE) - i * PAGE;
                let others = writers.entry(page).or_default();
                for &(other, a, b) in others.iter().filter(|&&(_, a, b)| a < to && from < b) {
                    tracing::warn!(
                        page,
                        from,
                        to,
                        seq = job.seq,
                        other,
                        other_from = a,
                        other_to = b,
                        "dstorage trace: two running requests write the same bytes of a page"
                    );
                }
                others.push((job.seq, from, to));
            }
        }
        let shown = super::xmem::pages_shown(&pages, offset, len);
        Some(Traced { pages, at_start, shown })
    }

    /// Where in its first page a request's destination starts, and its size.
    fn bytes(request: &Request) -> (usize, usize) {
        ((request.page_offset & 0xffff) as usize, request.destination_size as usize)
    }

    fn end(self, job: &Job, result: &Result<(), HRESULT>) {
        let (offset, len) = Traced::bytes(&job.request);
        let at_end = list_hash(&job.request);
        let moved = super::xmem::pages_shown(&self.pages, offset, len) != self.shown;
        {
            let mut writers = WRITERS.lock().unwrap_or_else(|e| e.into_inner());
            let writers = writers.get_or_insert_with(Writers::new);
            for page in &self.pages {
                if let Some(others) = writers.get_mut(page) {
                    others.retain(|w| w.0 != job.seq);
                    if others.is_empty() {
                        writers.remove(page);
                    }
                }
            }
        }
        let note = &job.note;
        // A request of the title's submit: its list is filled in by then,
        // and stays.
        let late = note.at_submit != self.at_start || self.at_start != at_end;
        if trace() < 2 && !late && !moved && !note.early && result.is_ok() {
            return;
        }
        let since = |t: Option<std::time::Instant>| t.map_or(0, |t| t.elapsed().as_millis() as u64);
        tracing::info!(
            seq = job.seq,
            queue = job.lane,
            early = note.early,
            list = format_args!("{:p}", job.request.destination),
            page_offset = job.request.page_offset,
            size = len,
            enqueued_ms_ago = since(note.enqueued),
            submitted_ms_ago = since(note.submitted),
            at_enqueue = format_args!("{:016x}", note.at_enqueue),
            at_submit = format_args!("{:016x}", note.at_submit),
            at_start = format_args!("{:016x}", self.at_start),
            at_end = format_args!("{at_end:016x}"),
            list_changed_late = late,
            mapping_changed = moved,
            first_page = self.pages.first().copied().unwrap_or(0),
            ok = result.is_ok(),
            "dstorage trace: a request"
        );
    }
}

/// A status entry to write, holding a reference on its array.
struct Status {
    array: Array,
    index: u32,
    /// For the log.
    enqueued: u64,
}

impl Drop for Status {
    fn drop(&mut self) {
        release(self.array);
    }
}

impl Status {
    fn entry(&self) -> &AtomicI32 {
        // SAFETY: we hold a reference; the index was checked when enqueued.
        &unsafe { &*self.array }.state.entries[self.index as usize]
    }

    fn written(&self) -> &AtomicU64 {
        // SAFETY: as above.
        &unsafe { &*self.array }.state.written[self.index as usize]
    }
}

extern "system" fn enqueue_request(this: QueueRef, request: *const Request) {
    // SAFETY: the title passes its request (null tolerated).
    let Some(&request) = (unsafe { request.as_ref() }) else {
        return;
    };
    let note = Note::enqueued(&request);
    queue(this, Command::Read(Read { request, note, cancelled: false }));
}

extern "system" fn enqueue_status(this: QueueRef, array: Array, index: u32) {
    // SAFETY: the title passes an array we created (null tolerated).
    let Some(entries) = (unsafe { array.as_ref() }).map(|a| &a.state.entries) else {
        return;
    };
    // As the console: a closed queue, or an entry past the end, is an error
    // the title is not told of, and the entry stays as it was.
    if !is_open(this, "EnqueueStatus") {
        return;
    }
    let Some(entry) = entries.get(index as usize) else {
        tracing::warn!(
            index,
            hr = format_args!("{:#010x}", INDEX_OUT_OF_RANGE as u32),
            "dstorage: status entry out of range"
        );
        return;
    };
    entry.store(E_PENDING, Ordering::Release);
    add_ref_instance(array);
    let enqueued = stream_log::now();
    queue(this, Command::Status(Status { array, index, enqueued }));
}

/// Whether the queue is still open; if not, logs the console's error for
/// `method` (which then does nothing).
fn is_open(this: QueueRef, method: &str) -> bool {
    // SAFETY: a queue we created and the title still holds.
    let id = unsafe { &*this }.state.id;
    let open = scheduler()
        .is_some_and(|s| s.lock().lanes.get(&id).is_some_and(|l| !l.closed));
    if !open {
        tracing::warn!(
            queue = id,
            method,
            hr = format_args!("{:#010x}", QUEUE_CLOSED as u32),
            "dstorage: a method of a closed queue"
        );
    }
    open
}

// When a queue submits itself (`Lane::manual`). As `XCRDAPI.dll`: a queue
// made without `MANUAL_SUBMIT` submits itself when the commands enqueued
// since the last submit reach half its capacity; one made with it (as the
// title makes all of its queues) only when the title submits. That matters:
// the title's batches (up to 4,095 requests, in queues of 4,159) hold more
// than half now and then, and it maps the pages they go to and fills in
// their page lists in its own submit, after it has enqueued them all (image
// +0xe88f0: the batch's mapping list first, at +0xf2c470, which writes the
// page lists at +0x55aa6a0; then the status entry; then our submit). When
// ours submitted at half regardless, 2,079 requests, three or four times a
// load, ran on lists of page 0, or, worse, of the pages the addresses had
// shown before: their data went into whatever those pages belonged to by
// then, and the pages it was for stayed as they were.

/// Adds a command to the queue; blocks while the queue is full. A queue
/// that is full of commands nobody submitted submits itself, since nothing
/// else could make room.
fn queue(this: QueueRef, command: Command) {
    // SAFETY: a queue we created and the title still holds.
    let id = unsafe { &*this }.state.id;
    let Some(scheduler) = scheduler() else {
        return;
    };
    let mut lanes = scheduler.lock();
    loop {
        let Some(lane) = lanes.lanes.get(&id).filter(|l| !l.closed) else {
            drop(lanes);
            is_open(this, "EnqueueEntry"); // the console's error, logged
            return; // closed: dropped (a status releases its array)
        };
        // One slot stays free, as the console's count of empty slots says.
        if lane.entries() + 1 < lane.capacity.max(2) {
            break;
        }
        if !lane.unsubmitted.is_empty() && lane.reads.is_empty() && lane.running.is_empty() {
            tracing::warn!(
                queue = id,
                commands = lane.unsubmitted.len(),
                "dstorage: a queue full of commands that are not submitted submits itself"
            );
            lanes.submit(id, true);
            scheduler.work.notify_all();
            continue;
        }
        lanes = scheduler
            .finished
            .wait(lanes)
            .unwrap_or_else(|e| e.into_inner());
    }
    let Some(lane) = lanes.lanes.get_mut(&id) else {
        return;
    };
    lane.unsubmitted.push(command);
    // The console counts commands since the last submit and submits when
    // the count is exactly half the capacity.
    if !lane.manual && lane.unsubmitted.len() == lane.capacity / 2 {
        lanes.submit(id, true);
        scheduler.work.notify_all();
    }
}

/// The title's submit, where its page lists become valid ([`Lane::manual`]):
/// the pages are mapped just before, also those under addresses without a
/// list (memory only the CPU sees).
extern "system" fn submit(this: QueueRef) {
    let _called = victory_gpu::timeline::span("the title's DirectStorage Submit");
    if !is_open(this, "Submit") {
        return;
    }
    // SAFETY: as above.
    let id = unsafe { &*this }.state.id;
    let Some(scheduler) = scheduler() else {
        return;
    };
    let mut lanes = scheduler.lock();
    let pending = lanes.lanes.get(&id).map_or(0, |l| l.unsubmitted.len());
    tracing::debug!(commands = pending, "dstorage: submit");
    lanes.submit(id, false);
    scheduler.work.notify_all();
}

/// Ends the queue whatever its reference count: what was not started never
/// completes, and this returns once the reads already running have
/// finished. The title calls this, not `Release`.
extern "system" fn close(this: QueueRef) {
    if !is_open(this, "Close") {
        return;
    }
    // SAFETY: as above.
    close_lane(unsafe { &*this }.state.id);
}

/// Runs `f` on the queue's lane if the queue is open (else logs the
/// console's error for `method`).
fn with_lane<R>(this: QueueRef, method: &str, f: impl FnOnce(&mut Lane) -> R) -> Option<R> {
    // SAFETY: a queue we created and the title still holds.
    let id = unsafe { &*this }.state.id;
    let scheduler = scheduler()?;
    let mut lanes = scheduler.lock();
    match lanes.lanes.get_mut(&id).filter(|l| !l.closed) {
        Some(lane) => Some(f(lane)),
        None => {
            drop(lanes);
            is_open(this, method);
            None
        }
    }
}

/// `EnqueueSignal` (+0x28): once every command before it has finished, the
/// fence moves to `value`. As the console, the fence is asked for
/// `ID3D12Fence1` first, and a fence that is not one is the error.
extern "system" fn enqueue_signal(this: QueueRef, fence: *mut c_void, value: u64) {
    if !is_open(this, "EnqueueSignal") || fence.is_null() {
        return;
    }
    let mut fence1 = std::ptr::null_mut();
    // SAFETY: the title passes a D3D12 fence.
    let query: extern "system" fn(*mut c_void, *const Guid, Out) -> HRESULT = unsafe { com(fence, 0) };
    let hr = query(fence, &FENCE1, &mut fence1);
    if hr < 0 || fence1.is_null() {
        tracing::warn!(hr = format_args!("{:#010x}", hr as u32), "dstorage: EnqueueSignal: not a fence");
        return;
    }
    queue(this, Command::Signal(Signal { fence: fence1, value }));
}

/// `EnqueueSetEvent` (+0x60): once every command before it has finished,
/// the event is set.
extern "system" fn enqueue_set_event(this: QueueRef, event: *mut c_void) {
    if is_open(this, "EnqueueSetEvent") {
        queue(this, Command::Event(event as usize));
    }
}

/// `CancelRequestsWithTag` (+0x38): requests whose `tag & mask == value`
/// and that have not started are dropped and report [`CANCELLED`]; those
/// running finish. As the console, a value with no bits in the mask is a
/// warning (nothing can match).
extern "system" fn cancel_with_tag(this: QueueRef, mask: u64, value: u64) {
    if value & mask == 0 && value != 0 {
        tracing::warn!(
            mask = format_args!("{mask:#x}"),
            value = format_args!("{value:#x}"),
            "dstorage: CancelRequestsWithTag: mask & value == 0, cancellation is a no-op"
        );
    }
    let matches = |r: &Request| r.tag & mask == value;
    let cancelled = with_lane(this, "CancelRequestsWithTag", |lane| {
        let mut n = 0;
        for command in &mut lane.unsubmitted {
            if let Command::Read(read) = command {
                if !read.cancelled && matches(&read.request) {
                    read.cancelled = true;
                    n += 1;
                }
            }
        }
        let (gone, kept): (VecDeque<_>, VecDeque<_>) =
            std::mem::take(&mut lane.reads).into_iter().partition(|(_, r)| matches(&r.request));
        lane.reads = kept;
        for (seq, _) in &gone {
            lane.failures.insert(*seq, CANCELLED);
        }
        n += gone.len();
        lane.flush();
        n
    });
    if let Some(n) = cancelled {
        tracing::debug!(n, mask, value, "dstorage: requests cancelled");
        if let Some(s) = scheduler() {
            s.finished.notify_all(); // slots freed
        }
    }
}

/// `GetErrorEvent` (+0x48): a duplicate of the queue's error event (the
/// caller closes it), set at the queue's first failure; null if closed.
extern "system" fn error_event(this: QueueRef) -> *mut c_void {
    use windows_sys::Win32::Foundation::{DUPLICATE_SAME_ACCESS, DuplicateHandle};
    use windows_sys::Win32::System::Threading::GetCurrentProcess;
    let Some(event) = with_lane(this, "GetErrorEvent", |lane| lane.errors.event) else {
        return std::ptr::null_mut();
    };
    let mut out = std::ptr::null_mut();
    // SAFETY: our own event, duplicated into this process.
    let ok = unsafe {
        DuplicateHandle(
            GetCurrentProcess(),
            event as *mut c_void,
            GetCurrentProcess(),
            &mut out,
            0,
            0,
            DUPLICATE_SAME_ACCESS,
        )
    };
    if ok == 0 { std::ptr::null_mut() } else { out }
}

/// The console's `DSTORAGE_ERROR_RECORD`, 0x290 bytes: the failure count,
/// then the first failure's result and command type (0: a request; -1 with
/// no failure) and, for a request, its parameters.
const ERROR_RECORD: usize = 0x290;

/// `RetrieveErrorRecord` (+0x50).
extern "system" fn retrieve_error_record(this: QueueRef, record: *mut u8) {
    if record.is_null() {
        return;
    }
    let Some((count, first)) = with_lane(this, "RetrieveErrorRecord", |lane| {
        (lane.errors.count, lane.errors.first.clone())
    }) else {
        return;
    };
    write_error_record(record, count, first.as_ref());
}

fn write_error_record(record: *mut u8, count: u32, first: Option<&(HRESULT, Request, String, String)>) {
    // SAFETY: the title passes the console's 0x290-byte record.
    let out = unsafe { std::slice::from_raw_parts_mut(record, ERROR_RECORD) };
    out.fill(0);
    let put = |out: &mut [u8], offset: usize, bytes: &[u8]| {
        out[offset..offset + bytes.len()].copy_from_slice(bytes);
    };
    put(out, 0, &count.to_le_bytes());
    let Some((hr, request, path, name)) = first.filter(|_| count != 0) else {
        put(out, 0xc, &u32::MAX.to_le_bytes());
        return;
    };
    put(out, 0x8, &hr.to_le_bytes());
    put(out, 0xc, &0u32.to_le_bytes());
    put(out, 0x10, &request.options.to_le_bytes());
    put(out, 0x18, &(request.destination as u64).to_le_bytes());
    put(out, 0x20, &request.destination_size.to_le_bytes());
    if request.options & MEMORY_SOURCE == 0 {
        // The file's path, at most 259 characters and a NUL.
        for (i, unit) in path.encode_utf16().take(259).enumerate() {
            put(out, 0x24 + 2 * i, &unit.to_le_bytes());
        }
    } else {
        put(out, 0x238, &(request.source as u64).to_le_bytes());
    }
    put(out, 0x230, &request.offset.to_le_bytes());
    put(out, 0x240, &request.source_size.to_le_bytes());
    put(out, 0x248, &request.tag.to_le_bytes());
    // The request's name, at most 63 bytes and a NUL.
    let name = name.as_bytes();
    put(out, 0x250, &name[..name.len().min(63)]);
}

/// The console's `DSTORAGE_QUEUE_INFO` (`Query`, +0x58): the description
/// (source type, capacity, priority, name), then the empty slots and the
/// requests until the queue submits itself.
#[repr(C)]
struct QueueInfo {
    source_type: u64,
    capacity: u16,
    priority: i8,
    name: *const c_char,
    empty_slots: u16,
    until_auto_submit: u16,
}

/// `DSTORAGE_QUEUE_INFO1` (`Query1`, +0x68): the same with `QueueDesc1`'s
/// options, and the count until a self-submit 0xffff for a manual queue.
#[repr(C)]
struct QueueInfo1 {
    source_type: u64,
    capacity: u16,
    priority: i8,
    name: *const c_char,
    options: u64,
    empty_slots: u16,
    until_auto_submit: u16,
}

impl Lane {
    /// As the console reckons it: slots free, one always kept back.
    fn empty_slots(&self) -> u16 {
        self.capacity.saturating_sub(self.entries() + 1) as u16
    }

    /// Commands until the queue would submit itself: half the capacity less
    /// those since the last submit (half the capacity when that is 0).
    fn until_auto_submit(&self) -> u16 {
        let half = (self.capacity / 2) as u16;
        match half.wrapping_sub(self.unsubmitted.len() as u16) {
            0 => half,
            n => n,
        }
    }

    fn priority(&self) -> i8 {
        self.class as i8 - 1
    }
}

extern "system" fn query(this: QueueRef, out: *mut QueueInfo) {
    if out.is_null() {
        return;
    }
    if let Some(info) = with_lane(this, "Query", |lane| QueueInfo {
        source_type: u64::from(lane.source_type),
        capacity: lane.capacity as u16,
        priority: lane.priority(),
        name: lane.name,
        empty_slots: lane.empty_slots(),
        until_auto_submit: lane.until_auto_submit(),
    }) {
        // SAFETY: checked non-null; the title's info.
        unsafe { out.write(info) };
    }
}

extern "system" fn query1(this: QueueRef, out: *mut QueueInfo1) {
    if out.is_null() {
        return;
    }
    // SAFETY: checked non-null; the console keeps the options' other bits
    // as the caller left them.
    let kept = unsafe { (*out).options } & !MANUAL_SUBMIT;
    let Some(info) = with_lane(this, "Query", |lane| QueueInfo1 {
        source_type: u64::from(lane.source_type),
        capacity: lane.capacity as u16,
        priority: lane.priority(),
        name: lane.name,
        options: kept | u64::from(lane.manual),
        empty_slots: lane.empty_slots(),
        until_auto_submit: if lane.manual { u16::MAX } else { lane.until_auto_submit() },
    }) else {
        return;
    };
    // SAFETY: as above.
    unsafe { out.write(info) };
}

/// Reads, inflates and/or unpacks, and writes one request.
fn execute(request: &Request) -> Result<(), HRESULT> {
    let options = request.options;
    let fail = |hr: HRESULT, why: &str| {
        tracing::warn!(
            options = format_args!("{options:#x}"),
            offset = request.offset,
            source_size = request.source_size,
            destination_size = request.destination_size,
            intermediate_size = request.intermediate_size,
            tag = request.tag,
            name = text(request.name),
            hr = format_args!("{:#010x}", hr as u32),
            "dstorage: request failed: {why}"
        );
        hr
    };
    if options & !(ZLIB | BCPACK | PAGE_DESTINATION | MEMORY_SOURCE) != 0 {
        return Err(fail(E_NOTIMPL, "unsupported options"));
    }
    if request.source.is_null() || request.destination.is_null() {
        return Err(fail(E_INVALIDARG, "no source or destination"));
    }
    if options & MEMORY_SOURCE == 0 {
        // SAFETY: a file object we created; the title keeps it until the
        // request completes.
        let file = &unsafe { &*request.source.cast::<Instance<File>>() }.state;
        if file.closed.load(Ordering::Acquire) {
            return Err(fail(FILE_NOT_OPEN, "its file was closed"));
        }
    }
    let bc = ((options & BCPACK) >> 1) as u8;
    let size = request.destination_size as usize;
    if options & (ZLIB | BCPACK) == 0 {
        if request.source_size > request.destination_size {
            return Err(fail(E_INVALIDARG, "source larger than destination"));
        }
        let size = request.source_size as usize;
        return into_destination(request, size, |out| read_into(request, out).map(|()| size))
            .map_err(|hr| fail(hr, "copying"));
    }
    let tables = if bc != 0 { Some(bcpack_tables()?) } else { None };
    let source = read_source(request).map_err(|hr| fail(hr, "reading the source"))?;
    // The console's order: zlib first, then BCPack.
    let source = if bc != 0 && options & ZLIB != 0 {
        let mut packed = vec![0; request.intermediate_size as usize];
        let n = inflate(&source, &mut packed).map_err(|hr| fail(hr, "zlib"))?;
        packed.truncate(n);
        Cow::Owned(packed)
    } else {
        source
    };
    into_destination(request, size, |out| match tables {
        Some(tables) => bcpack::decode(bc, &source, out, tables)
            .map(|()| out.len())
            .map_err(|e| fail(INVALID_DATA, &e.to_string())),
        None => inflate(&source, out).map_err(|hr| fail(hr, "zlib")),
    })?;
    tracing::trace!(
        options = format_args!("{options:#x}"),
        offset = request.offset,
        size,
        "dstorage: request done"
    );
    Ok(())
}

/// Inflates a zlib stream into `out`, returning the bytes written. A stream
/// that ends short of `out` passes: the title's chunks fill it exactly, and
/// nothing says that is required.
fn inflate(source: &[u8], out: &mut [u8]) -> Result<usize, HRESULT> {
    victory_core::fast_local! {
        static INFLATE: std::cell::RefCell<zlib_rs::Inflate> =
            std::cell::RefCell::new(zlib_rs::Inflate::new(true, 15));
    }
    INFLATE.with_borrow_mut(|state| {
        state.reset(true);
        // `Inflate::decompress` cannot distinguish a valid empty stream from
        // an insufficient buffer when `avail_out == 0`, so use a 1-byte scratch
        // destination and verify `written <= out_len` afterward.
        let out_len = out.len();
        let mut scratch = [0u8; 1];
        let dest = if out_len == 0 { &mut scratch[..] } else { out };
        let (mut in_pos, mut out_pos) = (0usize, 0usize);
        let status = loop {
            match state.decompress(
                &source[in_pos..],
                &mut dest[out_pos..],
                zlib_rs::InflateFlush::NoFlush,
            ) {
                Ok(zlib_rs::Status::StreamEnd) => break Ok(zlib_rs::Status::StreamEnd),
                Ok(zlib_rs::Status::Ok) => {
                    let next_in = state.total_in() as usize;
                    let next_out = state.total_out() as usize;
                    if next_in == in_pos && next_out == out_pos {
                        break Ok(zlib_rs::Status::BufError);
                    }
                    in_pos = next_in;
                    out_pos = next_out;
                }
                other => break other,
            }
        };
        let written = state.total_out() as usize;
        match status {
            Ok(zlib_rs::Status::StreamEnd) if written <= out_len => Ok(written),
            other => {
                tracing::debug!(?other, "dstorage: zlib stream");
                Err(INVALID_DATA)
            }
        }
    })
}

/// The BCPack format's constant tables, parsed once for the process. If
/// they do not parse every BCPack request fails: one warning, then debug.
fn bcpack_tables() -> Result<&'static Tables, HRESULT> {
    static TABLES: OnceLock<Result<Tables, String>> = OnceLock::new();
    static WARNED: AtomicBool = AtomicBool::new(false);
    match TABLES.get_or_init(load_bcpack_tables) {
        Ok(tables) => Ok(tables),
        Err(why) if !WARNED.swap(true, Ordering::Relaxed) => {
            tracing::warn!("dstorage: BCPack requests fail: {why} (more at debug)");
            Err(E_NOTIMPL)
        }
        Err(_) => {
            tracing::debug!("dstorage: BCPack request failed: no tables");
            Err(E_NOTIMPL)
        }
    }
}

/// The tables VIctory ships with.
#[cfg(not(test))]
fn load_bcpack_tables() -> Result<Tables, String> {
    Tables::builtin().map_err(|e| format!("the built-in BCPack tables: {e}"))
}

/// Tests decode streams made against tables of their own, from this folder.
#[cfg(test)]
static BCPACK_DIR: OnceLock<std::path::PathBuf> = OnceLock::new();

#[cfg(test)]
fn use_bcpack_tables(dir: std::path::PathBuf) {
    let _ = BCPACK_DIR.set(dir);
}

#[cfg(test)]
fn load_bcpack_tables() -> Result<Tables, String> {
    let dir = BCPACK_DIR.get().ok_or("no folder set for the test tables")?;
    let bytes = std::fs::read(dir.join("tables.bin")).map_err(|e| e.to_string())?;
    Tables::parse(&bytes).map_err(|e| e.to_string())
}

/// A compressed request's source: borrowed from memory, or read from the
/// file.
fn read_source(request: &Request) -> Result<Cow<'static, [u8]>, HRESULT> {
    let size = request.source_size as usize;
    if request.options & MEMORY_SOURCE != 0 {
        // SAFETY: the title's source holds `size` bytes until completion.
        let bytes = unsafe { std::slice::from_raw_parts(request.source.cast(), size) };
        return Ok(Cow::Borrowed(bytes));
    }
    let mut bytes = vec![0; size];
    read_into(request, &mut bytes)?;
    Ok(Cow::Owned(bytes))
}

/// Nanoseconds workers spent reading files, for the debug summary.
static READING: AtomicU64 = AtomicU64::new(0);

/// How often a read the host refuses is tried again (about 0.25 s in all).
const READ_RETRIES: u32 = 8;

/// Fills `out` (the source size) from the request's file or memory.
fn read_into(request: &Request, out: &mut [u8]) -> Result<(), HRESULT> {
    if request.options & MEMORY_SOURCE != 0 {
        // SAFETY: the title's source holds `out.len()` bytes until completion.
        out.copy_from_slice(unsafe { std::slice::from_raw_parts(request.source.cast(), out.len()) });
        return Ok(());
    }
    // SAFETY: a file object we created; the title keeps it open.
    let files = &unsafe { &*request.source.cast::<Instance<File>>() }
        .state
        .files;
    let started = std::time::Instant::now();
    let (mut done, mut tries) = (0, 0);
    let result = loop {
        if done == out.len() {
            break Ok(());
        }
        let error = match files.read_at(&mut out[done..], request.offset + done as u64) {
            Ok(0) => break Err(END_OF_FILE),
            Ok(n) => {
                done += n;
                continue;
            }
            Err(error) => error,
        };
        // Title memory the GPU side guards again in the moment between our
        // opening it and the kernel's write fails the read (and a busy host
        // can refuse one): opened again and tried again, since the title
        // never learns of a failed read.
        tracing::warn!(
            %error,
            offset = request.offset,
            size = out.len(),
            done,
            tries,
            "dstorage: a read of the file failed"
        );
        if tries == READ_RETRIES {
            break Err(from_win32(&error));
        }
        std::thread::sleep(std::time::Duration::from_millis(1 << tries));
        tries += 1;
        victory_gpu::kernel_will_write(out.as_ptr() as usize + done, out.len() - done);
    };
    READING.fetch_add(started.elapsed().as_nanos() as u64, Ordering::Relaxed);
    result
}

/// Runs `fill` on the destination's first `len` bytes; `fill` returns how
/// many it produced. GPU-visible destinations are host memory like the
/// rest (GPU address = CPU address), so an address is written in place;
/// physical pages are too when they are consecutive pages of one
/// allocation, and otherwise go through a buffer.
fn into_destination(
    request: &Request,
    len: usize,
    mut fill: impl FnMut(&mut [u8]) -> Result<usize, HRESULT>,
) -> Result<(), HRESULT> {
    if request.options & PAGE_DESTINATION == 0 {
        // Raw reads land through the kernel, which does not fault into the
        // GPU side's guard on title buffers; open them first, and until the
        // bytes are there.
        let writing = victory_gpu::kernel_writing(request.destination as usize, len);
        // SAFETY: the title's destination holds `destination_size` bytes, at
        // least `len`, and nothing else writes it until completion.
        let out = unsafe { std::slice::from_raw_parts_mut(request.destination.cast::<u8>(), len) };
        let filled = fill(out).map(drop);
        drop(writing);
        // Again now that the bytes are there: a texture looked at in between
        // was uploaded without them (`xmem::pages_written`).
        if super::xmem::stream_notice() {
            victory_gpu::kernel_will_write(request.destination as usize, len);
        } else {
            victory_gpu::title_memory_written(request.destination as usize, len, false);
        }
        return filled;
    }
    let offset = (request.page_offset & 0xffff) as usize;
    let list = request.destination.cast();
    let mut pages = page_list(list, offset, len);
    // The title sees its destination through the pages its list names: if
    // it names others by the time the data is written, they get it as well.
    // (Said aloud: by the title's protocol a submitted request's list stays
    // as it is.)
    for _ in 0..3 {
        into_pages(&pages, offset, len, &mut fill)?;
        let now = read_list(list, offset, len);
        if now == pages || !super::xmem::pages_known(&now, offset, len) {
            break;
        }
        tracing::warn!(
            list = format_args!("{list:p}"),
            offset,
            len,
            first = pages.first(),
            first_now = now.first(),
            "dstorage: a destination's page list changed during the write; written to its new pages too"
        );
        pages = now;
    }
    Ok(())
}

/// Runs `fill` on `len` bytes of `pages`, from `offset` into the first.
fn into_pages(
    pages: &[usize],
    offset: usize,
    len: usize,
    mut fill: impl FnMut(&mut [u8]) -> Result<usize, HRESULT>,
) -> Result<(), HRESULT> {
    // Consecutive pages of one allocation (the usual case): written in
    // place through one view.
    if let Some(mut window) = super::xmem::page_window(pages, offset, len) {
        let filled = fill(window.bytes()).map(drop);
        super::xmem::pages_written(pages, offset, len);
        return filled;
    }
    let mut data = vec![0; len];
    let n = fill(&mut data)?;
    // A page the title has just made, or a view the host refuses for a moment (commit
    // or address space short while a load maps many at once), can come right a little
    // later: tried again before the request fails (`VICTORY_DSTORAGE_RETRIES`, default 12,
    // about 1.5 s in all).
    static RETRIES: std::sync::OnceLock<u32> = std::sync::OnceLock::new();
    let retries = *RETRIES.get_or_init(|| {
        std::env::var("VICTORY_DSTORAGE_RETRIES").ok().and_then(|v| v.parse().ok()).unwrap_or(12)
    });
    let mut written = super::xmem::write_physical(pages, offset, &data[..n]);
    let mut tries = 0;
    while !written && tries < retries {
        std::thread::sleep(std::time::Duration::from_millis((2u64 << tries.min(6)).min(128)));
        tries += 1;
        written = super::xmem::write_physical(pages, offset, &data[..n]);
    }
    if written {
        super::xmem::pages_written(pages, offset, n);
        if tries > 0 {
            tracing::info!(offset, len, tries, "dstorage: a destination's physical pages written after a retry");
        }
        Ok(())
    } else {
        let known = super::xmem::pages_known(pages, offset, len);
        tracing::warn!(known, last_error = std::io::Error::last_os_error().raw_os_error(), "dstorage: retries used up");
        // Said aloud: the title never reads a request's result
        // (it only polls for completion), so a destination left unwritten
        // is a model or texture that silently holds whatever its pages
        // held before.
        tracing::warn!(
            offset,
            len,
            pages = pages.len(),
            "dstorage: a destination's physical pages could not be written (unknown page or a host failure); its request fails"
        );
        Err(E_INVALIDARG)
    }
}

/// The page numbers `list` holds now, one per 64 KiB of destination.
fn read_list(list: *const usize, offset: usize, len: usize) -> Vec<usize> {
    (0..(offset + len).div_ceil(PAGE))
        // SAFETY: the title passes a page number per 64 KiB of
        // destination, which holds at least `len` bytes from `offset`;
        // it may be writing them.
        .map(|i| unsafe { list.add(i).read_volatile() })
        .collect()
}

/// The request's page numbers. A list is the part of the title's own page
/// table (one number per 64 KiB of its address range) that describes the
/// destination, filled in when the title submits the batch, so it is valid
/// here. It is only ever overwritten: an address the title unmapped keeps
/// the number of the page it had, which the title maps elsewhere soon after,
/// so a number the title owns says nothing about whether it is the right
/// one ([`Lane::manual`]). Page 0 does (an address never mapped yet): a list
/// with a page the title does not own is read again for a while, up to
/// 1.5 s; what it holds then is used as it is.
fn page_list(list: *const usize, offset: usize, len: usize) -> Vec<usize> {
    let mut pages = read_list(list, offset, len);
    if super::xmem::pages_known(&pages, offset, len) {
        return pages;
    }
    // Timed from here: what took long before was the lock on the page table
    // (which the title's mapping calls hold), not the list.
    let started = std::time::Instant::now();
    let mut known = false;
    while !known && started.elapsed().as_millis() < 1500 {
        std::thread::sleep(std::time::Duration::from_millis(1));
        pages = read_list(list, offset, len);
        known = super::xmem::pages_known(&pages, offset, len);
    }
    tracing::warn!(
        ms = started.elapsed().as_millis() as u64,
        known,
        "dstorage: a destination's page list was not filled in yet"
    );
    pages
}

/// `XCRDAPI.dll`'s queue slots (the title calls 0x18, 0x20, 0x30 and
/// 0x40); 0x70-0x80 are the console's own (destructor, start, manual flag).
static QUEUE_VTABLE: Methods<17> = Methods([
    query_queue as *const (),
    add_ref_instance::<Queue> as *const (),
    release::<Queue> as *const (),
    enqueue_request as *const (),       // 0x18
    enqueue_status as *const (),        // 0x20
    enqueue_signal as *const (),        // 0x28
    submit as *const (),                // 0x30
    cancel_with_tag as *const (),       // 0x38
    close as *const (),                 // 0x40
    error_event as *const (),           // 0x48
    retrieve_error_record as *const (), // 0x50
    query as *const (),                 // 0x58
    enqueue_set_event as *const (),     // 0x60
    query1 as *const (),                // 0x68
    missing::<14> as *const (),
    missing::<15> as *const (),
    missing::<16> as *const (),
]);

pub(crate) fn module() -> HleModule {
    HleModule {
        name: "dstorage",
        dlls: &["xcrdapi.dll"],
        exports: [vec![hle_export!(DStorageGetFactory)], super::xcrd::exports()].concat(),
    }
}

#[cfg(test)]
mod tests {
    use std::os::windows::ffi::OsStrExt;
    use std::time::{Duration, Instant};

    use super::*;
    use crate::modules::xgameruntime::method;

    fn call<F: Copy>(object: *mut c_void, slot: usize) -> F {
        // SAFETY: every object here starts with the runtime header.
        method(unsafe { &*object.cast::<Object>() }, slot)
    }

    fn factory() -> *mut c_void {
        let mut factory = std::ptr::null_mut();
        assert_eq!(DStorageGetFactory(&FACTORY, &mut factory), S_OK);
        factory
    }

    fn open(path: &std::path::Path) -> *mut c_void {
        let wide: Vec<u16> = path.as_os_str().encode_wide().chain([0]).collect();
        let open: extern "system" fn(*mut c_void, *const u16, *const Guid, Out) -> HRESULT =
            call(factory(), 0x20);
        let mut file = std::ptr::null_mut();
        assert_eq!(
            open(factory(), wide.as_ptr(), std::ptr::null(), &mut file),
            S_OK
        );
        file
    }

    fn status_array(capacity: u32) -> *mut c_void {
        let create: extern "system" fn(
            *mut c_void,
            u32,
            *const c_char,
            *const Guid,
            Out,
        ) -> HRESULT = call(factory(), 0x28);
        let mut array = std::ptr::null_mut();
        let name = c"test".as_ptr();
        assert_eq!(
            create(factory(), capacity, name, std::ptr::null(), &mut array),
            S_OK
        );
        array
    }

    fn new_queue(source_type: u64) -> *mut c_void {
        let create: extern "system" fn(*mut c_void, *const QueueDesc, *const Guid, Out) -> HRESULT =
            call(factory(), 0x50);
        let desc = QueueDesc {
            source_type,
            capacity: 0x80,
            priority: 0,
            name: c"test".as_ptr(),
            options: 1,
        };
        let mut queue = std::ptr::null_mut();
        assert_eq!(create(factory(), &desc, std::ptr::null(), &mut queue), S_OK);
        queue
    }

    fn request(
        options: u64,
        destination: *mut c_void,
        destination_size: usize,
        source: *const c_void,
        offset: u64,
        source_size: usize,
    ) -> Request {
        Request {
            options,
            destination,
            page_offset: 0,
            destination_size: destination_size as u32,
            source,
            offset,
            source_size: source_size as u32,
            intermediate_size: 0,
            tag: 0,
            name: std::ptr::null(),
        }
    }

    fn enqueue(queue: *mut c_void, request: &Request) {
        let f: extern "system" fn(*mut c_void, *const Request) = call(queue, 0x18);
        f(queue, request);
    }

    fn enqueue_status(queue: *mut c_void, array: *mut c_void, index: u32) {
        let f: extern "system" fn(*mut c_void, *mut c_void, u32) = call(queue, 0x20);
        f(queue, array, index);
    }

    fn void(object: *mut c_void, slot: usize) {
        let f: extern "system" fn(*mut c_void) = call(object, slot);
        f(object);
    }

    fn result(array: *mut c_void, index: u32) -> HRESULT {
        let f: extern "system" fn(*mut c_void, u32) -> HRESULT = call(array, 0x20);
        f(array, index)
    }

    /// Polls as the title does, then returns the entry's result.
    fn wait(array: *mut c_void, index: u32) -> HRESULT {
        let done: extern "system" fn(*mut c_void, u32) -> bool = call(array, 0x18);
        let start = Instant::now();
        while !done(array, index) {
            assert!(start.elapsed() < Duration::from_secs(10), "never completed");
            std::thread::sleep(Duration::from_millis(1));
        }
        result(array, index)
    }

    fn file_with(bytes: &[u8]) -> (tempfile::TempDir, *mut c_void) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("data.bin");
        std::fs::write(&path, bytes).unwrap();
        let file = open(&path);
        (dir, file)
    }

    fn pattern(n: usize) -> Vec<u8> {
        (0..n).map(|i| (i * 7 + i / 251) as u8).collect()
    }

    #[test]
    fn request_layout_is_the_titles() {
        assert_eq!(size_of::<Request>(), 0x48);
        assert_eq!(std::mem::offset_of!(Request, destination_size), 0x18);
        assert_eq!(std::mem::offset_of!(Request, source), 0x20);
        assert_eq!(std::mem::offset_of!(Request, source_size), 0x30);
        assert_eq!(std::mem::offset_of!(Request, name), 0x40);
        assert_eq!(std::mem::offset_of!(QueueDesc, options), 0x18);
    }

    fn compress_zlib(plain: &[u8]) -> Vec<u8> {
        let mut buf = vec![0u8; zlib_rs::compress_bound(plain.len())];
        let (packed, rc) =
            zlib_rs::compress_slice(&mut buf, plain, zlib_rs::DeflateConfig::new(6));
        assert_eq!(rc, zlib_rs::ReturnCode::Ok);
        packed.to_vec()
    }

    #[test]
    fn raw_and_zlib_reads_from_a_file() {
        let plain = pattern(300_000);
        let packed = compress_zlib(&plain);
        let mut contents = plain.clone();
        contents.extend_from_slice(&packed);
        let (_dir, file) = file_with(&contents);
        let (queue, array) = (new_queue(0), status_array(1));

        let mut raw = vec![0u8; 1000];
        let mut inflated = vec![0u8; plain.len()];
        let at = |b: &mut Vec<u8>| b.as_mut_ptr().cast();
        enqueue(queue, &request(0, at(&mut raw), 1000, file, 5000, 1000));
        let offset = plain.len() as u64;
        let zlib = request(
            ZLIB,
            at(&mut inflated),
            plain.len(),
            file,
            offset,
            packed.len(),
        );
        enqueue(queue, &zlib);
        enqueue_status(queue, array, 0);
        assert_eq!(result(array, 0), E_PENDING, "pending until done");
        void(queue, 0x30);
        assert_eq!(wait(array, 0), S_OK);
        assert_eq!(raw, plain[5000..6000]);
        assert_eq!(inflated, plain);
        void(queue, 0x40);
    }

    #[test]
    fn memory_sources_inflate_too() {
        let plain = pattern(70_000);
        let packed = compress_zlib(&plain);
        let (queue, array) = (new_queue(1), status_array(1));
        let mut out = vec![0u8; plain.len()];
        let options = ZLIB | MEMORY_SOURCE;
        let r = request(
            options,
            out.as_mut_ptr().cast(),
            out.len(),
            packed.as_ptr().cast(),
            0,
            packed.len(),
        );
        enqueue(queue, &r);
        enqueue_status(queue, array, 0);
        void(queue, 0x30);
        assert_eq!(wait(array, 0), S_OK);
        assert_eq!(out, plain);
    }

    #[test]
    fn statuses_complete_in_order_with_the_first_failure() {
        let contents = pattern(4096);
        let (_dir, file) = file_with(&contents);
        let (queue, array) = (new_queue(0), status_array(3));
        // Requests in flight together run in any order, so none of them
        // shares a destination (failed ones leave theirs undefined).
        let mut buffers = [[0u8; 16]; 5];
        let [failed, failed_too, out, other, bcpack] = buffers.each_mut().map(|b| b.as_mut_ptr().cast());

        // Segment 0: a read past the end, then a bad stream; the first wins.
        enqueue(queue, &request(0, failed, 16, file, 4090, 16));
        enqueue(queue, &request(ZLIB, failed_too, 16, file, 0, 16));
        enqueue_status(queue, array, 0);
        // Segment 1: two reads.
        enqueue(queue, &request(0, other, 16, file, 0, 16));
        enqueue(queue, &request(0, out, 16, file, 100, 16));
        enqueue_status(queue, array, 1);
        void(queue, 0x30);
        // Segment 2: a BCPack request (BC7) whose stream is empty fails.
        with_test_tables();
        enqueue(queue, &request(7 << 1, bcpack, 16, file, 0, 0));
        enqueue_status(queue, array, 2);
        void(queue, 0x30);

        assert_eq!(wait(array, 2), INVALID_DATA);
        assert_eq!(result(array, 0), END_OF_FILE);
        assert_eq!(result(array, 1), S_OK);
        assert_eq!(buffers[2], contents[100..116]);
        assert_eq!(buffers[3], contents[..16]);
    }

    #[test]
    fn half_a_queue_waits_for_the_submit() {
        use crate::modules::xmem;
        // Two pages the title owns: the one its list still names from
        // earlier, and the one it maps for this batch when it submits.
        let mut pages = [0usize; 2];
        let mut n = 2;
        assert_eq!(xmem::XMemAllocatePhysicalPages(0, &mut n, pages.as_mut_ptr()), 1);
        let [stale, right] = pages;
        let plain = pattern(64);
        let mut list = [stale];
        let (queue, array) = (new_queue(1), status_array(1));
        // More than half of the queue's 128.
        for i in 0..100 {
            let mut r = request(
                PAGE_DESTINATION | MEMORY_SOURCE,
                list.as_mut_ptr().cast(),
                plain.len(),
                plain.as_ptr().cast(),
                0,
                plain.len(),
            );
            r.page_offset = i * 64;
            enqueue(queue, &r);
        }
        std::thread::sleep(Duration::from_millis(50));
        // SAFETY: our own list, which a worker may be reading.
        unsafe { std::ptr::write_volatile(list.as_mut_ptr(), right) };
        enqueue_status(queue, array, 0);
        void(queue, 0x30);
        assert_eq!(wait(array, 0), S_OK);
        let bytes = |page: usize| -> Vec<u8> {
            xmem::page_window(&[page], 0, 6400).unwrap().bytes().to_vec()
        };
        assert!(bytes(stale) == [0; 6400], "written to the page the list named before");
        assert!(bytes(right) == plain.repeat(100), "not written to the page the list names");
    }

    #[test]
    fn a_full_queue_submits_itself() {
        let (_dir, file) = file_with(&pattern(64));
        let create: extern "system" fn(*mut c_void, *const QueueDesc, *const Guid, Out) -> HRESULT =
            call(factory(), 0x50);
        let desc = QueueDesc {
            source_type: 0,
            capacity: 4,
            priority: 1,
            name: c"small".as_ptr(),
            options: 1,
        };
        let mut queue = std::ptr::null_mut();
        assert_eq!(create(factory(), &desc, std::ptr::null(), &mut queue), S_OK);
        let array = status_array(1);
        let mut outs = [[0u8; 16]; 4];
        for out in &mut outs[..2] {
            enqueue(queue, &request(0, out.as_mut_ptr().cast(), 16, file, 8, 16));
        }
        enqueue_status(queue, array, 0);
        std::thread::sleep(Duration::from_millis(20));
        assert_eq!(result(array, 0), E_PENDING, "three of four: not submitted");
        // One more than fits: would wait for good.
        for out in &mut outs[2..] {
            enqueue(queue, &request(0, out.as_mut_ptr().cast(), 16, file, 8, 16));
        }
        assert_eq!(wait(array, 0), S_OK);
        void(queue, 0x40);
        assert!(outs[..2].iter().all(|out| out[..] == pattern(64)[8..24]));
    }

    #[test]
    fn a_list_that_changes_during_the_write_has_its_new_pages_written() {
        use crate::modules::xmem;
        let mut pages = [0usize; 4];
        let mut n = 4;
        assert_eq!(xmem::XMemAllocatePhysicalPages(0, &mut n, pages.as_mut_ptr()), 1);
        // In place (consecutive pages) and through a buffer.
        for old in [[pages[0], pages[1]], [pages[1], pages[0]]] {
            let new = [pages[3], pages[2]];
            let mut list = old;
            let at = list.as_mut_ptr();
            let len = PAGE + 100;
            let r = request(PAGE_DESTINATION, at.cast(), len, std::ptr::null(), 0, len);
            let mut runs = 0;
            let written = into_destination(&r, len, |out| {
                runs += 1;
                out.fill(0xa5);
                // SAFETY: our own list.
                unsafe { at.cast::<[usize; 2]>().write_volatile(new) };
                Ok(out.len())
            });
            assert_eq!((written, runs), (Ok(()), 2));
            assert!(xmem::page_window(&new[..1], 0, PAGE).unwrap().bytes().iter().all(|&b| b == 0xa5));
            assert!(xmem::page_window(&new[1..], 0, 100).unwrap().bytes().iter().all(|&b| b == 0xa5));
            for page in new {
                xmem::page_window(&[page], 0, PAGE).unwrap().bytes().fill(0);
            }
        }
    }

    #[test]
    fn a_read_into_memory_guarded_for_a_moment_is_tried_again() {
        use windows_sys::Win32::System::Memory::{
            MEM_COMMIT, MEM_RESERVE, PAGE_READWRITE, PAGE_READONLY, VirtualAlloc, VirtualProtect,
        };
        let plain = pattern(3 * 4096);
        let (_dir, file) = file_with(&plain);
        // SAFETY: a fresh allocation; its middle page is read-only until the
        // thread opens it, as a guard of the GPU side's would be.
        let at = unsafe {
            VirtualAlloc(std::ptr::null(), plain.len(), MEM_RESERVE | MEM_COMMIT, PAGE_READWRITE)
        } as usize;
        assert_ne!(at, 0);
        let protect = move |to| {
            let mut old = 0;
            // SAFETY: as above.
            unsafe { VirtualProtect((at + 4096) as *const _, 4096, to, &mut old) }
        };
        assert_ne!(protect(PAGE_READONLY), 0);
        let opener = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(20));
            protect(PAGE_READWRITE)
        });
        let r = request(0, at as *mut c_void, plain.len(), file, 0, plain.len());
        assert_eq!(execute(&r), Ok(()));
        assert_ne!(opener.join().unwrap(), 0);
        // SAFETY: as above, all of it readable.
        assert_eq!(unsafe { std::slice::from_raw_parts(at as *const u8, plain.len()) }, plain);
    }

    /// What ways of reading a file give, at `VICTORY_BENCH_FILE` (a file of
    /// the title's, only read): 24 threads, requests of 48 KiB at random,
    /// first from places nobody read lately, then the same once more.
    /// `VICTORY_BENCH_FILE=... cargo test --release -p victory-hle --lib
    /// read_bench -- --ignored --nocapture`
    #[test]
    #[ignore = "a benchmark"]
    fn read_bench() {
        const THREADS: usize = 24;
        const SIZE: usize = 48 << 10;
        const READS: usize = 12_000;
        let path = std::env::var("VICTORY_BENCH_FILE").expect("VICTORY_BENCH_FILE");
        let len = std::fs::metadata(&path).unwrap().len();
        let mut seed = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos() as u64 | 1;
        let mut random = move || {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            seed
        };
        for round in 0..2 {
            for mode in [Io::One, Io::Handles, Io::Overlapped, Io::Direct] {
                let file = Files::open_for_test(&path, mode);
                let at: Vec<u64> = (0..READS)
                    .map(|_| (random() % (len - SIZE as u64)) & !15)
                    .collect();
                for pass in ["cold", "again"] {
                    let next = std::sync::atomic::AtomicUsize::new(0);
                    let busy = AtomicU64::new(0);
                    let start = Instant::now();
                    std::thread::scope(|s| {
                        for _ in 0..THREADS {
                            s.spawn(|| {
                                let mut out = vec![0u8; SIZE];
                                while let Some(&at) = at.get(next.fetch_add(1, Ordering::Relaxed)) {
                                    let began = Instant::now();
                                    let mut done = 0;
                                    while done < SIZE {
                                        done += file.read_at(&mut out[done..], at + done as u64).unwrap();
                                    }
                                    busy.fetch_add(began.elapsed().as_micros() as u64, Ordering::Relaxed);
                                }
                            });
                        }
                    });
                    let took = start.elapsed().as_secs_f64();
                    println!(
                        "{round} {mode:?} {pass}: {:.0} MB/s, {:.2} ms a read",
                        (READS * SIZE) as f64 / took / 1e6,
                        busy.load(Ordering::Relaxed) as f64 / READS as f64 / 1e3
                    );
                }
            }
        }
    }

    /// Every way of reading a file gives its bytes, also past its end.
    #[test]
    fn every_way_of_reading_gives_the_same_bytes() {
        let plain = pattern(300_001);
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("data.bin");
        std::fs::write(&path, &plain).unwrap();
        for mode in [Io::One, Io::Handles, Io::Overlapped, Io::Direct] {
            let file = Files::open_for_test(path.to_str().unwrap(), mode);
            for (at, len) in [(0, 4096), (1, 100), (4095, 2), (8192, 200_000), (12_345, 250_000), (299_990, 11), (300_000, 1)] {
                let mut out = vec![0u8; len];
                let mut done = 0;
                while done < len {
                    let n = file.read_at(&mut out[done..], (at + done) as u64).unwrap();
                    assert_ne!(n, 0, "{mode:?} {at} {len}");
                    done += n;
                }
                assert!(out == plain[at..at + len], "{mode:?} {at} {len}");
            }
            let mut out = [0u8; 16];
            assert_eq!(file.read_at(&mut out, 299_999).unwrap(), 2, "{mode:?}");
            assert_eq!(file.read_at(&mut out, 300_001).unwrap(), 0, "{mode:?}");
            assert_eq!(file.read_at(&mut out, 400_000).unwrap(), 0, "{mode:?}");
        }
    }

    /// Lanes without workers: what `start` picks, and when statuses land.
    fn lanes(staging: u64) -> Lanes {
        Lanes {
            lanes: HashMap::new(),
            next_id: 0,
            next_seq: 0,
            staged: 0,
            staging,
            starved: false,
            clock: [0; CLASSES],
            now: 0,
            lane_now: [0; CLASSES],
            done: Default::default(),
            refused: 0,
            bcpack: Default::default(),
            since: None,
        }
    }

    fn read(lanes: &mut Lanes, id: u64, options: u64, size: u32) {
        let mut r = request(options, std::ptr::null_mut(), 0, std::ptr::null(), 0, 0);
        (r.destination_size, r.source_size) = (size, size);
        let lane = lanes.lanes.get_mut(&id).unwrap();
        let note = Note::default();
        lane.unsubmitted.push(Command::Read(Read { request: r, note, cancelled: false }));
    }

    #[test]
    fn priorities_share_by_bytes_about_tenfold() {
        let mut l = lanes(STAGING);
        l.lanes.insert(0, Lane::new(0, 4096, true));
        l.lanes.insert(1, Lane::new(1, 4096, true));
        l.lanes.insert(2, Lane::new(2, 4096, true)); // real-time: a class of its own
        for _ in 0..400 {
            for id in 0..3 {
                read(&mut l, id, 0, 4096);
            }
        }
        (0..3).for_each(|id| l.submit(id, false));
        let mut started = [0u32; 3];
        for _ in 0..333 {
            let job = l.start().unwrap();
            started[job.lane as usize] += 1;
            l.finish(&job, Ok(()));
        }
        // Each level gets about ten times the one below.
        assert!(started[2].abs_diff(300) <= 2, "{started:?}");
        assert!(started[1].abs_diff(30) <= 2, "{started:?}");
        assert!(started[0].abs_diff(3) <= 1, "{started:?}");
    }

    #[test]
    fn staging_bounds_compressed_bytes_in_flight() {
        let mut l = lanes(100);
        l.lanes.insert(0, Lane::new(0, 64, true));
        read(&mut l, 0, ZLIB, 60);
        read(&mut l, 0, ZLIB, 60);
        read(&mut l, 0, 0, 1000); // raw: straight to its destination
        l.submit(0, false);
        let first = l.start().unwrap();
        assert!(l.start().is_none(), "60 + 60 > 100");
        l.finish(&first, Ok(()));
        let second = l.start().unwrap();
        let raw = l.start().unwrap();
        assert_eq!((second.staging, raw.staging), (60, 0));
        // Alone, a request larger than the buffer still runs.
        read(&mut l, 0, ZLIB, 500);
        l.submit(0, false);
        assert!(l.start().is_none());
        l.finish(&second, Ok(()));
        assert!(l.start().is_some());
    }

    #[test]
    fn a_status_waits_for_every_earlier_read() {
        let mut l = lanes(STAGING);
        l.lanes.insert(0, Lane::new(0, 64, true));
        let array = status_array(2).cast::<Instance<StatusArray>>();
        let status = |index| {
            add_ref_instance(array);
            Command::Status(Status { array, index, enqueued: 0 })
        };
        read(&mut l, 0, 0, 16);
        read(&mut l, 0, 0, 16);
        l.lanes.get_mut(&0).unwrap().unsubmitted.push(status(0));
        read(&mut l, 0, 0, 16);
        l.lanes.get_mut(&0).unwrap().unsubmitted.push(status(1));
        // SAFETY: an array we created.
        for entry in unsafe { &(*array).state.entries }.iter() {
            entry.store(E_PENDING, Ordering::Release);
        }
        l.submit(0, false);
        let jobs: Vec<Job> = (0..3).map(|_| l.start().unwrap()).collect();
        // The last read and the first finish; the second then fails.
        l.finish(&jobs[2], Ok(()));
        l.finish(&jobs[0], Ok(()));
        let results = || (result(array.cast(), 0), result(array.cast(), 1));
        assert_eq!(results(), (E_PENDING, E_PENDING));
        l.finish(&jobs[1], Err(INVALID_DATA));
        assert_eq!(results(), (INVALID_DATA, S_OK));
    }

    #[test]
    fn zlib_then_bcpack() {
        with_test_tables();
        let packed = compress_zlib(&BC4_STREAM);
        let (_dir, file) = file_with(&packed);
        let (queue, array) = (new_queue(0), status_array(1));
        let mut out = [0u8; 16];
        let options = ZLIB | 4 << 1;
        let mut r = request(options, out.as_mut_ptr().cast(), 16, file, 0, packed.len());
        r.intermediate_size = BC4_STREAM.len() as u32;
        enqueue(queue, &r);
        enqueue_status(queue, array, 0);
        void(queue, 0x30);
        assert_eq!(wait(array, 0), S_OK);
        assert_eq!(out, bc4_blocks());
    }

    /// BCPack tables we made (uniform models; BC4 index pairs: low 3 bits,
    /// next 3 bits), in a folder kept for the test process.
    fn with_test_tables() {
        static DIR: OnceLock<()> = OnceLock::new();
        DIR.get_or_init(|| {
            let mut t = Vec::new();
            for m in 0..128 {
                let g = m / 16;
                for b in 0..9 {
                    let f: u16 = if b < g + 2 { 1 << (15 - g) } else { 0 };
                    t.extend_from_slice(&f.to_le_bytes());
                }
            }
            t.extend([0; 256 + 64 + 256]);
            t.extend((0..64u8).map(|v| (v & 7) | (v >> 3) << 4));
            t.resize(bcpack::TABLES_SIZE, 0);
            let dir = tempfile::tempdir().unwrap().keep();
            std::fs::write(dir.join("tables.bin"), t).unwrap();
            use_bcpack_tables(dir);
        });
    }

    /// Two BC4 blocks from the format's test encoder: endpoints 10, -3 with
    /// indices p -> p & 7, then the same block plus 1 everywhere.
    const BC4_STREAM: [u8; 88] = [
        0, 0, 0, 0, 0, 0, 0, 0, 5, 5, 5, 5, 4, 4, 4, 4, 1, 1, 1, 1, 0x10, 0x10, 0x10, 0x10, 2, 1,
        0, 0, 0, 0, 0, 0, 0, 0, 0x14, 5, 0x20, 0x68, 0xb0, 0xf8, 3, 3, 2, 2, 0x24, 0x24, 0x24,
        0x24, 2, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 4, 4, 4, 4, 0x10, 0x10, 0x10, 0x10, 0, 0, 0, 0,
        0x20, 0x68, 0xb0, 0xf8, 0x24, 0x24, 0x24, 0x24, 0, 0, 0, 0, 0, 0, 0, 0,
    ];

    /// What `BC4_STREAM` unpacks to.
    fn bc4_blocks() -> [u8; 16] {
        let code = [0u64, 2, 3, 4, 5, 6, 7, 1];
        let block = |e0: u64, e1: u64, add: usize| {
            let bits = (0..16).fold(0, |a, p| a | code[(p + add) & 7] << (16 + 3 * p));
            (e0 | e1 << 8 | bits).to_le_bytes()
        };
        let mut out = [0; 16];
        out[..8].copy_from_slice(&block(138, 125, 0));
        out[8..].copy_from_slice(&block(139, 126, 1));
        out
    }

    #[test]
    fn bcpack_requests_unpack_into_the_destination() {
        with_test_tables();
        let mut contents = pattern(100);
        contents.extend_from_slice(&BC4_STREAM);
        let (_dir, file) = file_with(&contents);
        let (queue, array) = (new_queue(0), status_array(1));
        let mut out = [0u8; 16];
        enqueue(queue, &request(4 << 1, out.as_mut_ptr().cast(), 16, file, 100, 88));
        enqueue_status(queue, array, 0);
        void(queue, 0x30);
        assert_eq!(wait(array, 0), S_OK);
        assert_eq!(out, bc4_blocks());
    }

    #[test]
    fn a_closed_queue_completes_nothing() {
        let (_dir, file) = file_with(&pattern(64));
        let (queue, array) = (new_queue(0), status_array(1));
        let mut out = [0u8; 16];
        enqueue(queue, &request(0, out.as_mut_ptr().cast(), 16, file, 0, 16));
        enqueue_status(queue, array, 0);
        void(queue, 0x40);
        void(queue, 0x30);
        std::thread::sleep(Duration::from_millis(50));
        assert_eq!(result(array, 0), E_PENDING);
        assert_eq!(out, [0; 16]);
    }

    fn queue_with(capacity: u16, priority: i8, options: u64) -> (HRESULT, *mut c_void) {
        let create: extern "system" fn(*mut c_void, *const QueueDesc, *const Guid, Out) -> HRESULT =
            call(factory(), 0x50);
        let desc = QueueDesc {
            source_type: 0,
            capacity,
            priority,
            name: c"test".as_ptr(),
            options,
        };
        let mut queue = std::ptr::null_mut();
        (create(factory(), &desc, std::ptr::null(), &mut queue), queue)
    }

    /// A stand-in D3D12 fence: QueryInterface (any id), AddRef, Release,
    /// and Signal at +0x50.
    #[repr(C)]
    struct TestFence {
        table: *const [*const (); 11],
        value: AtomicU64,
        refs: AtomicU32,
    }
    extern "system" fn fence_query(this: *mut TestFence, _: *const Guid, out: Out) -> HRESULT {
        // SAFETY: the test's fence.
        unsafe { (*this).refs.fetch_add(1, Ordering::Relaxed) };
        put(out, this.cast())
    }
    extern "system" fn fence_add_ref(this: *mut TestFence) -> u32 {
        // SAFETY: as above.
        unsafe { (*this).refs.fetch_add(1, Ordering::Relaxed) + 1 }
    }
    extern "system" fn fence_release(this: *mut TestFence) -> u32 {
        // SAFETY: as above.
        unsafe { (*this).refs.fetch_sub(1, Ordering::Relaxed) - 1 }
    }
    extern "system" fn fence_signal(this: *mut TestFence, value: u64) -> HRESULT {
        // SAFETY: as above.
        unsafe { (*this).value.store(value, Ordering::Release) };
        S_OK
    }
    static FENCE_TABLE: SyncTable = SyncTable({
        let mut t = [std::ptr::null::<()>(); 11];
        t[0] = fence_query as *const ();
        t[1] = fence_add_ref as *const ();
        t[2] = fence_release as *const ();
        t[10] = fence_signal as *const ();
        t
    });
    struct SyncTable([*const (); 11]);
    // SAFETY: function pointers only.
    unsafe impl Sync for SyncTable {}

    #[test]
    fn signals_and_events_follow_the_reads_before_them() {
        use windows_sys::Win32::System::Threading::{CreateEventW, WaitForSingleObject};
        let (_dir, file) = file_with(&pattern(64));
        let queue = new_queue(0);
        let mut fence = TestFence { table: &FENCE_TABLE.0, value: AtomicU64::new(0), refs: AtomicU32::new(1) };
        // SAFETY: a plain event.
        let event = unsafe { CreateEventW(std::ptr::null(), 1, 0, std::ptr::null()) };
        let mut out = [0u8; 16];
        enqueue(queue, &request(0, out.as_mut_ptr().cast(), 16, file, 0, 16));
        let signal: extern "system" fn(*mut c_void, *mut c_void, u64) = call(queue, 0x28);
        signal(queue, (&raw mut fence).cast(), 7);
        let set: extern "system" fn(*mut c_void, *mut c_void) = call(queue, 0x60);
        set(queue, event);
        std::thread::sleep(Duration::from_millis(20));
        assert_eq!(fence.value.load(Ordering::Acquire), 0, "not submitted");
        void(queue, 0x30);
        // SAFETY: our event.
        assert_eq!(unsafe { WaitForSingleObject(event, 10_000) }, 0);
        assert_eq!(fence.value.load(Ordering::Acquire), 7);
        assert_eq!(out[..], pattern(64)[..16]);
        assert_eq!(fence.refs.load(Ordering::Relaxed), 1, "the queue's reference let go");
        void(queue, 0x40);
    }

    #[test]
    fn cancelled_requests_report_abort_and_are_not_read() {
        let (_dir, file) = file_with(&pattern(64));
        let (queue, array) = (new_queue(0), status_array(1));
        let mut outs = [[0u8; 16]; 2];
        let mut tagged = request(0, outs[0].as_mut_ptr().cast(), 16, file, 0, 16);
        tagged.tag = 0x15;
        enqueue(queue, &tagged);
        enqueue(queue, &request(0, outs[1].as_mut_ptr().cast(), 16, file, 0, 16));
        enqueue_status(queue, array, 0);
        let cancel: extern "system" fn(*mut c_void, u64, u64) = call(queue, 0x38);
        cancel(queue, 0xff, 0x15);
        void(queue, 0x30);
        assert_eq!(wait(array, 0), CANCELLED);
        assert_eq!(outs[0], [0; 16]);
        assert_eq!(outs[1][..], pattern(64)[..16]);
        void(queue, 0x40);
    }

    #[test]
    fn the_first_failure_is_recorded_and_sets_the_error_event() {
        use windows_sys::Win32::Foundation::CloseHandle;
        use windows_sys::Win32::System::Threading::WaitForSingleObject;
        let (_dir, file) = file_with(&pattern(64));
        let (queue, array) = (new_queue(0), status_array(1));
        let record = |queue| {
            let mut r = vec![0xaau8; ERROR_RECORD];
            let f: extern "system" fn(*mut c_void, *mut u8) = call(queue, 0x50);
            f(queue, r.as_mut_ptr());
            r
        };
        let none = record(queue);
        assert_eq!(none[..4], 0u32.to_le_bytes());
        assert_eq!(none[0xc..0x10], u32::MAX.to_le_bytes());
        let get_event: extern "system" fn(*mut c_void) -> *mut c_void = call(queue, 0x48);
        let event = get_event(queue);
        assert!(!event.is_null());
        let mut out = [0u8; 16];
        let mut past = request(0, out.as_mut_ptr().cast(), 16, file, 60, 16);
        past.tag = 9;
        enqueue(queue, &past);
        enqueue_status(queue, array, 0);
        void(queue, 0x30);
        assert_eq!(wait(array, 0), END_OF_FILE);
        // SAFETY: our duplicate of the queue's event.
        assert_eq!(unsafe { WaitForSingleObject(event, 10_000) }, 0);
        unsafe { CloseHandle(event) };
        let r = record(queue);
        assert_eq!(r[..4], 1u32.to_le_bytes());
        assert_eq!(r[0x8..0xc], END_OF_FILE.to_le_bytes());
        assert_eq!(r[0xc..0x10], 0u32.to_le_bytes(), "a request");
        assert_eq!(r[0x230..0x238], 60u64.to_le_bytes());
        assert_eq!(r[0x248..0x250], 9u64.to_le_bytes());
        let path: Vec<u16> = r[0x24..0x24 + 520].chunks(2).map(|c| u16::from_le_bytes([c[0], c[1]])).take_while(|&u| u != 0).collect();
        assert!(String::from_utf16_lossy(&path).ends_with("data.bin"));
        void(queue, 0x40);
    }

    #[test]
    fn query_reports_the_description_and_slots() {
        let (hr, queue) = queue_with(8, 1, 0);
        assert_eq!(hr, S_OK);
        let mut info = QueueInfo { source_type: 9, capacity: 0, priority: 0, name: std::ptr::null(), empty_slots: 0, until_auto_submit: 0 };
        let q: extern "system" fn(*mut c_void, *mut QueueInfo) = call(queue, 0x58);
        q(queue, &mut info);
        assert_eq!((info.source_type, info.capacity, info.priority), (0, 8, 1));
        assert_eq!((info.empty_slots, info.until_auto_submit), (7, 4));
        let mut out = [0u8; 16];
        let (_dir, file) = file_with(&pattern(64));
        enqueue(queue, &request(0, out.as_mut_ptr().cast(), 16, file, 0, 16));
        q(queue, &mut info);
        assert_eq!(info.until_auto_submit, 3);
        let mut info1 = QueueInfo1 { source_type: 0, capacity: 0, priority: 0, name: std::ptr::null(), options: 0x10, empty_slots: 0, until_auto_submit: 0 };
        let q1: extern "system" fn(*mut c_void, *mut QueueInfo1) = call(queue, 0x68);
        q1(queue, &mut info1);
        assert_eq!(info1.options, 0x10, "not manual; the other bits kept");
        let (_, manual) = queue_with(8, 2, 1);
        q1(manual, &mut info1);
        assert_eq!((info1.options & 1, info1.until_auto_submit, info1.priority), (1, u16::MAX, 2));
        assert_eq!(std::mem::offset_of!(QueueInfo, empty_slots), 0x18);
        assert_eq!(std::mem::offset_of!(QueueInfo1, empty_slots), 0x20);
        void(queue, 0x40);
        void(manual, 0x40);
    }

    #[test]
    fn objects_answer_their_interfaces() {
        let mut out = std::ptr::null_mut();
        for iid in FACTORY_IIDS {
            assert_eq!(DStorageGetFactory(&iid, &mut out), S_OK);
        }
        assert_eq!(DStorageGetFactory(&FILE_IIDS[0], &mut out), E_NOINTERFACE);
        assert!(out.is_null());
        let queue = new_queue(0);
        let qi: extern "system" fn(*mut c_void, *const Guid, Out) -> HRESULT = call(queue, 0);
        assert_eq!(qi(queue, &QUEUE_IIDS[1], &mut out), S_OK);
        assert_eq!(out, queue);
        void(queue, 0x10); // the reference QueryInterface gave
        assert_eq!(qi(queue, &STATUS_IIDS[0], &mut out), E_NOINTERFACE);
        let array = status_array(1);
        let qi: extern "system" fn(*mut c_void, *const Guid, Out) -> HRESULT = call(array, 0);
        assert_eq!(qi(array, &IUNKNOWN, &mut out), S_OK);
        void(queue, 0x40);
    }

    #[test]
    fn reads_of_a_closed_file_fail() {
        let (_dir, file) = file_with(&pattern(64));
        let handle: extern "system" fn(*mut c_void) -> *mut c_void = call(file, 0x20);
        let h = handle(file);
        assert!(!h.is_null());
        // SAFETY: our duplicate.
        unsafe { windows_sys::Win32::Foundation::CloseHandle(h) };
        void(file, 0x18);
        assert!(handle(file).is_null());
        let (queue, array) = (new_queue(0), status_array(1));
        let mut out = [0u8; 16];
        enqueue(queue, &request(0, out.as_mut_ptr().cast(), 16, file, 0, 16));
        enqueue_status(queue, array, 0);
        void(queue, 0x30);
        assert_eq!(wait(array, 0), FILE_NOT_OPEN);
        void(queue, 0x40);
    }

    #[test]
    fn status_entries_start_pending_and_past_the_end_is_an_error() {
        let array = status_array(2);
        let done: extern "system" fn(*mut c_void, u32) -> bool = call(array, 0x18);
        assert_eq!(result(array, 1), E_PENDING);
        assert!(!done(array, 1));
        assert_eq!(result(array, 2), INDEX_OUT_OF_RANGE);
        assert!(!done(array, 2));
    }

    #[test]
    fn queue_priorities_and_options_are_checked() {
        assert_eq!(queue_with(0x80, 3, 1).0, INVALID_QUEUE_PRIORITY);
        assert_eq!(queue_with(0x80, -2, 1).0, INVALID_QUEUE_PRIORITY);
        assert_eq!(queue_with(0x80, 0, 2).0, E_INVALIDARG);
        let (hr, queue) = queue_with(0x80, 2, 1);
        assert_eq!(hr, S_OK);
        void(queue, 0x40);
    }

    #[test]
    fn a_queue_without_manual_submit_submits_itself_at_half() {
        let (_dir, file) = file_with(&pattern(64));
        let (hr, queue) = queue_with(8, 0, 0);
        assert_eq!(hr, S_OK);
        let array = status_array(1);
        let mut outs = [[0u8; 16]; 3];
        for out in &mut outs {
            enqueue(queue, &request(0, out.as_mut_ptr().cast(), 16, file, 8, 16));
        }
        // The fourth command (half of 8) submits the lot; nobody calls Submit.
        enqueue_status(queue, array, 0);
        assert_eq!(wait(array, 0), S_OK);
        assert!(outs.iter().all(|out| out[..] == pattern(64)[8..24]));
        void(queue, 0x40);
    }

    #[test]
    fn a_closed_queue_leaves_status_entries_as_they_were() {
        let (_dir, file) = file_with(&pattern(64));
        let (queue, array) = (new_queue(0), status_array(1));
        let mut out = [0u8; 16];
        enqueue(queue, &request(0, out.as_mut_ptr().cast(), 16, file, 0, 16));
        enqueue_status(queue, array, 0);
        void(queue, 0x30);
        assert_eq!(wait(array, 0), S_OK);
        void(queue, 0x40);
        enqueue_status(queue, array, 0);
        assert_eq!(result(array, 0), S_OK, "not made pending again");
        void(queue, 0x40); // a second close: an error, nothing else
    }

    #[test]
    fn physical_page_destinations() {
        use crate::modules::xmem;
        let mut pages = [0usize; 2];
        let mut n = 2;
        assert_eq!(
            xmem::XMemAllocatePhysicalPages(0, &mut n, pages.as_mut_ptr()),
            1
        );
        // Where the title would see them: a reservation of 64 KiB pages.
        let mapped = xmem::XMemVirtualAlloc(
            std::ptr::null_mut(),
            2 * PAGE,
            0x2000 | 0x2040_0000, // reserve, 64 KiB pages
            0x21,                  // CPU, mappable: as the title asks
            0x04,
        );
        assert!(!mapped.is_null());
        assert_eq!(
            xmem::XMemMapPhysicalPages(mapped, 2, pages.as_ptr()),
            mapped
        );

        let plain = pattern(100);
        let (queue, array) = (new_queue(1), status_array(1));
        let mut r = request(
            PAGE_DESTINATION | MEMORY_SOURCE,
            pages.as_mut_ptr().cast(),
            plain.len(),
            plain.as_ptr().cast(),
            0,
            plain.len(),
        );
        r.page_offset = (PAGE - 40) as u64; // across the page boundary
        enqueue(queue, &r);
        enqueue_status(queue, array, 0);
        void(queue, 0x30);
        assert_eq!(wait(array, 0), S_OK);
        // SAFETY: both pages are mapped read-write.
        let seen = unsafe { std::slice::from_raw_parts(mapped.cast::<u8>().add(PAGE - 40), 100) };
        assert_eq!(seen, plain);
    }
    #[test]
    fn zlib_into_pages_in_place_and_across_allocations() {
        use crate::modules::xmem;
        let allocate = |n: usize| {
            let (mut pages, mut given) = (vec![0usize; n], n);
            assert_eq!(
                xmem::XMemAllocatePhysicalPages(0, &mut given, pages.as_mut_ptr()),
                1
            );
            pages
        };
        let (a, b) = (allocate(3), allocate(1));
        let plain = pattern(2 * PAGE + 100);
        let packed = compress_zlib(&plain);
        // One allocation's pages in order (one window), then pages of two
        // (through a buffer), both from 50 bytes into the first.
        let mut destinations = [a.clone(), vec![a[0], a[1], b[0]]];
        let (queue, array) = (new_queue(1), status_array(1));
        for pages in &mut destinations {
            let mut r = request(
                ZLIB | PAGE_DESTINATION | MEMORY_SOURCE,
                pages.as_mut_ptr().cast(),
                plain.len(),
                packed.as_ptr().cast(),
                0,
                packed.len(),
            );
            r.page_offset = 50;
            enqueue(queue, &r);
        }
        enqueue_status(queue, array, 0);
        void(queue, 0x30);
        assert_eq!(wait(array, 0), S_OK);
        for pages in &destinations {
            let mapped = xmem::XMemVirtualAlloc(
                std::ptr::null_mut(),
                3 * PAGE,
                0x2000 | 0x2040_0000, // reserve, 64 KiB pages
                0x21,                  // CPU, mappable: as the title asks
                0x04,
            );
            assert!(!mapped.is_null());
            assert_eq!(
                xmem::XMemMapPhysicalPages(mapped, 3, pages.as_ptr()),
                mapped
            );
            // SAFETY: all three pages are mapped read-write.
            let seen = unsafe { std::slice::from_raw_parts(mapped.cast::<u8>(), 3 * PAGE) };
            assert!(seen[..50].iter().all(|&b| b == 0));
            assert_eq!(seen[50..50 + plain.len()], plain);
            assert_eq!(
                xmem::XMemMapPhysicalPages(mapped, 3, std::ptr::null()),
                mapped
            );
        }
        assert_eq!(xmem::XMemFreePhysicalPages(3, a.as_ptr()), 1);
        assert_eq!(xmem::XMemFreePhysicalPages(1, b.as_ptr()), 1);
    }

    #[test]
    fn zlib_inflate_handles_short_exact_empty_and_bad_streams() {
        let plain = pattern(65_537);
        let packed = compress_zlib(&plain);

        // Exact fit.
        let mut exact = vec![0u8; plain.len()];
        assert_eq!(inflate(&packed, &mut exact), Ok(plain.len()));
        assert_eq!(exact, plain);

        // Short stream into a larger buffer passes and returns bytes written;
        // guard bytes outside the slice passed to `inflate` stay untouched.
        let mut larger = vec![0xAA; plain.len() + 64];
        assert_eq!(
            inflate(&packed, &mut larger[..plain.len() + 32]),
            Ok(plain.len())
        );
        assert_eq!(&larger[..plain.len()], &plain[..]);
        assert!(larger[plain.len() + 32..].iter().all(|&b| b == 0xAA));

        // Empty valid stream passes for empty or non-empty destinations.
        let empty = compress_zlib(&[]);
        assert_eq!(inflate(&empty, &mut []), Ok(0));
        let mut scratch = [0x55u8; 4];
        assert_eq!(inflate(&empty, &mut scratch), Ok(0));

        // Oversized stream (destination too small, including empty) fails.
        let mut small = vec![0u8; plain.len() - 1];
        assert_eq!(inflate(&packed, &mut small), Err(INVALID_DATA));
        assert_eq!(inflate(&packed, &mut []), Err(INVALID_DATA));

        // Truncated stream (missing trailer or payload bytes) fails.
        let mut out = vec![0u8; plain.len()];
        assert_eq!(
            inflate(&packed[..packed.len() - 1], &mut out),
            Err(INVALID_DATA)
        );
        assert_eq!(
            inflate(&packed[..packed.len() / 2], &mut out),
            Err(INVALID_DATA)
        );
        assert_eq!(inflate(&[], &mut out), Err(INVALID_DATA));

        // Corrupt Adler-32 trailer fails.
        let mut bad_adler = packed.clone();
        *bad_adler.last_mut().unwrap() ^= 1;
        assert_eq!(inflate(&bad_adler, &mut out), Err(INVALID_DATA));

        // Corrupt header/payload fails, and subsequent valid inflate on the
        // same thread still succeeds (verifies thread-local state reset).
        let mut bad_body = packed.clone();
        bad_body[2] ^= 0xFF;
        assert_eq!(inflate(&bad_body, &mut out), Err(INVALID_DATA));
        assert_eq!(inflate(&packed, &mut out), Ok(plain.len()));
        assert_eq!(out, plain);
    }
}
