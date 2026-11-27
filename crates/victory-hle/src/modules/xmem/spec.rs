//! `xmem.dll`'s own decisions, as pure functions of its arguments and of
//! what the system tells it ([`Env`]): what it asks the kernel for, which
//! requests it refuses and with what error, where `XMemAllocDefault` takes
//! a block, what a tag is. The exports in `xmem.rs` act on these; the tests
//! compare them with what the console's `xmem.dll` asks its kernel for,
//! called under a harness (`tests.rs`, vectors in `vectors/`).
//!
//! Function names `xmem!0x…` are addresses in `xmem.dll` 10.0.26100.7901.

/// What `xmem.dll` reads of the system and keeps: the console mode
/// (`XpalGetConsoleMode`), the system type (`XpalGetSystemType`), whether
/// the tools partition exists, the answers of its two capability queries,
/// the NUMA node count, the title partition's size, and the options
/// `XMemSetOption` set.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Env {
    pub mode: u16,
    pub system_type: u16,
    pub tools_partition: bool,
    /// Device query 0x150068 (`xmem!0x180004dfc`): bit 0 asks for write
    /// watching of graphics memory.
    pub write_watch_query: u64,
    /// Device query 0x1500e8 with 0x3b (`xmem!0x180004888`,
    /// `xmem!0x1800049a0`).
    pub tools_query: u64,
    /// `NtQuerySystemInformation(0x3c)`'s first word; 0 when it fails.
    pub numa_nodes: u32,
    /// The title partition's bytes (`xmem!0x180010980`, read at start).
    pub title_partition: u64,
    /// Option 0x10001 (`xmem!0x180010158`, 1 at start): write watching of
    /// graphics memory allowed.
    pub graphics_write_watch: u32,
    /// Option 2 (`xmem!0x1800108f0`): `XMemAllocDefault` tags its own
    /// allocations.
    pub tag_allocations: u32,
    /// Option 5 (`xmem!0x1800108ec`): every `XMemAllocDefault` block an
    /// allocation of its own.
    pub all_virtual: u32,
    /// Option 6 (`xmem!0x1800108e4`): heaps grow by 2 MiB, not 512 KiB.
    pub large_chunks: u32,
}

impl Env {
    /// The retail console as the layer presents it: Series X title mode,
    /// no tools partition, no NUMA answer (the host's), 13.5 GiB of title
    /// memory.
    pub const RETAIL: Env = Env {
        mode: 0,
        system_type: 0,
        tools_partition: false,
        write_watch_query: 0,
        tools_query: 0,
        numa_nodes: 0,
        title_partition: 13_824 << 20,
        graphics_write_watch: 1,
        tag_allocations: 0,
        all_virtual: 0,
        large_chunks: 0,
    };

    /// A devkit in tools mode (`xmem!0x180004888`): mode 0x68, or mode
    /// 0x60 with query 0x1500e8 answering 0x60 in bits 3-10.
    pub fn devkit_tools(&self) -> bool {
        self.mode == 0x68 || self.mode == 0x60 && self.tools_query & 0x7f8 == 0x60
    }

    /// Profiling mode (`xmem!0x180004a10`): a devkit in tools mode, or a
    /// tools partition and either `any` or (system type not 0x60) query
    /// 0x1500e8's bit 0 (`xmem!0x1800049a0`).
    pub fn profiling(&self, any: bool) -> bool {
        if self.devkit_tools() {
            return true;
        }
        if !self.tools_partition {
            return false;
        }
        any || self.system_type != 0x60 && self.tools_query & 1 == 1
    }

    /// Graphics memory is write-watched (`xmem!0x180004dfc`).
    pub fn graphics_write_watch(&self) -> bool {
        (self.write_watch_query & 1 == 1 || self.profiling(true)) && self.graphics_write_watch != 0
    }

    /// `xmem!0x180004934`.
    pub fn numa(&self) -> bool {
        self.numa_nodes as i32 > 0
    }
}

pub const MEM_COMMIT: u32 = 0x1000;
pub const MEM_RESERVE: u32 = 0x2000;
pub const MEM_RESERVE_PLACEHOLDER: u32 = 0x4_0000;
pub const MEM_REPLACE_PLACEHOLDER: u32 = 0x4000;
pub const MEM_RESET: u32 = 0x8_0000;
pub const MEM_WRITE_WATCH: u32 = 0x20_0000;
/// The kernel's AWE reservation (`MEM_PHYSICAL`).
pub const MEM_PHYSICAL: u32 = 0x40_0000;
pub const MEM_ROTATE: u32 = 0x80_0000;
pub const MEM_RESET_UNDO: u32 = 0x100_0000;
/// 2 MiB pages; with [`PAGES_64K_BIT`], 64 KiB pages.
pub const PAGES_2M: u32 = 0x2000_0000;
pub const PAGES_64K_BIT: u32 = 0x40_0000;
pub const PAGES_64K: u32 = PAGES_2M | PAGES_64K_BIT;

pub const XMEM_CPU: u64 = 1;
pub const XMEM_GPU: u64 = 2;
pub const XMEM_32BIT: u64 = 4;
pub const XMEM_TOOL: u64 = 0x10;
pub const XMEM_MAPPABLE: u64 = 0x20;
pub const XMEM_ALIGN_2M: u64 = 0x40;
pub const XMEM_BANDWIDTH: u64 = 0x80;
pub const XMEM_TAG: u64 = 0xffff_ff00_0000_0000;
pub const PAGE_NOCACHE: u32 = 0x200;

pub const ERROR_INVALID_PARAMETER: u32 = 87;
pub const ERROR_NOT_FOUND: u32 = 1168;

/// An extended parameter of `NtAllocateVirtualMemoryEx`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Param {
    /// 1: lowest and highest address, alignment.
    Address(u64, u64, u64),
    /// 2: the NUMA node / bandwidth class.
    Numa(u64),
    /// 3: the tools partition's handle.
    Partition,
    /// 4: the title's physical-page (AWE) section.
    Physical,
    /// 5: attribute flags.
    Attributes(u64),
}

/// What `XMemVirtualAlloc` asks the kernel for.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Request {
    pub base: u64,
    pub size: u64,
    pub kind: u32,
    pub protect: u32,
    pub params: Vec<Param>,
}

/// `xmem!0x180005258`: the retail checks, in order.
pub fn refused(env: &Env, size: u64, kind: u32, flags: u64, protect: u32) -> Option<u32> {
    let invalid = Some(ERROR_INVALID_PARAMETER);
    if kind & (MEM_RESET | MEM_RESET_UNDO | MEM_ROTATE) != 0
        || size == 0
        || protect == 0
        || flags & XMEM_CPU == 0
        || flags & XMEM_GPU != 0 && protect & PAGE_NOCACHE != 0
    {
        return invalid;
    }
    if flags & XMEM_MAPPABLE != 0
        && (kind & (MEM_COMMIT | MEM_RESERVE) != MEM_RESERVE
            || kind & MEM_RESERVE_PLACEHOLDER != 0
            || kind & PAGES_2M == 0)
    {
        return invalid;
    }
    if flags & XMEM_TOOL != 0 {
        if kind & PAGES_2M == 0 {
            return invalid;
        }
        if !env.profiling(flags >> 15 & 1 != 0) {
            return Some(ERROR_NOT_FOUND);
        }
    }
    if kind & MEM_COMMIT == 0 && flags & XMEM_TAG != 0 {
        return invalid;
    }
    if flags & XMEM_BANDWIDTH != 0 && kind & PAGES_64K == 0 {
        return invalid;
    }
    None
}

/// The page `kind` asks for (`xmem!0x1800032d0`).
pub fn page_size(kind: u32) -> u64 {
    if kind & PAGES_64K == PAGES_64K {
        64 << 10
    } else if kind & PAGES_2M != 0 {
        2 << 20
    } else {
        4 << 10
    }
}

/// `xmem!0x180004e78`: lowest and highest address and alignment of an
/// allocation at an address of the kernel's choice. (0, 0): anywhere.
pub fn address_requirements(kind: u32, flags: u64) -> (u64, u64, u64) {
    let alignment = if kind & PAGES_64K == PAGES_2M || kind & MEM_RESERVE_PLACEHOLDER != 0 || flags & XMEM_ALIGN_2M != 0 {
        2 << 20
    } else {
        64 << 10
    };
    const TIB: u64 = 1 << 40;
    let (lowest, highest) = if flags & XMEM_GPU != 0 {
        if flags & XMEM_32BIT != 0 { (1 << 31, u32::MAX as u64) } else { (1 << 32, TIB - 1) }
    } else {
        match 47 - (flags >> 12 & 7) as u32 {
            47 => (0, 0),
            40 => (1 << 32, TIB - 1),
            bits => (TIB, (1 << bits) - 1),
        }
    };
    (lowest, highest, alignment)
}

/// `xmem!0x1800032d0` up to its kernel call: the request, or the error it
/// fails with.
pub fn virtual_alloc(env: &Env, base: u64, size: u64, kind: u32, flags: u64, protect: u32) -> Result<Request, u32> {
    if let Some(error) = refused(env, size, kind, flags, protect) {
        return Err(error);
    }
    let mut flags = flags;
    // Series S title mode: bandwidth classes for pages of the large kinds.
    if env.mode == 0x60 && kind & PAGES_64K != 0 && flags & XMEM_MAPPABLE == 0 {
        if flags & XMEM_TOOL != 0 {
            flags |= 0x380;
        } else if env.title_partition < 0x800_0000 {
            flags = flags & !0x100 | 0x280;
        }
    }
    let page = page_size(kind);
    let (base, size) = if base == 0 {
        (0, size.div_ceil(page) * page)
    } else {
        let start = base & !(page - 1);
        (start, (base + size).div_ceil(page) * page - start)
    };
    let mut kind = kind;
    let mut params = Vec::new();
    if base == 0 {
        let (lo, hi, align) = address_requirements(kind, flags);
        params.push(Param::Address(lo, hi, align));
    }
    let large_only = |kind: u32| kind & PAGES_64K == PAGES_2M;
    if flags & XMEM_MAPPABLE != 0 {
        params.push(Param::Physical);
        if flags & XMEM_GPU != 0 {
            if env.graphics_write_watch() {
                kind |= MEM_WRITE_WATCH;
            }
            params.push(Param::Attributes(if large_only(kind) { 9 } else { 1 }));
        } else if large_only(kind) {
            params.push(Param::Attributes(8));
        }
        kind = kind & !PAGES_2M | MEM_PHYSICAL;
    } else if flags & XMEM_GPU != 0 {
        if kind & MEM_RESERVE_PLACEHOLDER == 0 {
            if kind & MEM_RESERVE != 0 || kind & MEM_COMMIT != 0 && base == 0 {
                kind |= MEM_RESERVE;
                if env.graphics_write_watch() {
                    kind |= MEM_WRITE_WATCH;
                }
            }
            let mut a = if large_only(kind) { 9 } else { 3 };
            if kind & PAGES_64K != 0 {
                a |= 4;
            }
            params.push(Param::Attributes(a));
        }
    } else if kind & PAGES_64K != 0 {
        params.push(Param::Attributes(if large_only(kind) { 8 } else { 2 } | 4));
    }
    if flags & XMEM_TOOL != 0 && !env.devkit_tools() && env.tools_partition {
        params.push(Param::Partition);
    }
    if env.numa() {
        let mut v = if flags & XMEM_GPU != 0 { 1 } else { 0 };
        if flags & XMEM_BANDWIDTH != 0 {
            v = flags >> 9 & 1;
            if flags & 0x100 == 0 {
                v |= 1 << 63;
            }
        }
        params.push(Param::Numa(v));
    }
    Ok(Request { base, size, kind, protect, params })
}

/// `XMemAlloc` attributes: alignment as log2 (bits 24-28), memory type
/// (bits 29-32), bit 33 a 32-bit address; page size at bits 14-15.
pub const ALIGNMENT_SHIFT: u64 = 24;
pub const ALLOC_32BIT: u64 = 1 << 33;

pub fn memory_type(attributes: u64) -> usize {
    (attributes >> 29 & 0xf) as usize
}

/// What `xmem.dll` adds to the allocation type of each memory type
/// (`xmem!0x18000ccd0`): graphics (bit 28), 64 KiB CPU pages (bit 22),
/// nothing.
pub const KIND_ALLOCATION: [u32; 16] = [
    0, GRAPHICS_KIND, GRAPHICS_KIND, GRAPHICS_KIND, GRAPHICS_KIND, GRAPHICS_KIND, GRAPHICS_KIND,
    LARGE_CPU_KIND, LARGE_CPU_KIND, LARGE_CPU_KIND, GRAPHICS_KIND, GRAPHICS_KIND, GRAPHICS_KIND, 0, 0, 0,
];
pub const GRAPHICS_KIND: u32 = 0x1000_0000;
pub const LARGE_CPU_KIND: u32 = 0x40_0000;
/// The protection of each memory type (`xmem!0x18000cd10`); 0: refused.
pub const KIND_PROTECTION: [u32; 16] = [
    0x4, 0x2404, 0x1404, 0x22004, 0x1004, 0x50404, 0x70004, 0x4, 0x404, 0x204, 0x62004, 0x8004, 0x10404, 0, 0, 0,
];

/// Where `XMemAllocDefault` (`xmem!0x1800055b0`) takes a block from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Route {
    /// The title partition's process heap.
    ProcessHeap,
    /// One of the memory type's heaps: index (type, plus 16 for 4 KiB
    /// units).
    Heap(usize),
    /// An allocation of its own.
    Virtual,
}

impl Route {
    pub fn of(env: &Env, size: u64, attributes: u64) -> Option<Route> {
        let t = memory_type(attributes);
        if size == 0 || KIND_PROTECTION[t] == 0 {
            return None;
        }
        let align = attributes >> ALIGNMENT_SHIFT & 0x1f;
        let pages = attributes >> 14 & 3;
        let allocation = KIND_ALLOCATION[t];
        if allocation == 0 && align <= 4 && pages == 0 {
            return Some(Route::ProcessHeap);
        }
        if allocation == LARGE_CPU_KIND {
            return Some(Route::Virtual);
        }
        let pages = if allocation == GRAPHICS_KIND && pages == 0 { 1 } else { pages };
        if align > 16 || size > 0x2_0000 || attributes & ALLOC_32BIT != 0 || pages == 2 {
            return Some(Route::Virtual);
        }
        let index = if align > 12 || size > 0xf00 { t | 0x10 } else { t };
        if env.all_virtual != 0 {
            return Some(Route::Virtual);
        }
        Some(Route::Heap(index))
    }
}

/// The `XMemVirtualAlloc` call of [`Route::Virtual`] (`xmem!0x1800066d0`):
/// type, XMem flags, protection.
pub fn virtual_request(env: &Env, attributes: u64) -> (u32, u64, u32) {
    let t = memory_type(attributes);
    let align = attributes >> ALIGNMENT_SHIFT & 0x1f;
    let allocation = KIND_ALLOCATION[t];
    let mut pages = attributes >> 14 & 3;
    if allocation == GRAPHICS_KIND && pages == 0 {
        pages = 1;
    }
    const ALIGN_2M_KIND: u32 = 0x2_0000;
    let mut kind = MEM_COMMIT | MEM_RESERVE | if align == 17 { ALIGN_2M_KIND } else { 0 };
    kind |= match pages {
        1 => PAGES_64K,
        2 => PAGES_2M,
        _ => 0,
    };
    if attributes & ALLOC_32BIT != 0 {
        kind |= MEM_ROTATE;
    }
    kind |= allocation;
    if kind & PAGES_64K_BIT != 0 {
        kind &= !PAGES_64K_BIT;
        if pages >= 2 {
            kind |= PAGES_2M;
        } else if align <= 16 {
            kind |= PAGES_64K;
        }
    }
    let mut flags = XMEM_CPU;
    if kind & GRAPHICS_KIND != 0 {
        kind &= !GRAPHICS_KIND;
        flags = 3;
        if kind & MEM_ROTATE != 0 {
            kind &= !MEM_ROTATE;
            flags |= XMEM_32BIT;
        }
    }
    if kind & ALIGN_2M_KIND != 0 {
        kind &= !ALIGN_2M_KIND;
        flags |= XMEM_ALIGN_2M;
    }
    if env.tag_allocations != 0 {
        flags |= make_tag(0x586d_5641);
    }
    (kind, flags, KIND_PROTECTION[t])
}

/// A heap of a memory type (`xmem!0x1800072d0`): its unit, its
/// reservation (what it asks `XMemVirtualAlloc` for: 2 GiB of 64 KiB pages
/// of the type's kind and protection) and the bytes it commits at a time.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct HeapShape {
    pub unit: u64,
    pub reserve: u64,
    pub kind: u32,
    pub flags: u64,
    pub protect: u32,
    pub chunk: u64,
    /// How a chunk is asked for (at its address, the heap's flags and the
    /// type's protection): committed 64 KiB or (option 6) 2 MiB pages.
    pub chunk_kind: u32,
    /// With option 6 the heap's placeholder is first made 2 MiB
    /// reservations of 2 MiB pages, all of it, one piece at a time: how each
    /// piece is asked for.
    pub piece_kind: Option<u32>,
    /// Hysteresis at the start, in chunks (`+0x88`): 64 MiB for graphics
    /// memory, 8 MiB for CPU memory.
    pub hysteresis_chunks: u32,
}

pub fn heap_shape(env: &Env, index: usize) -> HeapShape {
    let t = index & 0xf;
    let graphics = KIND_ALLOCATION[t] == GRAPHICS_KIND;
    let chunk_shift = if env.large_chunks != 0 { 21 } else { 19 };
    let large = env.large_chunks != 0;
    HeapShape {
        unit: if index & 0x10 != 0 { 4 << 10 } else { 256 },
        reserve: 1 << 31,
        kind: if large { MEM_RESERVE | MEM_RESERVE_PLACEHOLDER } else { MEM_RESERVE | PAGES_64K },
        flags: if graphics { 3 } else { XMEM_CPU },
        protect: if large { 1 } else { KIND_PROTECTION[t] },
        chunk: 1 << chunk_shift,
        chunk_kind: if large { MEM_COMMIT | PAGES_2M } else { MEM_COMMIT | PAGES_64K },
        piece_kind: large.then_some(MEM_RESERVE | MEM_REPLACE_PLACEHOLDER | PAGES_2M),
        hysteresis_chunks: (if graphics { 64u32 << 20 } else { 8 << 20 }) >> chunk_shift,
    }
}

/// `XMemMakeTag` (`xmem!0x180009a20`): four characters (low byte first)
/// as 6-bit codes (a-z 0-25, A-Z 26-51, 0-9 52-61) at
/// bits 40, 46, 52 and 58 of an XMem flags word. 0 for 0.
/// (`$` is 62, every other character 63.)
pub fn make_tag(tag: u32) -> u64 {
    if tag == 0 {
        return 0;
    }
    let code = |c: u8| -> u64 {
        match c {
            b'a'..=b'z' => (c - b'a') as u64,
            b'A'..=b'Z' => (c - b'A' + 26) as u64,
            b'0'..=b'9' => (c - b'0' + 52) as u64,
            b'$' => 62,
            _ => 63,
        }
    };
    let b = tag.to_le_bytes();
    (code(b[0]) | code(b[1]) << 6 | code(b[2]) << 12 | code(b[3]) << 18) << 40
}

/// The characters of a tag's codes (`xmem!0x18000dc50`), for
/// `XMemVirtualQuery`.
pub const TAG_ALPHABET: &[u8; 64] = b"abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789$*";

/// `XMemVirtualQuery`'s tag fields of XMem flags (`xmem!0x180003780`): the
/// 24-bit code and its four characters, last first.
pub fn tag_fields(flags: u64) -> (u32, [u8; 4]) {
    let code = (flags >> 40) as u32 & 0xff_ffff;
    let c = |i: u32| TAG_ALPHABET[(code >> (6 * i) & 0x3f) as usize];
    (code, [c(3), c(2), c(1), c(0)])
}
