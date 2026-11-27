//! `xg_xs.dll`: the console's texture layout library. Titles ask it which
//! swizzle mode a texture should use, how large it is, where each mip and
//! slice lies, and to copy linear data into tiled memory and back. Served
//! from `victory_formats::texture`.
//!
//! The title imports `XGCreateTextureComputer` and
//! `XGSuggestSwizzleMode` and calls these computer methods: Release (0x08),
//! the layout (0x10), copies into (0x40) and out of (0x48) a subresource,
//! and the 64 KiB tiling (0x50). The other slots log and fail.
#![allow(non_snake_case)]
// Title code passes pointers the real library would trust.
#![allow(clippy::not_unsafe_ptr_arg_deref)]

use std::sync::atomic::{AtomicU32, Ordering};

use victory_formats::texture::{Dimension, Error, Layout, Swizzle, Texture, TextureDesc};

use super::xasync::{E_INVALIDARG, E_NOTIMPL};
use super::xgameruntime::{HRESULT, S_OK};
use crate::{HleModule, hle_export};

const E_FAIL: HRESULT = 0x8000_4005_u32 as i32;

/// `D3D12_RESOURCE_DESC`, whose layout field carries `0x100 | swizzle
/// mode` on the console (1 row major, 0 "pick one").
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct ResourceDesc {
    pub dimension: u32,
    pub alignment: u64,
    pub width: u64,
    pub height: u32,
    pub depth_or_array: u16,
    pub mips: u16,
    pub format: u32,
    pub samples: u32,
    pub quality: u32,
    pub layout: u32,
    pub flags: u32,
}
const _: () = assert!(size_of::<ResourceDesc>() == 0x38);

const SWIZZLED: u32 = 0x100;
const ROW_MAJOR: u32 = 1;
const ALLOW_RENDER_TARGET: u32 = 1;
const ALLOW_DEPTH_STENCIL: u32 = 2;
const ALLOW_UNORDERED_ACCESS: u32 = 4;
/// A console flag that makes render and unordered-access textures take
/// the 64 KiB render XOR mode whatever their size.
const RENDER_XOR: u32 = 0x40_0000;
const BUFFER: u32 = 1;
const TEXTURE_1D: u32 = 2;
const TEXTURE_3D: u32 = 4;

/// The mode the console's library picks for a description (measured):
/// linear for buffers and 1D, depth `64KB_Z_X` for depth (1D and 3D too),
/// `64KB_R_X` for render targets and for unordered access with flag
/// 0x400000, else `4KB_S` (3D and plain 2D) or, for 2D unordered access,
/// `64KB_R_X` past 64 KiB of mip 0 (or with samples) and `4KB_S_X` below.
/// `None`: render target and depth together, which it refuses.
fn suggested(desc: &ResourceDesc) -> Option<Swizzle> {
    let flags = desc.flags;
    if flags & ALLOW_RENDER_TARGET != 0 && flags & ALLOW_DEPTH_STENCIL != 0 {
        return None;
    }
    let targets = flags & (ALLOW_RENDER_TARGET | ALLOW_UNORDERED_ACCESS) != 0;
    Some(if desc.dimension == BUFFER {
        Swizzle::LINEAR
    } else if flags & ALLOW_DEPTH_STENCIL != 0 {
        Swizzle::SW_64KB_Z_X
    } else if flags & RENDER_XOR != 0 && targets {
        Swizzle::SW_64KB_R_X
    } else if desc.dimension == TEXTURE_1D {
        Swizzle::LINEAR
    } else if flags & ALLOW_RENDER_TARGET != 0 {
        Swizzle::SW_64KB_R_X
    } else if flags & ALLOW_UNORDERED_ACCESS != 0 && desc.dimension != TEXTURE_3D {
        if desc.samples > 1 || mip0_bytes(desc) > 64 << 10 {
            Swizzle::SW_64KB_R_X
        } else {
            Swizzle::SW_4KB_S_X
        }
    } else {
        Swizzle::SW_4KB_S
    })
}

/// Bytes of one slice of mip 0 in elements of the first plane.
fn mip0_bytes(desc: &ResourceDesc) -> u64 {
    if (5..=8).contains(&desc.format) {
        // 96-bit: refused for layout, sized as 12 bytes here.
        return desc.width * u64::from(desc.height) * 12;
    }
    victory_formats::texture::planes(desc.format)
        .and_then(|p| p.first())
        .map_or(0, |f| {
            let w = desc.width.div_ceil(u64::from(f.block));
            let h = u64::from(desc.height.div_ceil(f.block_height));
            w * h * u64::from(f.bytes)
        })
}

/// `(desc, flags, out layout)`; the title passes flags 0 and puts the
/// result in the description's layout field.
pub extern "system" fn XGSuggestSwizzleMode(
    desc: *const ResourceDesc,
    _flags: u32,
    out: *mut u32,
) -> HRESULT {
    // SAFETY: the title passes a description and room for the layout
    // (either may be null).
    match unsafe { (desc.as_ref(), out.as_mut()) } {
        (Some(desc), Some(out)) => match suggested(desc) {
            Some(s) => {
                *out = SWIZZLED | s.0;
                S_OK
            }
            None => E_INVALIDARG,
        },
        _ => E_INVALIDARG,
    }
}

#[repr(C)]
pub struct Computer {
    vtable: &'static Vtable,
    refs: AtomicU32,
    desc: ResourceDesc,
    /// The mode the texture is laid out in.
    swizzle: Swizzle,
    mips: u32,
    texture: Texture,
    /// For a format laid out empty, the addresses the console's library
    /// still gives: those of 1-byte texels.
    bytes: Option<Texture>,
    /// A format the library lays out empty.
    degenerate: bool,
    /// The planes as the library reports them, and their whole.
    planes: Vec<Placed>,
    size: u64,
    alignment: u64,
}

/// A plane as the console's library reports it: one of the texture's
/// planes, or compression metadata the GPU keeps beside a render target
/// (`CMASK`, and `FMASK` with samples) or a depth buffer (`HTILE`).
#[derive(Debug)]
struct Placed {
    usage: u32,
    offset: u64,
    kind: PlaneKind,
}

#[derive(Debug)]
enum PlaneKind {
    /// Index into the texture's planes.
    Data(usize),
    Meta(Meta),
    /// A data plane with nothing laid out.
    Empty,
}

#[derive(Debug, Clone)]
struct Meta {
    size: u64,
    slice_size: u64,
    alignment: u64,
    /// The mode its mip reports (`FMASK` and `DCC`).
    swizzle: u32,
    dcc: Option<Dcc>,
}

/// `DCC`'s own mip chain: 8 KiB blocks, each for 2 MiB of texels
/// (2 samples counting for any number), the small mips sharing one 8 KiB
/// tail block at the start, the others after it from the smallest up.
#[derive(Debug, Clone)]
struct Dcc {
    /// Texels a block covers across and down.
    block: [u32; 2],
    /// Size and offset of each mip.
    mips: Vec<(u64, u64)>,
    /// The first mip in the tail.
    tail: usize,
    slice_size: u64,
}

const DCC_BLOCK: u64 = 8 << 10;

/// The `DCC` of a render target with the render XOR flag (measured).
fn dcc(desc: &ResourceDesc, l: &Layout, mips: u32) -> Dcc {
    let element_log2 = l.desc.element_bytes.trailing_zeros();
    let samples_log2 = u32::from(desc.samples > 1);
    let texels_log2 = 21 - element_log2 - samples_log2;
    let wide = texels_log2.div_ceil(2);
    let block = [1 << wide, 1 << (texels_log2 - wide)];
    // The tail takes mips of at most this many elements a side.
    let tail_side = match l.desc.element_bytes {
        1 | 2 => 128,
        4 | 8 => 64,
        _ => 32,
    };
    let (w, h) = (desc.width as u32, desc.height);
    let mut sizes = vec![];
    let mut tail = mips as usize;
    for m in 0..mips {
        let (mw, mh) = ((w >> m).max(1), (h >> m).max(1));
        let elements = mw.div_ceil(l.desc.block).max(mh.div_ceil(l.desc.block_height));
        // A single mip has no tail.
        if elements <= tail_side && mips > 1 {
            tail = m as usize;
            break;
        }
        sizes.push(DCC_BLOCK * u64::from(mw.div_ceil(block[0])) * u64::from(mh.div_ceil(block[1])));
    }
    let mut offset = if tail < mips as usize { DCC_BLOCK } else { 0 };
    let mut out = vec![(0, 0); mips as usize];
    for m in (0..sizes.len()).rev() {
        out[m] = (sizes[m], offset);
        offset += sizes[m];
    }
    if tail < mips as usize {
        out[tail] = (DCC_BLOCK, 0);
    }
    Dcc { block, mips: out, tail, slice_size: offset }
}

/// Usages the library reports.
const DATA: u32 = 1;
const CMASK: u32 = 2;
const FMASK: u32 = 3;
const HTILE: u32 = 4;
const FIRST_OF_TWO: u32 = 5;
const DEPTH: u32 = 7;
const DCC: u32 = 9;

/// `CMASK` and `HTILE` take this much per slice for each started 1024x1024
/// texels.
const CMASK_BLOCK: u64 = 8 << 10;
const HTILE_BLOCK: u64 = 64 << 10;

/// The planes of a texture in the library's order and place: `HTILE`
/// first for a depth buffer, then the texture's planes, then `FMASK` and
/// `CMASK` for a render target. Only single-mip 2D textures get metadata;
/// render targets only without the console's render XOR flag and with one
/// plane of up to 8-byte elements.
/// What the use flags change in a texture, as the console's library does
/// it: the render XOR flag refuses 3D and, for render targets, any mode
/// but `64KB_R_X`; a single-mip 2D depth buffer must be `64KB_Z_X` and is
/// laid out as its depth format (planes of nothing for other formats); a
/// block-compressed single-mip render target has no data plane laid out.
/// `None` where the library refuses; `true` with the data plane empty.
fn with_flags(desc: &ResourceDesc, texture: Option<Texture>, swizzle: Swizzle, mips: u32) -> Option<(Texture, bool)> {
    let flags = desc.flags;
    let surface = desc.dimension == 3 && mips == 1;
    if flags & RENDER_XOR != 0
        && (desc.dimension == TEXTURE_3D || (flags & ALLOW_RENDER_TARGET != 0 && swizzle != Swizzle::SW_64KB_R_X))
    {
        return None;
    }
    if flags & ALLOW_DEPTH_STENCIL != 0 && surface {
        if swizzle != Swizzle::SW_64KB_Z_X {
            return None;
        }
        // The formats it refuses stay refused.
        victory_formats::texture::planes(desc.format)?;
        // Laid out as the depth format; other formats keep their own
        // layout (if any) for addresses only.
        return Some(match depth_format(desc.format) {
            Some(format) => (Texture::new(&TextureDesc { format, ..texture_desc(desc, swizzle)? }).ok()?, false),
            None => (texture.unwrap_or(Texture { planes: vec![], size: 0, alignment: 0 }), true),
        });
    }
    if flags & ALLOW_RENDER_TARGET != 0 && flags & RENDER_XOR == 0 && surface && unrenderable(desc.format) {
        // Nothing laid out but metadata; multisampled, the description
        // itself need not make a texture (compressed in a depth mode).
        let small = victory_formats::texture::planes(desc.format)
            .and_then(|p| p.first())
            .is_some_and(|f| f.bytes <= 8 && f.bytes != 0);
        let empty = small || desc.samples > 1;
        let texture = match texture {
            Some(t) => t,
            None if desc.samples > 1 => Texture::new(&TextureDesc { format: R8_UNORM, ..texture_desc(desc, swizzle)? }).ok()?,
            None => return None,
        };
        return Some((texture, empty));
    }
    let texture = texture?;
    if texture.planes.len() > 1 && desc.samples > 1 {
        // No multisampled two-plane textures but depth buffers.
        return None;
    }
    Some((texture, false))
}

/// Formats the GPU cannot render to: block compressed, packed 4:2:2 and
/// 190 (`A4B4G4R4`).
fn unrenderable(format: u32) -> bool {
    format == 190
        || victory_formats::texture::planes(format)
            .and_then(|p| p.first())
            .is_some_and(|f| f.block != 1)
}

/// The format a depth buffer of `format` is laid out as (its depth and
/// stencil planes); `None` for formats with no depth.
fn depth_format(format: u32) -> Option<u32> {
    Some(match format {
        19 | 20 | 22 => 20,
        21 | 39 | 40 | 46 => 40,
        44 | 45 | 47 => 45,
        53 | 55 => 55,
        118..=120 => 118,
        _ => return None,
    })
}

fn place(desc: &ResourceDesc, texture: &Texture, mips: u32, empty: bool) -> (Vec<Placed>, u64, u64) {
    let flags = desc.flags;
    let surface = desc.dimension == 3 && mips == 1;
    let regions = desc.width.div_ceil(1024) * u64::from(desc.height.div_ceil(1024));
    let slices = u64::from(desc.depth_or_array.max(1));
    let depth = flags & ALLOW_DEPTH_STENCIL != 0;
    let htile = depth && surface && (empty || !texture.planes.is_empty());
    let first = texture.planes.first().map(|p| &p.layout);
    // With the render XOR flag only multisampled targets keep `FMASK` and
    // `CMASK` (every target gets `DCC`).
    let target = flags & ALLOW_RENDER_TARGET != 0
        && (flags & RENDER_XOR == 0 || desc.samples > 1)
        && surface
        && texture.planes.len() == 1
        && first.is_some_and(|l| l.desc.element_bytes <= 8 || desc.samples > 1);
    let mut placed = vec![];
    let mut end = 0u64;
    let mut alignment = 1u64;
    let mut put = |usage, kind: PlaneKind, size: u64, align: u64, placed: &mut Vec<Placed>| {
        let offset = end.next_multiple_of(align.max(1));
        end = offset + size;
        alignment = alignment.max(align);
        placed.push(Placed { usage, offset, kind });
    };
    if htile {
        let size = HTILE_BLOCK * regions * slices;
        let meta = Meta { size, slice_size: 0, alignment: HTILE_BLOCK, swizzle: 0, dcc: None };
        put(HTILE, PlaneKind::Meta(meta), size, HTILE_BLOCK, &mut placed);
    }
    if empty {
        // Nothing laid out for the data: a depth buffer of a format without
        // depth is all of size 0 (but for `R8_UINT`, the stencil view: a
        // stencil of 1-byte texels, at 0 too), an unrenderable target its
        // metadata alone.
        placed.push(Placed { usage: if depth { DEPTH } else { DATA }, offset: 0, kind: PlaneKind::Empty });
        if depth {
            if desc.format == R8_UINT {
                if let Some(p) = texture.planes.first() {
                    placed.push(Placed { usage: DEPTH + 1, offset: 0, kind: PlaneKind::Data(0) });
                    return (placed, p.layout.size, alignment.max(p.layout.alignment));
                }
            }
            return (placed, 0, alignment);
        }
    }
    let two = texture.planes.len() > 1;
    for (i, p) in texture.planes.iter().enumerate().filter(|_| !empty) {
        let usage = match (depth, two) {
            (true, true) => DEPTH + i as u32,
            (true, false) if htile => DEPTH,
            (_, true) => FIRST_OF_TWO + i as u32,
            _ => DATA,
        };
        put(usage, PlaneKind::Data(i), p.layout.size, p.layout.alignment, &mut placed);
    }
    if target {
        if desc.samples > 1 {
            // A 64 KiB depth-mode surface of 1-byte (2 and 4 samples) or
            // 4-byte (8 samples) elements.
            let bytes = if desc.samples > 4 { 4 } else { 1 };
            let l = Layout::new(&victory_formats::texture::Desc {
                samples: 1,
                element_bytes: bytes,
                block: 1,
                block_height: 1,
                swizzle: Swizzle::SW_64KB_Z_X,
                ..first.expect("a target plane").desc
            });
            if let Ok(l) = l {
                let meta = Meta {
                    size: l.size,
                    slice_size: l.slice_size,
                    alignment: l.alignment,
                    swizzle: Swizzle::SW_64KB_Z_X.0,
                    dcc: None,
                };
                put(FMASK, PlaneKind::Meta(meta), l.size, l.alignment, &mut placed);
            }
        }
        let size = CMASK_BLOCK * regions * slices;
        let meta = Meta { size, slice_size: CMASK_BLOCK * regions, alignment: CMASK_BLOCK, swizzle: 0, dcc: None };
        put(CMASK, PlaneKind::Meta(meta), size, CMASK_BLOCK, &mut placed);
    }
    let compressed = flags & ALLOW_RENDER_TARGET != 0
        && flags & RENDER_XOR != 0
        && desc.dimension == 3
        && texture.planes.len() == 1
        && !empty;
    if let (true, Some(l)) = (compressed, first) {
        let d = dcc(desc, l, mips);
        let size = d.slice_size * slices;
        let meta = Meta { size, slice_size: d.slice_size, alignment: 64 << 10, swizzle: Swizzle::SW_64KB_R_X.0, dcc: Some(d) };
        put(DCC, PlaneKind::Meta(meta), size, 64 << 10, &mut placed);
    }
    (placed, end, alignment)
}

type This = *mut Computer;

fn computer<'a>(this: This) -> &'a Computer {
    // SAFETY: called from computer methods only.
    unsafe { &*this }
}

pub extern "system" fn XGCreateTextureComputer(
    desc: *const ResourceDesc,
    out: *mut This,
) -> HRESULT {
    // SAFETY: the title passes a description and room for a pointer.
    let (Some(&desc), false) = (unsafe { desc.as_ref() }, out.is_null()) else {
        return E_INVALIDARG;
    };
    let Some((texture, swizzle, empty)) = texture(&desc) else {
        tracing::warn!(?desc, "no texture layout");
        return E_INVALIDARG;
    };
    let degenerate = victory_formats::texture::planes(desc.format).is_some_and(|p| p.is_empty());
    // Formats laid out empty have one mip whatever was asked.
    let mips = if degenerate { 1 } else { requested_mips(&desc) };
    tracing::debug!(
        width = desc.width,
        height = desc.height,
        format = desc.format,
        layout = format_args!("{:#x}", desc.layout),
        size = texture.size,
        "texture computer"
    );
    let (planes, size, alignment) = place(&desc, &texture, mips, empty);
    let computer = Box::new(Computer {
        vtable: &VTABLE,
        refs: AtomicU32::new(1),
        desc,
        swizzle,
        mips,
        planes,
        size,
        alignment,
        degenerate,
        bytes: (degenerate || empty)
            .then(|| Texture::new(&TextureDesc { format: R8_UNORM, ..texture_desc(&desc, swizzle)? }).ok())
            .flatten(),
        texture,
    });
    // SAFETY: checked non-null.
    unsafe { out.write(Box::into_raw(computer)) };
    S_OK
}

/// `XG_TEXTURE1D_DESC`, `XG_TEXTURE2D_DESC`, `XG_TEXTURE3D_DESC`: D3D11-style
/// descriptions with the swizzle mode itself (0 linear; no pick).
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct Texture1dDesc {
    pub width: u32,
    pub mips: u32,
    pub array: u32,
    pub format: u32,
    pub usage: u32,
    pub bind_flags: u32,
    pub cpu_access: u32,
    pub misc_flags: u32,
    pub swizzle: u32,
    pub reserved: u32,
}

#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct Texture2dDesc {
    pub width: u32,
    pub height: u32,
    pub mips: u32,
    pub array: u32,
    pub format: u32,
    pub samples: u32,
    pub quality: u32,
    pub usage: u32,
    pub bind_flags: u32,
    pub cpu_access: u32,
    pub misc_flags: u32,
    pub swizzle: u32,
    pub reserved: u32,
}

#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct Texture3dDesc {
    pub width: u32,
    pub height: u32,
    pub depth: u32,
    pub mips: u32,
    pub format: u32,
    pub usage: u32,
    pub bind_flags: u32,
    pub cpu_access: u32,
    pub misc_flags: u32,
    pub swizzle: u32,
    pub reserved: u32,
}

/// D3D11 bind flags.
const BIND_RENDER_TARGET: u32 = 0x20;
const BIND_DEPTH_STENCIL: u32 = 0x40;
const BIND_UNORDERED_ACCESS: u32 = 0x80;

/// The D3D12-style description an XG one stands for: bind flags as use
/// flags, the console's flags (`misc`) as they are, the mode as the layout.
/// `None` for what the library refuses here and not through
/// `XGCreateTextureComputer`: reserved fields set, block-compressed linear
/// textures.
#[allow(clippy::too_many_arguments)]
fn from_xg(
    dimension: u32,
    width: u32,
    height: u32,
    depth: u32,
    mips: u32,
    format: u32,
    samples: u32,
    bind: u32,
    misc: u32,
    swizzle: u32,
    reserved: u32,
) -> Option<ResourceDesc> {
    let compressed = victory_formats::texture::planes(format)
        .and_then(|p| p.first())
        .is_some_and(|f| f.block == 4);
    if reserved != 0 || (compressed && swizzle == 0) || swizzle > 0xff {
        return None;
    }
    let flags = u32::from(bind & BIND_RENDER_TARGET != 0) * ALLOW_RENDER_TARGET
        | u32::from(bind & BIND_DEPTH_STENCIL != 0) * ALLOW_DEPTH_STENCIL
        | u32::from(bind & BIND_UNORDERED_ACCESS != 0) * ALLOW_UNORDERED_ACCESS
        | misc & 0xffff_0000;
    Some(ResourceDesc {
        dimension,
        alignment: 0,
        width: u64::from(width),
        height,
        depth_or_array: u16::try_from(depth).ok()?,
        mips: u16::try_from(mips).ok()?,
        format,
        samples: samples.max(1),
        quality: 0,
        layout: SWIZZLED | swizzle,
        flags,
    })
}

fn create_from(desc: Option<ResourceDesc>, out: *mut This) -> HRESULT {
    match desc {
        Some(d) => XGCreateTextureComputer(&d, out),
        None => E_INVALIDARG,
    }
}

pub extern "system" fn XGCreateTexture1DComputer(desc: *const Texture1dDesc, out: *mut This) -> HRESULT {
    // SAFETY: the caller passes a description or null.
    let Some(d) = (unsafe { desc.as_ref() }) else {
        return E_INVALIDARG;
    };
    create_from(from_xg(TEXTURE_1D, d.width, 1, d.array, d.mips, d.format, 1, d.bind_flags, d.misc_flags, d.swizzle, d.reserved), out)
}

pub extern "system" fn XGCreateTexture2DComputer(desc: *const Texture2dDesc, out: *mut This) -> HRESULT {
    // SAFETY: the caller passes a description or null.
    let Some(d) = (unsafe { desc.as_ref() }) else {
        return E_INVALIDARG;
    };
    create_from(from_xg(3, d.width, d.height, d.array, d.mips, d.format, d.samples, d.bind_flags, d.misc_flags, d.swizzle, d.reserved), out)
}

pub extern "system" fn XGCreateTexture3DComputer(desc: *const Texture3dDesc, out: *mut This) -> HRESULT {
    // SAFETY: the caller passes a description or null.
    let Some(d) = (unsafe { desc.as_ref() }) else {
        return E_INVALIDARG;
    };
    create_from(from_xg(TEXTURE_3D, d.width, d.height, d.depth, d.mips, d.format, 1, d.bind_flags, d.misc_flags, d.swizzle, d.reserved), out)
}

/// `XGComputeTexture*Layout(desc, layout)`: a computer's layout, the
/// computer let go.
fn compute_layout(hr: HRESULT, c: This, out: *mut LayoutOut) -> HRESULT {
    if hr < 0 {
        return hr;
    }
    let hr = layout(c, out);
    release(c);
    hr
}

pub extern "system" fn XGComputeTexture1DLayout(desc: *const Texture1dDesc, out: *mut LayoutOut) -> HRESULT {
    if desc.is_null() || out.is_null() {
        return E_INVALIDARG;
    }
    let mut c = std::ptr::null_mut();
    compute_layout(XGCreateTexture1DComputer(desc, &mut c), c, out)
}

pub extern "system" fn XGComputeTexture2DLayout(desc: *const Texture2dDesc, out: *mut LayoutOut) -> HRESULT {
    if desc.is_null() || out.is_null() {
        return E_INVALIDARG;
    }
    let mut c = std::ptr::null_mut();
    compute_layout(XGCreateTexture2DComputer(desc, &mut c), c, out)
}

pub extern "system" fn XGComputeTexture3DLayout(desc: *const Texture3dDesc, out: *mut LayoutOut) -> HRESULT {
    if desc.is_null() || out.is_null() {
        return E_INVALIDARG;
    }
    let mut c = std::ptr::null_mut();
    compute_layout(XGCreateTexture3DComputer(desc, &mut c), c, out)
}

/// Bits per texel by DXGI format, as `XGComputeOptimalSwizzleMode` sizes
/// textures (0: unknown).
fn bits_per_texel(format: u32) -> u32 {
    match format {
        1..=4 => 128,
        5..=8 => 96,
        9..=22 | 102 => 64,
        23..=47 | 67 | 87..=93 | 100 | 101 | 108 | 109 | 116 | 117 | 189 | 191 => 32,
        104 | 105 => 24,
        48..=59 | 68 | 69 | 85 | 86 | 107 | 114 | 115 | 118..=120 => 16,
        103 | 106 | 110 => 12,
        60..=65 | 73..=78 | 82..=84 | 94..=99 | 113 | 190 => 8,
        70..=72 | 79..=81 | 111 | 112 => 4,
        66 => 1,
        _ => 0,
    }
}

/// D3D11 misc flag: a 64 KiB display texture.
const MISC_DISPLAY: u32 = 0x800_0000;

/// `(dimension, format, width, height, depth or array, samples, bind
/// flags, misc flags)`: the mode the library suggests for a D3D11-style
/// texture (`XG_RESOURCE_DIMENSION`: 1 buffer, 2 1D, 3 2D, 4 3D).
#[allow(clippy::too_many_arguments)]
pub extern "system" fn XGComputeOptimalSwizzleMode(
    dimension: u32,
    format: u32,
    width: u32,
    height: u32,
    _depth: u32,
    samples: u32,
    bind: u32,
    misc: u32,
) -> u32 {
    let target = bind & BIND_RENDER_TARGET != 0;
    let mode = match dimension {
        1 => Swizzle::LINEAR,
        _ if bind & BIND_DEPTH_STENCIL != 0 => Swizzle::SW_64KB_Z_X,
        3 | 4 if misc & MISC_DISPLAY != 0 => Swizzle::SW_64KB_D,
        3 | 4 if target => Swizzle::SW_64KB_R_X,
        3 if bind & BIND_UNORDERED_ACCESS != 0 => {
            let bits = u64::from(bits_per_texel(format)) * u64::from(width) * u64::from(height);
            if samples != 1 || bits == 0 || bits & !7 > 64 << 13 {
                Swizzle::SW_64KB_R_X
            } else {
                Swizzle::SW_4KB_S_X
            }
        }
        3 | 4 => Swizzle::SW_4KB_S,
        _ => Swizzle::LINEAR,
    };
    mode.0
}

/// Writes `64KB_Z_X` through its ninth argument; returns that argument
/// (the library returns it unchanged in the result register).
#[allow(clippy::too_many_arguments)]
pub extern "system" fn XGComputeOptimalDepthStencilSwizzleMode(
    _dimension: u32,
    _format: u32,
    _width: u32,
    _height: u32,
    _depth: u32,
    _mips: u32,
    _samples: u32,
    _bind: u32,
    out: *mut u32,
) -> HRESULT {
    // SAFETY: the caller passes room for the mode or null.
    if let Some(o) = unsafe { out.as_mut() } {
        *o = Swizzle::SW_64KB_Z_X.0;
    }
    out as usize as HRESULT
}

/// `(_, _, mode)`: S_OK for modes 4 to 28 that are multiples of 4, E_FAIL
/// for others, whatever else is passed.
pub extern "system" fn XGValidateDepthStencilSwizzleMode(_a: u32, _b: u32, mode: u32) -> HRESULT {
    if (4..=28).contains(&mode) && mode % 4 == 0 { S_OK } else { E_FAIL }
}

/// Whether the library is initialized and whether exclusive locking is on
/// (and held).
static INITIALIZED: AtomicU32 = AtomicU32::new(0);
static LOCKING: AtomicU32 = AtomicU32::new(0);
/// The library's exclusive lock: whether it is held, and the waiters' bell.
/// Taken and let go by two calls of the title's, so no guard is kept.
static LOCKED: (std::sync::Mutex<bool>, std::sync::Condvar) = (std::sync::Mutex::new(false), std::sync::Condvar::new());

/// 0 if this call initialized the library, 1 if it already was.
pub extern "system" fn XGInitializeLibrary() -> u32 {
    INITIALIZED.swap(1, Ordering::AcqRel)
}

/// 0 if this call ended the library, 1 if it was not initialized.
pub extern "system" fn XGTerminateLibrary() -> u32 {
    u32::from(INITIALIZED.swap(0, Ordering::AcqRel) != 1)
}

/// `(version)`: initializes the library for a console model (4: Series);
/// returns whether it already was. Layouts here are Series X's.
pub extern "system" fn XGSetHardwareVersion(_version: u32) -> u32 {
    INITIALIZED.swap(1, Ordering::AcqRel)
}

/// `(on)`: whether the library takes a lock around its work.
pub extern "system" fn XGEnableExclusiveLocking(on: u32) {
    LOCKING.store(on, Ordering::Release);
}

/// Takes the lock if locking is on: 1 then, else 0.
pub extern "system" fn XGEnterExclusiveLock() -> u32 {
    if LOCKING.load(Ordering::Acquire) == 0 {
        return 0;
    }
    let held = LOCKED.0.lock().unwrap_or_else(|e| e.into_inner());
    let mut held = LOCKED.1.wait_while(held, |held| *held).unwrap_or_else(|e| e.into_inner());
    *held = true;
    1
}

pub extern "system" fn XGLeaveExclusiveLock() {
    *LOCKED.0.lock().unwrap_or_else(|e| e.into_inner()) = false;
    LOCKED.1.notify_one();
}

/// Tile-shape class by DXGI format (measured).
fn tile_class(format: u32) -> usize {
    match format {
        0 => 0,
        1..=4 => 1,
        5..=8 => 2,
        9..=22 | 102 => 3,
        23..=47 | 67 | 87..=93 | 100..=101 | 116..=117 | 189 | 191 => 4,
        48..=59 | 85..=86 | 104 | 114..=115 | 118..=119 => 5,
        60..=66 | 103 | 105..=106 | 108..=113 | 120..=188 | 190 => 6,
        68..=69 | 107 => 7,
        70..=72 | 79..=81 => 8,
        73..=78 | 82..=84 | 94..=99 => 9,
        _ => 6,
    }
}

/// `XGComputeTileShape` answers: result and the two shapes, by class.
const TILE_SHAPES: &[(u32, [u32; 3], [u32; 3])] = &[
    (0x80070057, [7, 7, 7], [7, 7, 7]),
    (0x0, [0, 0, 0], [0, 0, 1]),
    (0x0, [0, 0, 1], [0, 0, 1]),
    (0x0, [0, 0, 0], [65536, 1, 1]),
    (0x80004001, [7, 7, 7], [7, 7, 7]),
    (0x0, [64, 64, 1], [64, 64, 1]),
    (0x0, [0, 128, 1], [128, 128, 1]),
    (0x0, [128, 64, 1], [128, 64, 1]),
    (0x0, [128, 128, 1], [128, 128, 1]),
    (0x0, [256, 128, 1], [256, 128, 1]),
    (0x0, [256, 256, 1], [256, 256, 1]),
    (0x0, [256, 128, 1], [128, 128, 1]),
    (0x0, [512, 256, 1], [128, 64, 1]),
    (0x0, [256, 256, 1], [64, 64, 1]),
    (0x0, [32, 64, 1], [32, 64, 1]),
    (0x0, [0, 128, 1], [64, 128, 1]),
    (0x0, [64, 128, 1], [64, 128, 1]),
    (0x0, [128, 256, 1], [128, 256, 1]),
    (0x0, [128, 128, 1], [64, 128, 1]),
    (0x0, [128, 256, 1], [32, 64, 1]),
    (0x0, [32, 32, 1], [32, 32, 1]),
    (0x0, [0, 64, 1], [64, 64, 1]),
    (0x0, [64, 32, 1], [64, 32, 1]),
    (0x0, [128, 64, 1], [64, 64, 1]),
    (0x0, [256, 128, 1], [64, 32, 1]),
    (0x0, [128, 128, 1], [32, 32, 1]),
    (0x0, [16, 32, 1], [16, 32, 1]),
    (0x0, [0, 64, 1], [32, 64, 1]),
    (0x0, [64, 64, 1], [32, 64, 1]),
    (0x0, [64, 128, 1], [16, 32, 1]),
    (0x0, [0, 0, 0], [256, 256, 1]),
    (0x0, [0, 0, 0], [128, 256, 1]),
    (0x0, [0, 0, 0], [128, 128, 1]),
    (0x0, [0, 0, 0], [64, 128, 1]),
    (0x0, [0, 0, 0], [256, 128, 1]),
    (0x0, [0, 0, 0], [128, 64, 1]),
    (0x0, [0, 0, 0], [64, 64, 1]),
    (0x0, [0, 0, 0], [32, 64, 1]),
];

/// `(dimension, mode argument, samples)` to the answer for each class.
const TILE_SHAPE_INDEX: &[((u32, u32, u32), [u8; 10])] = &[
    ((0, 0, 0), [0, 0, 0, 0, 0, 0, 0, 0, 0, 0]),
    ((0, 0, 1), [1, 2, 2, 2, 2, 2, 2, 2, 2, 2]),
    ((0, 0, 2), [0, 0, 0, 0, 0, 0, 0, 0, 0, 0]),
    ((0, 0, 4), [0, 0, 0, 0, 0, 0, 0, 0, 0, 0]),
    ((0, 0, 8), [0, 0, 0, 0, 0, 0, 0, 0, 0, 0]),
    ((0, 1, 0), [0, 0, 0, 0, 0, 0, 0, 0, 0, 0]),
    ((0, 1, 1), [1, 2, 2, 2, 2, 2, 2, 2, 2, 2]),
    ((0, 1, 2), [0, 0, 0, 0, 0, 0, 0, 0, 0, 0]),
    ((0, 1, 4), [0, 0, 0, 0, 0, 0, 0, 0, 0, 0]),
    ((0, 1, 8), [0, 0, 0, 0, 0, 0, 0, 0, 0, 0]),
    ((0, 2, 0), [0, 0, 0, 0, 0, 0, 0, 0, 0, 0]),
    ((0, 2, 1), [1, 2, 2, 2, 2, 2, 2, 2, 2, 2]),
    ((0, 2, 2), [0, 0, 0, 0, 0, 0, 0, 0, 0, 0]),
    ((0, 2, 4), [0, 0, 0, 0, 0, 0, 0, 0, 0, 0]),
    ((0, 2, 8), [0, 0, 0, 0, 0, 0, 0, 0, 0, 0]),
    ((0, 3, 0), [0, 0, 0, 0, 0, 0, 0, 0, 0, 0]),
    ((0, 3, 1), [1, 2, 2, 2, 2, 2, 2, 2, 2, 2]),
    ((0, 3, 2), [0, 0, 0, 0, 0, 0, 0, 0, 0, 0]),
    ((0, 3, 4), [0, 0, 0, 0, 0, 0, 0, 0, 0, 0]),
    ((0, 3, 8), [0, 0, 0, 0, 0, 0, 0, 0, 0, 0]),
    ((0, 4, 0), [0, 0, 0, 0, 0, 0, 0, 0, 0, 0]),
    ((0, 4, 1), [1, 2, 2, 2, 2, 2, 2, 2, 2, 2]),
    ((0, 4, 2), [0, 0, 0, 0, 0, 0, 0, 0, 0, 0]),
    ((0, 4, 4), [0, 0, 0, 0, 0, 0, 0, 0, 0, 0]),
    ((0, 4, 8), [0, 0, 0, 0, 0, 0, 0, 0, 0, 0]),
    ((0, 5, 0), [0, 0, 0, 0, 0, 0, 0, 0, 0, 0]),
    ((0, 5, 1), [1, 2, 2, 2, 2, 2, 2, 2, 2, 2]),
    ((0, 5, 2), [0, 0, 0, 0, 0, 0, 0, 0, 0, 0]),
    ((0, 5, 4), [0, 0, 0, 0, 0, 0, 0, 0, 0, 0]),
    ((0, 5, 8), [0, 0, 0, 0, 0, 0, 0, 0, 0, 0]),
    ((1, 0, 0), [0, 0, 0, 0, 0, 0, 0, 0, 0, 0]),
    ((1, 0, 1), [3, 0, 0, 0, 0, 0, 0, 0, 0, 0]),
    ((1, 0, 2), [0, 0, 0, 0, 0, 0, 0, 0, 0, 0]),
    ((1, 0, 4), [0, 0, 0, 0, 0, 0, 0, 0, 0, 0]),
    ((1, 0, 8), [0, 0, 0, 0, 0, 0, 0, 0, 0, 0]),
    ((1, 1, 0), [0, 0, 0, 0, 0, 0, 0, 0, 0, 0]),
    ((1, 1, 1), [3, 0, 0, 0, 0, 0, 0, 0, 0, 0]),
    ((1, 1, 2), [0, 0, 0, 0, 0, 0, 0, 0, 0, 0]),
    ((1, 1, 4), [0, 0, 0, 0, 0, 0, 0, 0, 0, 0]),
    ((1, 1, 8), [0, 0, 0, 0, 0, 0, 0, 0, 0, 0]),
    ((1, 2, 0), [0, 0, 0, 0, 0, 0, 0, 0, 0, 0]),
    ((1, 2, 1), [0, 0, 0, 0, 0, 0, 0, 0, 0, 0]),
    ((1, 2, 2), [0, 0, 0, 0, 0, 0, 0, 0, 0, 0]),
    ((1, 2, 4), [0, 0, 0, 0, 0, 0, 0, 0, 0, 0]),
    ((1, 2, 8), [0, 0, 0, 0, 0, 0, 0, 0, 0, 0]),
    ((1, 3, 0), [0, 0, 0, 0, 0, 0, 0, 0, 0, 0]),
    ((1, 3, 1), [0, 0, 0, 0, 0, 0, 0, 0, 0, 0]),
    ((1, 3, 2), [0, 0, 0, 0, 0, 0, 0, 0, 0, 0]),
    ((1, 3, 4), [0, 0, 0, 0, 0, 0, 0, 0, 0, 0]),
    ((1, 3, 8), [0, 0, 0, 0, 0, 0, 0, 0, 0, 0]),
    ((1, 4, 0), [0, 0, 0, 0, 0, 0, 0, 0, 0, 0]),
    ((1, 4, 1), [0, 0, 0, 0, 0, 0, 0, 0, 0, 0]),
    ((1, 4, 2), [0, 0, 0, 0, 0, 0, 0, 0, 0, 0]),
    ((1, 4, 4), [0, 0, 0, 0, 0, 0, 0, 0, 0, 0]),
    ((1, 4, 8), [0, 0, 0, 0, 0, 0, 0, 0, 0, 0]),
    ((1, 5, 0), [0, 0, 0, 0, 0, 0, 0, 0, 0, 0]),
    ((1, 5, 1), [0, 0, 0, 0, 0, 0, 0, 0, 0, 0]),
    ((1, 5, 2), [0, 0, 0, 0, 0, 0, 0, 0, 0, 0]),
    ((1, 5, 4), [0, 0, 0, 0, 0, 0, 0, 0, 0, 0]),
    ((1, 5, 8), [0, 0, 0, 0, 0, 0, 0, 0, 0, 0]),
    ((2, 0, 0), [0, 0, 0, 0, 0, 0, 0, 0, 0, 0]),
    ((2, 0, 1), [4, 4, 4, 4, 4, 4, 4, 4, 4, 4]),
    ((2, 0, 2), [0, 0, 0, 0, 0, 0, 0, 0, 0, 0]),
    ((2, 0, 4), [0, 0, 0, 0, 0, 0, 0, 0, 0, 0]),
    ((2, 0, 8), [0, 0, 0, 0, 0, 0, 0, 0, 0, 0]),
    ((2, 1, 0), [0, 0, 0, 0, 0, 0, 0, 0, 0, 0]),
    ((2, 1, 1), [4, 4, 4, 4, 4, 4, 4, 4, 4, 4]),
    ((2, 1, 2), [0, 0, 0, 0, 0, 0, 0, 0, 0, 0]),
    ((2, 1, 4), [0, 0, 0, 0, 0, 0, 0, 0, 0, 0]),
    ((2, 1, 8), [0, 0, 0, 0, 0, 0, 0, 0, 0, 0]),
    ((2, 2, 0), [0, 0, 0, 0, 0, 0, 0, 0, 0, 0]),
    ((2, 2, 1), [4, 4, 4, 4, 4, 4, 4, 4, 4, 4]),
    ((2, 2, 2), [0, 0, 0, 0, 0, 0, 0, 0, 0, 0]),
    ((2, 2, 4), [0, 0, 0, 0, 0, 0, 0, 0, 0, 0]),
    ((2, 2, 8), [0, 0, 0, 0, 0, 0, 0, 0, 0, 0]),
    ((2, 3, 0), [0, 0, 0, 0, 0, 0, 0, 0, 0, 0]),
    ((2, 3, 1), [4, 4, 4, 4, 4, 4, 4, 4, 4, 4]),
    ((2, 3, 2), [0, 0, 0, 0, 0, 0, 0, 0, 0, 0]),
    ((2, 3, 4), [0, 0, 0, 0, 0, 0, 0, 0, 0, 0]),
    ((2, 3, 8), [0, 0, 0, 0, 0, 0, 0, 0, 0, 0]),
    ((2, 4, 0), [0, 0, 0, 0, 0, 0, 0, 0, 0, 0]),
    ((2, 4, 1), [4, 4, 4, 4, 4, 4, 4, 4, 4, 4]),
    ((2, 4, 2), [0, 0, 0, 0, 0, 0, 0, 0, 0, 0]),
    ((2, 4, 4), [0, 0, 0, 0, 0, 0, 0, 0, 0, 0]),
    ((2, 4, 8), [0, 0, 0, 0, 0, 0, 0, 0, 0, 0]),
    ((2, 5, 0), [0, 0, 0, 0, 0, 0, 0, 0, 0, 0]),
    ((2, 5, 1), [4, 4, 4, 4, 4, 4, 4, 4, 4, 4]),
    ((2, 5, 2), [0, 0, 0, 0, 0, 0, 0, 0, 0, 0]),
    ((2, 5, 4), [0, 0, 0, 0, 0, 0, 0, 0, 0, 0]),
    ((2, 5, 8), [0, 0, 0, 0, 0, 0, 0, 0, 0, 0]),
    ((3, 0, 0), [0, 0, 0, 0, 0, 0, 0, 0, 0, 0]),
    ((3, 0, 1), [0, 5, 6, 7, 8, 9, 10, 11, 12, 13]),
    ((3, 0, 2), [0, 14, 15, 5, 16, 8, 17, 18, 13, 19]),
    ((3, 0, 4), [0, 20, 21, 22, 5, 7, 8, 23, 24, 25]),
    ((3, 0, 8), [0, 26, 27, 20, 14, 5, 16, 28, 25, 29]),
    ((3, 1, 0), [0, 0, 0, 0, 0, 0, 0, 0, 0, 0]),
    ((3, 1, 1), [30, 5, 6, 7, 8, 9, 10, 11, 12, 13]),
    ((3, 1, 2), [31, 14, 15, 5, 16, 8, 17, 18, 13, 19]),
    ((3, 1, 4), [32, 20, 21, 22, 5, 7, 8, 23, 24, 25]),
    ((3, 1, 8), [33, 26, 27, 20, 14, 5, 16, 28, 25, 29]),
    ((3, 2, 0), [0, 0, 0, 0, 0, 0, 0, 0, 0, 0]),
    ((3, 2, 1), [34, 5, 6, 7, 8, 9, 10, 11, 12, 13]),
    ((3, 2, 2), [32, 14, 15, 5, 16, 8, 17, 18, 13, 19]),
    ((3, 2, 4), [35, 20, 21, 22, 5, 7, 8, 23, 24, 25]),
    ((3, 2, 8), [36, 26, 27, 20, 14, 5, 16, 28, 25, 29]),
    ((3, 3, 0), [0, 0, 0, 0, 0, 0, 0, 0, 0, 0]),
    ((3, 3, 1), [0, 5, 6, 7, 8, 9, 10, 11, 12, 13]),
    ((3, 3, 2), [0, 14, 15, 5, 16, 8, 17, 18, 13, 19]),
    ((3, 3, 4), [0, 20, 21, 22, 5, 7, 8, 23, 24, 25]),
    ((3, 3, 8), [0, 26, 27, 20, 14, 5, 16, 28, 25, 29]),
    ((3, 4, 0), [0, 0, 0, 0, 0, 0, 0, 0, 0, 0]),
    ((3, 4, 1), [32, 5, 6, 7, 8, 9, 10, 11, 12, 13]),
    ((3, 4, 2), [33, 14, 15, 5, 16, 8, 17, 18, 13, 19]),
    ((3, 4, 4), [36, 20, 21, 22, 5, 7, 8, 23, 24, 25]),
    ((3, 4, 8), [37, 26, 27, 20, 14, 5, 16, 28, 25, 29]),
    ((3, 5, 0), [0, 0, 0, 0, 0, 0, 0, 0, 0, 0]),
    ((3, 5, 1), [0, 5, 6, 7, 8, 9, 10, 11, 12, 13]),
    ((3, 5, 2), [0, 14, 15, 5, 16, 8, 17, 18, 13, 19]),
    ((3, 5, 4), [0, 20, 21, 22, 5, 7, 8, 23, 24, 25]),
    ((3, 5, 8), [0, 26, 27, 20, 14, 5, 16, 28, 25, 29]),
    ((4, 0, 0), [0, 0, 0, 0, 0, 0, 0, 0, 0, 0]),
    ((4, 0, 1), [4, 4, 4, 4, 4, 4, 4, 4, 4, 4]),
    ((4, 0, 2), [0, 0, 0, 0, 0, 0, 0, 0, 0, 0]),
    ((4, 0, 4), [0, 0, 0, 0, 0, 0, 0, 0, 0, 0]),
    ((4, 0, 8), [0, 0, 0, 0, 0, 0, 0, 0, 0, 0]),
    ((4, 1, 0), [0, 0, 0, 0, 0, 0, 0, 0, 0, 0]),
    ((4, 1, 1), [4, 4, 4, 4, 4, 4, 4, 4, 4, 4]),
    ((4, 1, 2), [0, 0, 0, 0, 0, 0, 0, 0, 0, 0]),
    ((4, 1, 4), [0, 0, 0, 0, 0, 0, 0, 0, 0, 0]),
    ((4, 1, 8), [0, 0, 0, 0, 0, 0, 0, 0, 0, 0]),
    ((4, 2, 0), [0, 0, 0, 0, 0, 0, 0, 0, 0, 0]),
    ((4, 2, 1), [4, 4, 4, 4, 4, 4, 4, 4, 4, 4]),
    ((4, 2, 2), [0, 0, 0, 0, 0, 0, 0, 0, 0, 0]),
    ((4, 2, 4), [0, 0, 0, 0, 0, 0, 0, 0, 0, 0]),
    ((4, 2, 8), [0, 0, 0, 0, 0, 0, 0, 0, 0, 0]),
    ((4, 3, 0), [0, 0, 0, 0, 0, 0, 0, 0, 0, 0]),
    ((4, 3, 1), [4, 4, 4, 4, 4, 4, 4, 4, 4, 4]),
    ((4, 3, 2), [0, 0, 0, 0, 0, 0, 0, 0, 0, 0]),
    ((4, 3, 4), [0, 0, 0, 0, 0, 0, 0, 0, 0, 0]),
    ((4, 3, 8), [0, 0, 0, 0, 0, 0, 0, 0, 0, 0]),
    ((4, 4, 0), [0, 0, 0, 0, 0, 0, 0, 0, 0, 0]),
    ((4, 4, 1), [4, 4, 4, 4, 4, 4, 4, 4, 4, 4]),
    ((4, 4, 2), [0, 0, 0, 0, 0, 0, 0, 0, 0, 0]),
    ((4, 4, 4), [0, 0, 0, 0, 0, 0, 0, 0, 0, 0]),
    ((4, 4, 8), [0, 0, 0, 0, 0, 0, 0, 0, 0, 0]),
    ((4, 5, 0), [0, 0, 0, 0, 0, 0, 0, 0, 0, 0]),
    ((4, 5, 1), [4, 4, 4, 4, 4, 4, 4, 4, 4, 4]),
    ((4, 5, 2), [0, 0, 0, 0, 0, 0, 0, 0, 0, 0]),
    ((4, 5, 4), [0, 0, 0, 0, 0, 0, 0, 0, 0, 0]),
    ((4, 5, 8), [0, 0, 0, 0, 0, 0, 0, 0, 0, 0]),
    ((5, 0, 0), [0, 0, 0, 0, 0, 0, 0, 0, 0, 0]),
    ((5, 0, 1), [1, 2, 2, 2, 2, 2, 2, 2, 2, 2]),
    ((5, 0, 2), [0, 0, 0, 0, 0, 0, 0, 0, 0, 0]),
    ((5, 0, 4), [0, 0, 0, 0, 0, 0, 0, 0, 0, 0]),
    ((5, 0, 8), [0, 0, 0, 0, 0, 0, 0, 0, 0, 0]),
    ((5, 1, 0), [0, 0, 0, 0, 0, 0, 0, 0, 0, 0]),
    ((5, 1, 1), [1, 2, 2, 2, 2, 2, 2, 2, 2, 2]),
    ((5, 1, 2), [0, 0, 0, 0, 0, 0, 0, 0, 0, 0]),
    ((5, 1, 4), [0, 0, 0, 0, 0, 0, 0, 0, 0, 0]),
    ((5, 1, 8), [0, 0, 0, 0, 0, 0, 0, 0, 0, 0]),
    ((5, 2, 0), [0, 0, 0, 0, 0, 0, 0, 0, 0, 0]),
    ((5, 2, 1), [1, 2, 2, 2, 2, 2, 2, 2, 2, 2]),
    ((5, 2, 2), [0, 0, 0, 0, 0, 0, 0, 0, 0, 0]),
    ((5, 2, 4), [0, 0, 0, 0, 0, 0, 0, 0, 0, 0]),
    ((5, 2, 8), [0, 0, 0, 0, 0, 0, 0, 0, 0, 0]),
    ((5, 3, 0), [0, 0, 0, 0, 0, 0, 0, 0, 0, 0]),
    ((5, 3, 1), [1, 2, 2, 2, 2, 2, 2, 2, 2, 2]),
    ((5, 3, 2), [0, 0, 0, 0, 0, 0, 0, 0, 0, 0]),
    ((5, 3, 4), [0, 0, 0, 0, 0, 0, 0, 0, 0, 0]),
    ((5, 3, 8), [0, 0, 0, 0, 0, 0, 0, 0, 0, 0]),
    ((5, 4, 0), [0, 0, 0, 0, 0, 0, 0, 0, 0, 0]),
    ((5, 4, 1), [1, 2, 2, 2, 2, 2, 2, 2, 2, 2]),
    ((5, 4, 2), [0, 0, 0, 0, 0, 0, 0, 0, 0, 0]),
    ((5, 4, 4), [0, 0, 0, 0, 0, 0, 0, 0, 0, 0]),
    ((5, 4, 8), [0, 0, 0, 0, 0, 0, 0, 0, 0, 0]),
    ((5, 5, 0), [0, 0, 0, 0, 0, 0, 0, 0, 0, 0]),
    ((5, 5, 1), [1, 2, 2, 2, 2, 2, 2, 2, 2, 2]),
    ((5, 5, 2), [0, 0, 0, 0, 0, 0, 0, 0, 0, 0]),
    ((5, 5, 4), [0, 0, 0, 0, 0, 0, 0, 0, 0, 0]),
    ((5, 5, 8), [0, 0, 0, 0, 0, 0, 0, 0, 0, 0]),
];

/// `(dimension, format, mode argument, samples, standard tile shape, other
/// shape)`: the library's tile shapes in texels (measured answers; failures
/// leave the outputs as they were).
pub extern "system" fn XGComputeTileShape(
    dimension: u32,
    format: u32,
    mode: u32,
    samples: u32,
    shape: *mut [u32; 3],
    other: *mut [u32; 3],
) -> HRESULT {
    let Some((_, row)) = TILE_SHAPE_INDEX.iter().find(|(k, _)| *k == (dimension, mode, samples)) else {
        return E_INVALIDARG;
    };
    let (hr, a, b) = TILE_SHAPES[usize::from(row[tile_class(format)])];
    if hr == 0 {
        // SAFETY: each output is null or room for a shape.
        unsafe {
            if let Some(o) = shape.as_mut() {
                *o = a;
            }
            if let Some(o) = other.as_mut() {
                *o = b;
            }
        }
    }
    hr as HRESULT
}

/// `XG_BUFFER_DESC`: D3D11-style.
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct BufferDesc {
    pub width: u32,
    pub usage: u32,
    pub bind_flags: u32,
    pub cpu_access: u32,
    pub misc_flags: u32,
    pub stride: u32,
}

/// `(desc, layout)`: a buffer as a one-mip layout of no planes, aligned to
/// 64 KiB with misc bit 26 or 27 (bit 26 needs a whole number of 64 KiB),
/// else 4 bytes. Refuses an empty buffer and bind flags past 0x6ff.
pub extern "system" fn XGComputeBufferLayout(desc: *const BufferDesc, out: *mut LayoutOut) -> HRESULT {
    // SAFETY: the caller passes a description and room for the layout, or null.
    let (Some(d), false) = (unsafe { desc.as_ref() }, out.is_null()) else {
        return E_INVALIDARG;
    };
    if d.width == 0 || d.bind_flags & 0xffff_f900 != 0 || (d.misc_flags & 0x400_0000 != 0 && d.width & 0xffff != 0) {
        return E_INVALIDARG;
    }
    // SAFETY: room for the layout; plain data.
    let out = unsafe {
        out.write_bytes(0, 1);
        &mut *out
    };
    out.size = u64::from(d.width);
    out.alignment = if d.misc_flags & 0xc00_0000 != 0 { 64 << 10 } else { 4 };
    out.mips = 1;
    out.dimension = BUFFER;
    S_OK
}

const R8_UNORM: u32 = 61;
const R8_UINT: u32 = 62;

fn texture_desc(desc: &ResourceDesc, swizzle: Swizzle) -> Option<TextureDesc> {
    Some(TextureDesc {
        dimension: match desc.dimension {
            TEXTURE_1D => Dimension::Tex1d,
            3 => Dimension::Tex2d,
            TEXTURE_3D => Dimension::Tex3d,
            _ => return None,
        },
        width: u32::try_from(desc.width).ok()?,
        height: desc.height,
        depth_or_array: u32::from(desc.depth_or_array),
        mips: u32::from(desc.mips),
        format: desc.format,
        samples: desc.samples.max(1),
        swizzle,
    })
}

/// The mip count a description asks for (0: the whole chain).
fn requested_mips(desc: &ResourceDesc) -> u32 {
    match desc.mips {
        0 => {
            let depth = if desc.dimension == TEXTURE_3D { u64::from(desc.depth_or_array) } else { 1 };
            64 - desc.width.max(u64::from(desc.height)).max(depth).max(1).leading_zeros()
        }
        n => u32::from(n),
    }
}

/// The texture, its mode, and whether its data plane is laid out empty
/// (see [`with_flags`]).
fn texture(desc: &ResourceDesc) -> Option<(Texture, Swizzle, bool)> {
    let swizzle = match desc.layout {
        // The computer's own pick leaves out the render XOR flag.
        0 => suggested(&ResourceDesc { flags: desc.flags & !RENDER_XOR, ..*desc })?,
        ROW_MAJOR => Swizzle::LINEAR,
        l if l & !0xff == SWIZZLED => Swizzle(l & 0xff),
        _ => return None,
    };
    let own = Texture::new(&texture_desc(desc, swizzle)?);
    let (texture, empty) = with_flags(desc, own.ok(), swizzle, requested_mips(desc))?;
    // The layout struct has room for 15 mips.
    texture
        .planes
        .iter()
        .all(|p| p.layout.desc.mips as usize <= MIPS)
        .then_some((texture, swizzle, empty))
}

const MIPS: usize = 15;

/// One mip in the layout the title reads (offsets 0x00, 0x08, 0x10 and
/// 0x1c are read by the title; the rest follow the library's
/// older public shape).
#[repr(C)]
#[derive(Debug, Default, Clone, Copy)]
pub struct MipOut {
    pub size: u64,
    pub offset: u64,
    pub slice_size: u64,
    pub pitch_texels: u32,
    pub pitch_bytes: u32,
    pub alignment: u32,
    /// Width, height in elements, depth or array size.
    pub padded: [u32; 3],
    pub elements: [u32; 2],
    pub texels: [u32; 2],
    pub depth_or_array: u32,
    pub samples: u32,
    pub swizzle: u32,
    /// Padding is spelled out so every byte the title gets is written (the
    /// library writes zeroes).
    pub _pad0: u32,
    pub bank_rotation: [u64; 2],
    pub slice_depth: u32,
    pub in_tail: u32,
    /// Where a tail mip starts in elements: x bits 0-10, y 11-21.
    pub tail_position: u32,
    pub _pad1: u32,
}

#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct PlaneOut {
    /// 1 data, 7 depth, 8 stencil (metadata planes are not made).
    pub usage: u32,
    pub _pad0: u32,
    pub size: u64,
    pub slice_size: u64,
    pub offset: u64,
    pub alignment: u64,
    pub element_bytes: u32,
    pub standard_mips: u32,
    pub tail_mips: u32,
    pub _pad1: u32,
    pub mips: [MipOut; MIPS],
}

#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct LayoutOut {
    pub size: u64,
    pub alignment: u64,
    pub mips: u32,
    pub planes: u32,
    pub plane: [PlaneOut; 4],
    pub dimension: u32,
    pub _pad: u32,
}
const _: () = assert!(size_of::<MipOut>() == 0x70);
const _: () = assert!(size_of::<PlaneOut>() == 0x6c8);
const _: () = assert!(size_of::<LayoutOut>() == 0x1b40);

/// The 64 KiB pipe and bank XOR modes report this in both bank fields.
const XOR_BANKS: u64 = 1 << 15;

fn mip_out(desc: &ResourceDesc, l: &Layout, i: usize) -> MipOut {
    let m = &l.mips[i];
    let bpe = u64::from(l.desc.element_bytes);
    let block = l.desc.block;
    let three_d = desc.dimension == TEXTURE_3D;
    let array = u32::from(desc.depth_or_array).max(1);
    let banks = if matches!(l.desc.swizzle.0, 24..=27) { XOR_BANKS } else { 0 };
    let mut out = MipOut {
        pitch_texels: m.pitch * block,
        pitch_bytes: m.pitch * l.desc.element_bytes,
        padded: [m.pitch, m.rows, if three_d { m.depth_pitch } else { array }],
        elements: [m.width, m.height],
        texels: [
            (l.desc.width >> i).max(1),
            (l.desc.height >> i).max(1),
        ],
        depth_or_array: if three_d { m.depth } else { array },
        samples: l.desc.samples,
        swizzle: l.desc.swizzle.0,
        bank_rotation: [banks; 2],
        slice_depth: l.block[2],
        ..MipOut::default()
    };
    let slice = u64::from(m.pitch) * u64::from(m.rows) * bpe;
    let depth = u64::from(if three_d { m.depth_pitch } else { 1 });
    match m.tail {
        None => {
            out.offset = m.offset;
            out.slice_size = slice * u64::from(l.desc.samples);
            out.size = out.slice_size * depth;
            out.alignment = l.alignment as u32;
        }
        Some(t) => {
            out.in_tail = 1;
            out.alignment = 256;
            out.tail_position = t.x | t.y << 11;
            if l.thick {
                // A 3D tail mip is placed by position, not by offset.
                out.slice_size = l.alignment / u64::from(l.block[2]);
            } else if three_d {
                out.slice_size = l.alignment;
            } else {
                out.offset = u64::from(t.offset);
                out.slice_size = slice;
            }
            out.size = out.slice_size * depth;
        }
    }
    out
}

extern "system" fn add_ref(this: This) -> u32 {
    computer(this).refs.fetch_add(1, Ordering::Relaxed) + 1
}

extern "system" fn release(this: This) -> u32 {
    let left = computer(this).refs.fetch_sub(1, Ordering::AcqRel) - 1;
    if left == 0 {
        // SAFETY: made by `Box::into_raw` in `XGCreateTextureComputer`; the
        // last reference is gone.
        drop(unsafe { Box::from_raw(this) });
    }
    left
}

extern "system" fn layout(this: This, out: *mut LayoutOut) -> HRESULT {
    if out.is_null() {
        return E_INVALIDARG;
    }
    let c = computer(this);
    // SAFETY: the title passes room for the layout; every field is plain
    // data, so zeroes are a valid start.
    let out = unsafe {
        out.write_bytes(0, 1);
        &mut *out
    };
    out.size = c.size;
    out.alignment = c.alignment;
    out.mips = c.mips;
    out.planes = c.planes.len() as u32;
    out.dimension = c.desc.dimension;
    if c.degenerate && c.planes.is_empty() {
        // A format laid out empty: one data plane of nothing.
        out.size = 0;
        out.alignment = 0;
        out.planes = 1;
        out.plane[0].usage = DATA;
        return S_OK;
    }
    for (plane, placed) in out.plane.iter_mut().zip(&c.planes) {
        plane.usage = placed.usage;
        let l = match &placed.kind {
            PlaneKind::Meta(m) => {
                plane.size = m.size;
                plane.slice_size = m.slice_size;
                plane.offset = placed.offset;
                plane.alignment = m.alignment;
                if let Some(d) = &m.dcc {
                    plane.element_bytes = 1;
                    for (i, mip) in plane.mips.iter_mut().take(c.mips as usize).enumerate() {
                        let (size, offset) = d.mips[i];
                        *mip = MipOut {
                            size,
                            offset,
                            alignment: if i == 0 { m.alignment as u32 } else { 0 },
                            padded: [d.block[0], d.block[1], 1],
                            samples: c.desc.samples.max(1),
                            swizzle: m.swizzle,
                            slice_depth: 1,
                            in_tail: u32::from(i >= d.tail),
                            ..MipOut::default()
                        };
                    }
                    continue;
                }
                plane.mips[0].size = m.size;
                plane.mips[0].alignment = m.alignment as u32;
                plane.mips[0].swizzle = m.swizzle;
                continue;
            }
            PlaneKind::Data(i) => &c.texture.planes[*i].layout,
            PlaneKind::Empty => continue,
        };
        if l.size == 0 {
            continue;
        }
        plane.size = l.size;
        plane.slice_size = l.slice_size;
        plane.offset = placed.offset;
        plane.alignment = l.alignment;
        plane.element_bytes = l.desc.element_bytes;
        plane.standard_mips = l.first_tail_mip;
        plane.tail_mips = c.mips - l.first_tail_mip;
        for (i, mip) in plane.mips.iter_mut().take(l.mips.len()).enumerate() {
            *mip = mip_out(&c.desc, l, i);
        }
    }
    S_OK
}

/// A data plane by the library's plane index: its layout and offset.
fn data_plane(c: &Computer, plane: u32) -> Option<(&Layout, u64)> {
    let placed = c.planes.get(plane as usize)?;
    match placed.kind {
        PlaneKind::Data(i) => Some((&c.texture.planes[i].layout, placed.offset)),
        PlaneKind::Meta(_) | PlaneKind::Empty => None,
    }
}

/// The plane, its mip and array slice for a subresource index, and the
/// tiled bytes and the linear bytes the copy touches.
struct CopyArgs<'a> {
    layout: &'a Layout,
    mip: u32,
    slice: u32,
    tiled: usize,
    linear: usize,
}

fn copy_args(
    c: &Computer,
    plane: u32,
    subresource: u32,
    row: u32,
    depth: u32,
) -> Option<CopyArgs<'_>> {
    let (l, offset) = data_plane(c, plane)?;
    let (mip, slice) = (subresource % c.mips, subresource / c.mips);
    let m = l.mips.get(mip as usize)?;
    let bytes = m.width as usize * l.desc.element_bytes as usize;
    let depths = l.subresource_depth(mip) as usize;
    let linear = (depths - 1) * depth as usize + (m.height as usize - 1) * row as usize + bytes;
    Some(CopyArgs {
        layout: l,
        mip,
        slice,
        tiled: (offset + l.size) as usize,
        linear,
    })
}

fn result(r: Result<(), Error>) -> HRESULT {
    match r {
        Ok(()) => S_OK,
        // A layout the library has no addresses for (3D `64KB_D_X` of 8-
        // and 16-byte elements).
        Err(Error::Unsupported) => E_FAIL,
        Err(_) => E_INVALIDARG,
    }
}

/// `(tiled base, plane, subresource, linear, row pitch, depth pitch)`.
extern "system" fn copy_into(
    this: This,
    tiled: *mut u8,
    plane: u32,
    subresource: u32,
    linear: *const u8,
    row: u32,
    depth: u32,
) -> HRESULT {
    let c = computer(this);
    match c.planes.get(plane as usize).map(|p| (&p.kind, p.usage)) {
        // `CMASK` takes copies and keeps nothing.
        Some((PlaneKind::Meta(_), CMASK)) => return S_OK,
        Some((PlaneKind::Meta(_) | PlaneKind::Empty, _)) => return E_FAIL,
        _ => {}
    }
    if data_plane(c, plane).is_some_and(|(l, _)| l.size == 0) {
        // Nothing laid out to copy (the depth of a stencil view format).
        return E_FAIL;
    }
    let Some(a) = copy_args(c, plane, subresource, row, depth) else {
        return E_INVALIDARG;
    };
    if tiled.is_null() || linear.is_null() {
        return E_INVALIDARG;
    }
    let offset = data_plane(c, plane).map_or(0, |(_, o)| o) as usize;
    // SAFETY: the title's memory holds the whole texture and its rows.
    let (tiled, linear) = unsafe {
        (
            std::slice::from_raw_parts_mut(tiled.add(offset), a.tiled - offset),
            std::slice::from_raw_parts(linear, a.linear),
        )
    };
    let r = a
        .layout
        .tile(tiled, a.mip, a.slice, linear, row as usize, depth as usize);
    if r.is_err() {
        tracing::warn!(?r, desc = ?c.desc, subresource, "copy into a texture failed");
    }
    result(r)
}

/// `(linear, plane, subresource, sample, tiled base, row pitch, depth
/// pitch)`.
#[allow(clippy::too_many_arguments)]
extern "system" fn copy_from(
    this: This,
    linear: *mut u8,
    plane: u32,
    subresource: u32,
    _sample: u32,
    tiled: *const u8,
    row: u32,
    depth: u32,
) -> HRESULT {
    let c = computer(this);
    match c.planes.get(plane as usize).map(|p| (&p.kind, p.usage)) {
        // `CMASK` takes copies and keeps nothing.
        Some((PlaneKind::Meta(_), CMASK)) => return S_OK,
        Some((PlaneKind::Meta(_) | PlaneKind::Empty, _)) => return E_FAIL,
        _ => {}
    }
    if data_plane(c, plane).is_some_and(|(l, _)| l.size == 0) {
        // Nothing laid out to copy (the depth of a stencil view format).
        return E_FAIL;
    }
    let Some(a) = copy_args(c, plane, subresource, row, depth) else {
        return E_INVALIDARG;
    };
    if tiled.is_null() || linear.is_null() {
        return E_INVALIDARG;
    }
    let offset = data_plane(c, plane).map_or(0, |(_, o)| o) as usize;
    // SAFETY: the title's memory holds the whole texture and its rows.
    let (tiled, linear) = unsafe {
        (
            std::slice::from_raw_parts(tiled.add(offset), a.tiled - offset),
            std::slice::from_raw_parts_mut(linear, a.linear),
        )
    };
    let r = a
        .layout
        .detile(tiled, a.mip, a.slice, linear, row as usize, depth as usize);
    if r.is_err() {
        tracing::warn!(?r, desc = ?c.desc, subresource, "copy out of a texture failed");
    }
    result(r)
}

#[repr(C)]
pub struct PackedMips {
    pub standard: u8,
    pub packed: u8,
    pub _pad: u16,
    pub tiles: u32,
    pub start: u32,
}

#[repr(C)]
pub struct SubresourceTiling {
    pub width: u32,
    pub height: u16,
    pub depth: u16,
    pub start: u32,
}

const TILE: u64 = 64 << 10;

/// `(total tiles, packed mips, tile shape in texels, in: room / out:
/// count, first, subresource tilings)`, in 64 KiB tiles of plane 0, for 2D
/// 64 KiB modes only (others fail, 1D and 3D are not implemented, as on
/// the console). The mip tail is the packed part, tile 0 of each slice.
extern "system" fn tiling(
    this: This,
    tiles: *mut u32,
    packed: *mut PackedMips,
    shape: *mut [u32; 3],
    count: *mut u32,
    first: u32,
    out: *mut SubresourceTiling,
) -> HRESULT {
    let c = computer(this);
    // 64 KiB modes only, and not with `HTILE` as plane 0.
    if c.swizzle.block_log2() != Some(16) || c.planes.first().is_some_and(|p| matches!(p.kind, PlaneKind::Meta(_))) {
        return E_FAIL;
    }
    let Some(l) = data_plane(c, 0).map(|(l, _)| l).filter(|l| l.size != 0) else {
        // A format laid out empty (or with an empty first plane).
        if let Some(n) = unsafe { count.as_mut() } {
            *n = 0;
        }
        return E_INVALIDARG;
    };
    if l.desc.dimension != Dimension::Tex2d {
        // No tiling for 1D and 3D textures: none given.
        if let Some(n) = unsafe { count.as_mut() } {
            *n = 0;
        }
        return E_NOTIMPL;
    }
    let standard = l.first_tail_mip;
    let tail_tiles = match standard {
        0 => l.slice_size / TILE,
        s if s < c.mips => l.mips[s as usize - 1].offset / TILE,
        _ => 0,
    };
    let block = l.desc.block;
    // SAFETY: each output is null or room for its value; `out` has room
    // for `*count` entries.
    unsafe {
        if let Some(t) = tiles.as_mut() {
            *t = (l.size / TILE) as u32;
        }
        if let Some(p) = packed.as_mut() {
            *p = PackedMips {
                _pad: 0,
                standard: standard as u8,
                packed: (c.mips - standard) as u8,
                tiles: tail_tiles as u32,
                start: 0,
            };
        }
        if let Some(s) = shape.as_mut() {
            *s = [l.block[0] * block, l.block[1] * l.desc.block_height, l.block[2]];
        }
        if let Some(n) = count.as_mut() {
            // By subresource from `first`, as far as there is room: the
            // standard mips of each slice, zeroes for the packed ones; the
            // count of standard ones given back.
            let total = c.mips * l.slices;
            let mut given = 0;
            for i in 0..(*n).min(total.saturating_sub(first)) {
                let (slice, mip) = ((first + i) / c.mips, (first + i) % c.mips);
                let entry = if mip < standard {
                    let m = &l.mips[mip as usize];
                    given += 1;
                    SubresourceTiling {
                        width: m.pitch / l.block[0],
                        height: (m.rows / l.block[1]) as u16,
                        depth: 1,
                        start: ((u64::from(slice) * l.slice_size + m.offset) / TILE) as u32,
                    }
                } else {
                    SubresourceTiling {
                        width: 0,
                        height: 0,
                        depth: 0,
                        start: 0,
                    }
                };
                out.add(i as usize).write(entry);
            }
            *n = given;
        }
    }
    S_OK
}

/// Slot 0x30 `(plane, mip, x, y, z or slice, sample, bit position)`: an
/// element's byte offset from the start of its plane. The title reads
/// texels of a 128x4 `4KB_S` texture on the CPU through it (title+0x5151c90).
/// As the console's library: x and y carry on past the mip, a plane, mip,
/// slice or sample it does not have gives `u64::MAX`.
extern "system" fn element_offset(
    this: This,
    plane: u32,
    mip: u32,
    x: u64,
    y: u32,
    z: u32,
    sample: u32,
    bit: *mut u32,
) -> u64 {
    if !bit.is_null() {
        // SAFETY: the caller passes room for the bit position or null.
        unsafe { bit.write(0) };
    }
    let c = computer(this);
    if let Some(Placed { kind: PlaneKind::Empty, .. }) = c.planes.get(plane as usize) {
        // Nothing laid out, yet the addresses of 1-byte texels.
        let layout = c.bytes.as_ref().and_then(|b| b.planes.first()).map(|p| &p.layout);
        return layout.and_then(|l| l.address(mip, x as u32, y, z, sample).ok()).unwrap_or(u64::MAX);
    }
    if let Some(Placed { usage, kind: PlaneKind::Meta(_), .. }) = c.planes.get(plane as usize) {
        let at = meta_address(c, *usage, mip, x, y, z, sample);
        if let (Some(at), Some(b)) = (at, unsafe { bit.as_mut() }) {
            *b = (at % 8) as u32;
        }
        return at.map_or(u64::MAX, |at| at / 8);
    }
    let layout = match c.bytes.as_ref().filter(|_| c.degenerate) {
        Some(bytes) => bytes.planes.get(plane as usize).map(|p| &p.layout),
        // An empty plane (the depth of a stencil view format) answers with
        // the stencil's addresses, as on the console.
        None => (plane..c.planes.len() as u32)
            .map_while(|i| data_plane(c, i))
            .map(|(l, _)| l)
            .find(|l| l.size != 0),
    };
    let Some(layout) = layout else {
        return u64::MAX;
    };
    let x = u32::try_from(x).unwrap_or(u32::MAX);
    // From the start of the plane.
    match layout.address(mip, x, y, z, sample) {
        Ok(at) => at,
        Err(e) => {
            tracing::warn!(?e, mip, x, y, z, sample, "no address for a texel");
            u64::MAX
        }
    }
}

/// Bit position masks of `CMASK` and `HTILE` (measured; the same for
/// every format and mode): what each of the low 10 bits of x and y and the
/// low bits of the slice flip inside the plane's block for 1024x1024
/// texels of a slice. The higher bits of x and y pick the block, row by
/// row of the texture's width.
const CMASK_X: [u32; 10] = [0, 0, 0, 0x4, 0x1000, 0x4000, 0xa000, 0x820, 0x80, 0x200];
const CMASK_Y: [u32; 10] = [0, 0, 0, 0x8, 0x1800, 0x2000, 0xc010, 0x840, 0x100, 0x400];
const HTILE_X: [u32; 10] = [0, 0, 0, 0x20, 0x1000, 0x4000, 0xa000, 0x900, 0x400, 0x20000];
const HTILE_Y: [u32; 10] = [0, 0, 0, 0x40, 0x1800, 0x2000, 0xc080, 0xa00, 0x10000, 0x40000];
const META_SLICE: [u32; 2] = [0x2000, 0x1000];
/// `DCC`'s, by element size (1 to 16 bytes); its slice bits flip these.
const DCC_X: [[u32; 11]; 5] = [
    [0, 0, 0, 0, 0x1000, 0x4000, 0xa000, 0x810, 0x8040, 0x100, 0x400],
    [0, 0, 0, 0, 0x1000, 0x4000, 0xa000, 0x820, 0x8080, 0x200, 0],
    [0, 0, 0, 0x8, 0x1000, 0x4000, 0xa000, 0x840, 0x8100, 0x400, 0],
    [0, 0, 0, 0x10, 0x1000, 0x4000, 0xa000, 0x880, 0x8200, 0, 0],
    [0, 0, 0x8, 0x8000, 0x1000, 0x4000, 0x2040, 0x900, 0x8400, 0, 0],
];
const DCC_Y: [[u32; 11]; 5] = [
    [0, 0, 0, 0, 0x1800, 0x2000, 0x4008, 0x820, 0x8080, 0x200, 0],
    [0, 0, 0, 0x8, 0x1800, 0x2000, 0x4010, 0x840, 0x8100, 0x400, 0],
    [0, 0, 0, 0x10, 0x1800, 0x2000, 0x4020, 0x880, 0x8200, 0, 0],
    [0, 0, 0x8, 0x20, 0x1800, 0x2000, 0x4040, 0x900, 0x8400, 0, 0],
    [0, 0, 0x10, 0x20, 0x1800, 0x2000, 0x4080, 0xa00, 0x8000, 0, 0],
];
const DCC_SLICE: [u32; 3] = [0x4000, 0x2000, 0x1000];

/// The bit address of a texel's `CMASK` or `HTILE` entry in its plane;
/// `None` for `FMASK` and what the texture does not have.
fn meta_address(c: &Computer, usage: u32, mip: u32, x: u64, y: u32, z: u32, sample: u32) -> Option<u64> {
    let flip = |masks: &[u32], v: u64| masks.iter().enumerate().filter(|&(i, _)| v >> i & 1 != 0).fold(0, |a, (_, m)| a ^ m);
    if z >= u32::from(c.desc.depth_or_array.max(1)) || sample >= c.desc.samples.max(1) {
        return None;
    }
    if usage == DCC {
        let Some(Placed { kind: PlaneKind::Meta(Meta { dcc: Some(d), .. }), .. }) = c.planes.iter().find(|p| p.usage == DCC) else {
            return None;
        };
        let e = c.texture.planes.first()?.layout.desc.element_bytes.trailing_zeros() as usize;
        let &(_, base) = d.mips.get(mip as usize)?;
        // Addresses for single-sample, single-mip textures only.
        if c.desc.samples > 1 || c.mips > 1 {
            return None;
        }
        let row = ((c.desc.width as u32 >> mip).max(1)).div_ceil(d.block[0]);
        let inside = flip(&DCC_X[e], x) ^ flip(&DCC_Y[e], u64::from(y)) ^ flip(&DCC_SLICE, u64::from(z));
        let index = u64::from(y / d.block[1]) * u64::from(row) + x / u64::from(d.block[0]);
        return Some((u64::from(z) * d.slice_size + base + index * DCC_BLOCK) * 8 + u64::from(inside));
    }
    let (xs, ys, block) = match usage {
        CMASK => (&CMASK_X, &CMASK_Y, CMASK_BLOCK),
        HTILE => (&HTILE_X, &HTILE_Y, HTILE_BLOCK),
        _ => return None,
    };
    if mip != 0 {
        return None;
    }
    let inside = flip(xs, x) ^ flip(ys, u64::from(y)) ^ flip(&META_SLICE, u64::from(z));
    let row = c.desc.width.div_ceil(1024);
    let regions = row * u64::from(c.desc.height.div_ceil(1024));
    let index = u64::from(z) * regions + u64::from(y / 1024) * row + x / 1024;
    Some(index * block * 8 + u64::from(inside))
}

/// The layout as slot 0x10 gives it (the library keeps a copy to answer
/// the slots below from).
fn layout_out(this: This) -> Box<LayoutOut> {
    // SAFETY: plain data, filled by `layout`.
    let mut out: Box<LayoutOut> = Box::new(unsafe { std::mem::zeroed() });
    layout(this, &mut *out);
    out
}

/// Slot 0x18: the texture's size.
extern "system" fn size(this: This) -> u64 {
    layout_out(this).size
}

/// Slot 0x20: its base alignment.
extern "system" fn alignment(this: This) -> u64 {
    layout_out(this).alignment
}

/// Slot 0x28 `(plane, mip)`: where the mip starts, `u64::MAX` for a plane
/// or mip the texture does not have.
extern "system" fn mip_offset(this: This, plane: u32, mip: u32) -> u64 {
    let l = layout_out(this);
    if plane >= l.planes || mip >= l.mips {
        return u64::MAX;
    }
    let p = &l.plane[plane as usize];
    p.offset + p.mips[mip as usize].offset
}

/// Slot 0x60: whether the texture is a tiled resource (a 2D texture of one
/// plane in a `_T` mode).
extern "system" fn tiled_resource(this: This) -> u8 {
    let c = computer(this);
    u8::from(c.desc.dimension == 3 && matches!(c.swizzle.0, 17 | 18) && c.texture.planes.len() == 1)
}

/// Slot 0x38 `(offset, plane, mip, x, y, z, sample)`: the element at a
/// byte offset from the texture's start. The library answers for linear
/// layouts only (E_INVALIDARG for tiled ones), for offsets inside a mip of
/// the first slice.
#[allow(clippy::too_many_arguments)]
extern "system" fn element_at(
    this: This,
    offset: u64,
    plane_out: *mut u32,
    mip_out_: *mut u32,
    x_out: *mut u64,
    y_out: *mut u32,
    z_out: *mut u32,
    sample_out: *mut u32,
) -> HRESULT {
    let c = computer(this);
    let l = layout_out(this);
    for (index, p) in l.plane.iter().take(l.planes as usize).enumerate() {
        for (m, mip) in p.mips.iter().take(l.mips as usize).enumerate() {
            let start = p.offset + mip.offset;
            if offset < start || offset >= start + mip.size {
                continue;
            }
            if let Some(Placed { kind: PlaneKind::Meta(_), usage, .. }) = c.planes.get(index) {
                if *usage != HTILE {
                    return E_INVALIDARG;
                }
                // `HTILE`: the plane, the rest zero.
                // SAFETY: each output is null or room for its value.
                unsafe {
                    if let Some(o) = plane_out.as_mut() {
                        *o = index as u32;
                    }
                    for o in [mip_out_, y_out, z_out, sample_out] {
                        if let Some(o) = o.as_mut() {
                            *o = 0;
                        }
                    }
                    if let Some(o) = x_out.as_mut() {
                        *o = 0;
                    }
                }
                return S_OK;
            }
            let linear = matches!(c.planes.get(index).map(|p| &p.kind), Some(PlaneKind::Data(_)))
                && mip.swizzle == Swizzle::LINEAR.0
                && p.element_bytes != 0;
            if !linear {
                return E_INVALIDARG;
            }
            // The library reads the offset (from the texture's start) as
            // if each slice held its chain largest mip first, the way the
            // GPU's address library lays out linear surfaces (every mip at
            // mip 0's pitch), though its own layouts put the smallest
            // first: the mip and position it reports come from that
            // reading, none past a mip's extent.
            let chain = &p.mips[..l.mips as usize];
            let pitch = u64::from(chain[0].pitch_bytes);
            let size = |m: &MipOut| pitch * u64::from(m.padded[1]);
            // 3D slices are the layout's own (each depth slice holds the
            // chain at each mip's pitch).
            let slice: u64 = if c.desc.dimension == TEXTURE_3D {
                p.slice_size
            } else {
                chain.iter().map(size).sum()
            };
            let (z, mut rest) = (offset / slice, offset % slice);
            let mut m = 0;
            while m < chain.len() && rest >= size(&chain[m]) {
                rest -= size(&chain[m]);
                m += 1;
            }
            let Some(mip) = chain.get(m) else {
                return E_INVALIDARG;
            };
            let (y, x) = (rest / pitch, rest % pitch / u64::from(p.element_bytes));
            if y >= u64::from(mip.elements[1]) || x >= u64::from(mip.elements[0]) || z >= u64::from(mip.depth_or_array) {
                return E_INVALIDARG;
            }
            // SAFETY: each output is null or room for its value.
            unsafe {
                if let Some(o) = plane_out.as_mut() {
                    *o = index as u32;
                }
                if let Some(o) = mip_out_.as_mut() {
                    *o = m as u32;
                }
                if let Some(o) = x_out.as_mut() {
                    *o = x;
                }
                if let Some(o) = y_out.as_mut() {
                    *o = y as u32;
                }
                if let Some(o) = z_out.as_mut() {
                    *o = z as u32;
                }
                if let Some(o) = sample_out.as_mut() {
                    *o = 0;
                }
            }
            return S_OK;
        }
    }
    E_INVALIDARG
}

/// The library's address-equation number for a plane: 5 per mode (by
/// element size) in mode order for 2D, from 80 over the 3D modes; none
/// (`u32::MAX`) for linear, 1D, samples and 3D `64KB_D_X` of 8 and 16
/// bytes, which take no numbers (3D `64KB_R_X` follows on at 113).
fn equation_index(c: &Computer, l: &Layout) -> u32 {
    const MODES_2D: [u32; 14] = [1, 2, 5, 6, 9, 10, 17, 18, 21, 22, 24, 25, 26, 27];
    const MODES_3D: [u32; 7] = [5, 9, 17, 21, 24, 25, 26];
    let e = l.desc.element_bytes.trailing_zeros();
    let sw = l.desc.swizzle.0;
    if l.desc.samples > 1 || !l.addressable() {
        return u32::MAX;
    }
    match c.desc.dimension {
        3 => MODES_2D.iter().position(|&m| m == sw).map_or(u32::MAX, |k| 5 * k as u32 + e),
        TEXTURE_3D if sw == Swizzle::SW_64KB_R_X.0 => 113 + e,
        TEXTURE_3D => MODES_3D.iter().position(|&m| m == sw).map_or(u32::MAX, |k| 80 + 5 * k as u32 + e),
        _ => u32::MAX,
    }
}

/// Slot 0x68 `(plane, out [u32; 4])`: the plane's address-equation number
/// and block shape in elements. E_INVALIDARG for a plane without a view
/// descriptor, E_FAIL for metadata.
extern "system" fn plane_info(this: This, plane: u32, out: *mut [u32; 4]) -> HRESULT {
    let c = computer(this);
    // SAFETY: the caller passes room for four values.
    let out = unsafe { &mut *out };
    *out = [u32::MAX, 0, 0, 0];
    if descriptor(c, plane).is_none() {
        return E_INVALIDARG;
    }
    match c.planes.get(plane as usize).map(|p| &p.kind) {
        Some(PlaneKind::Data(i)) => {
            let l = &c.texture.planes[*i].layout;
            *out = [equation_index(c, l), l.block[0], l.block[1], l.block[2]];
            S_OK
        }
        _ => E_FAIL,
    }
}

/// Slot 0x58 `(plane, out [u32; 8])`: the GPU image descriptor the library
/// makes for a plane (the first three), base address 0. E_INVALIDARG for
/// planes without one: `HTILE`, `DCC`, planes of nothing.
extern "system" fn view_descriptor(this: This, plane: u32, out: *mut [u32; 8]) -> HRESULT {
    if out.is_null() || plane >= 3 {
        return E_INVALIDARG;
    }
    match descriptor(computer(this), plane) {
        Some(d) => {
            // SAFETY: checked non-null; room for the descriptor.
            unsafe { out.write(d) };
            S_OK
        }
        None => E_INVALIDARG,
    }
}

/// Image formats, channel selects and BC swizzles by DXGI format, plane
/// usage and context (0 plain, 1 render target, 2 depth buffer with
/// `HTILE`), as the library writes them (measured).
#[rustfmt::skip]
const DESCRIPTOR_FORMATS: &[(u32, u32, u32, u32, u32, u32)] = &[
    (1, 1, 0, 75, 0xfac, 0),
    (1, 1, 1, 77, 0xfac, 0),
    (2, 1, 0, 77, 0xfac, 0),
    (2, 1, 1, 77, 0xfac, 0),
    (3, 1, 0, 75, 0xfac, 0),
    (3, 1, 1, 75, 0xfac, 0),
    (4, 1, 0, 76, 0xfac, 0),
    (4, 1, 1, 76, 0xfac, 0),
    (9, 1, 0, 69, 0xfac, 0),
    (9, 1, 1, 71, 0xfac, 0),
    (10, 1, 0, 71, 0xfac, 0),
    (10, 1, 1, 71, 0xfac, 0),
    (11, 1, 0, 65, 0xfac, 0),
    (11, 1, 1, 65, 0xfac, 0),
    (12, 1, 0, 69, 0xfac, 0),
    (12, 1, 1, 69, 0xfac, 0),
    (13, 1, 0, 66, 0xfac, 0),
    (13, 1, 1, 66, 0xfac, 0),
    (14, 1, 0, 70, 0xfac, 0),
    (14, 1, 1, 70, 0xfac, 0),
    (15, 1, 0, 62, 0x22c, 0),
    (15, 1, 1, 64, 0x22c, 0),
    (16, 1, 0, 64, 0x22c, 0),
    (16, 1, 1, 64, 0x22c, 0),
    (17, 1, 0, 62, 0x22c, 0),
    (17, 1, 1, 62, 0x22c, 0),
    (18, 1, 0, 63, 0x22c, 0),
    (18, 1, 1, 63, 0x22c, 0),
    (19, 5, 0, 20, 0x204, 0),
    (19, 5, 1, 20, 0x204, 0),
    (19, 6, 0, 5, 0x204, 0),
    (19, 6, 1, 5, 0x204, 0),
    (19, 7, 0, 20, 0x204, 0),
    (19, 7, 2, 22, 0x204, 0),
    (19, 8, 0, 5, 0x204, 0),
    (19, 8, 2, 5, 0x004, 0),
    (20, 5, 0, 22, 0x204, 0),
    (20, 5, 1, 22, 0x204, 0),
    (20, 6, 0, 5, 0x220, 0),
    (20, 6, 1, 5, 0x220, 0),
    (20, 7, 0, 22, 0x204, 0),
    (20, 7, 2, 22, 0x204, 0),
    (20, 8, 0, 5, 0x220, 0),
    (20, 8, 2, 5, 0x004, 0),
    (21, 1, 0, 146, 0x204, 0),
    (21, 1, 1, 146, 0x22c, 0),
    (21, 7, 2, 22, 0x204, 0),
    (22, 6, 0, 5, 0x220, 0),
    (22, 6, 1, 5, 0x220, 0),
    (22, 7, 2, 22, 0x204, 0),
    (22, 8, 2, 5, 0x004, 0),
    (23, 1, 0, 54, 0xfac, 0),
    (23, 1, 1, 50, 0xfac, 0),
    (24, 1, 0, 50, 0xfac, 0),
    (24, 1, 1, 50, 0xfac, 0),
    (25, 1, 0, 54, 0xfac, 0),
    (25, 1, 1, 54, 0xfac, 0),
    (26, 1, 0, 36, 0x3ac, 0),
    (26, 1, 1, 36, 0x3ac, 0),
    (27, 1, 0, 60, 0xfac, 0),
    (27, 1, 1, 56, 0xfac, 0),
    (28, 1, 0, 56, 0xfac, 0),
    (28, 1, 1, 56, 0xfac, 0),
    (29, 1, 0, 130, 0xfac, 0),
    (29, 1, 1, 130, 0xfac, 0),
    (30, 1, 0, 60, 0xfac, 0),
    (30, 1, 1, 60, 0xfac, 0),
    (31, 1, 0, 57, 0xfac, 0),
    (31, 1, 1, 57, 0xfac, 0),
    (32, 1, 0, 61, 0xfac, 0),
    (32, 1, 1, 61, 0xfac, 0),
    (33, 1, 0, 27, 0x22c, 0),
    (33, 1, 1, 29, 0x22c, 0),
    (34, 1, 0, 29, 0x22c, 0),
    (34, 1, 1, 29, 0x22c, 0),
    (35, 1, 0, 23, 0x22c, 0),
    (35, 1, 1, 23, 0x22c, 0),
    (36, 1, 0, 27, 0x22c, 0),
    (36, 1, 1, 27, 0x22c, 0),
    (37, 1, 0, 24, 0x22c, 0),
    (37, 1, 1, 24, 0x22c, 0),
    (38, 1, 0, 28, 0x22c, 0),
    (38, 1, 1, 28, 0x22c, 0),
    (39, 1, 0, 20, 0x204, 0),
    (39, 1, 1, 22, 0x204, 0),
    (39, 7, 2, 22, 0x204, 0),
    (40, 1, 0, 22, 0x204, 0),
    (40, 1, 1, 22, 0x204, 0),
    (40, 7, 2, 22, 0x204, 0),
    (41, 1, 0, 22, 0x204, 0),
    (41, 1, 1, 22, 0x204, 0),
    (42, 1, 0, 20, 0x204, 0),
    (42, 1, 1, 20, 0x204, 0),
    (43, 1, 0, 21, 0x204, 0),
    (43, 1, 1, 21, 0x204, 0),
    (44, 5, 0, 142, 0x22c, 0),
    (44, 5, 1, 142, 0x22c, 0),
    (44, 6, 0, 5, 0x204, 0),
    (44, 6, 1, 5, 0x204, 0),
    (44, 7, 0, 142, 0x22c, 0),
    (44, 7, 2, 141, 0x204, 0),
    (44, 8, 0, 5, 0x204, 0),
    (44, 8, 2, 5, 0x004, 0),
    (45, 5, 0, 141, 0x204, 0),
    (45, 5, 1, 141, 0x204, 0),
    (45, 6, 0, 5, 0x204, 0),
    (45, 6, 1, 5, 0x204, 0),
    (45, 7, 0, 141, 0x204, 0),
    (45, 7, 2, 141, 0x204, 0),
    (45, 8, 0, 5, 0x204, 0),
    (45, 8, 2, 5, 0x004, 0),
    (46, 1, 0, 141, 0x204, 0),
    (46, 1, 1, 141, 0x22c, 0),
    (46, 7, 2, 141, 0x204, 0),
    (47, 6, 0, 5, 0x220, 0),
    (47, 6, 1, 5, 0x220, 0),
    (47, 7, 2, 141, 0x204, 0),
    (47, 8, 2, 5, 0x004, 0),
    (48, 1, 0, 18, 0x22c, 0),
    (48, 1, 1, 14, 0x22c, 0),
    (49, 1, 0, 14, 0x22c, 0),
    (49, 1, 1, 14, 0x22c, 0),
    (50, 1, 0, 18, 0x22c, 0),
    (50, 1, 1, 18, 0x22c, 0),
    (51, 1, 0, 15, 0x22c, 0),
    (51, 1, 1, 15, 0x22c, 0),
    (52, 1, 0, 19, 0x22c, 0),
    (52, 1, 1, 19, 0x22c, 0),
    (53, 1, 0, 11, 0x204, 0),
    (53, 1, 1, 13, 0x204, 0),
    (53, 7, 2, 7, 0x204, 0),
    (54, 1, 0, 13, 0x204, 0),
    (54, 1, 1, 13, 0x204, 0),
    (55, 1, 0, 7, 0x204, 0),
    (55, 1, 1, 7, 0x204, 0),
    (55, 7, 2, 7, 0x204, 0),
    (56, 1, 0, 7, 0x204, 0),
    (56, 1, 1, 7, 0x204, 0),
    (57, 1, 0, 11, 0x204, 0),
    (57, 1, 1, 11, 0x204, 0),
    (58, 1, 0, 8, 0x204, 0),
    (58, 1, 1, 8, 0x204, 0),
    (59, 1, 0, 12, 0x204, 0),
    (59, 1, 1, 12, 0x204, 0),
    (60, 1, 0, 5, 0x204, 0),
    (60, 1, 1, 1, 0x204, 0),
    (61, 1, 0, 1, 0x204, 0),
    (61, 1, 1, 1, 0x204, 0),
    (62, 1, 0, 5, 0x204, 0),
    (62, 1, 1, 5, 0x204, 0),
    (62, 8, 2, 5, 0x004, 0),
    (63, 1, 0, 2, 0x204, 0),
    (63, 1, 1, 2, 0x204, 0),
    (64, 1, 0, 6, 0x204, 0),
    (64, 1, 1, 6, 0x204, 0),
    (65, 1, 0, 1, 0x800, 0),
    (65, 1, 1, 1, 0x800, 0),
    (66, 1, 0, 1, 0x204, 0),
    (66, 1, 1, 1, 0x204, 0),
    (67, 1, 0, 132, 0x3ac, 0),
    (67, 1, 1, 132, 0x3ac, 0),
    (68, 1, 0, 147, 0x3ac, 0),
    (69, 1, 0, 151, 0x3ac, 0),
    (70, 1, 0, 169, 0xfac, 0),
    (71, 1, 0, 169, 0xfac, 0),
    (71, 1, 1, 169, 0xfac, 0),
    (72, 1, 0, 170, 0xfac, 0),
    (73, 1, 0, 171, 0xfac, 0),
    (73, 1, 1, 171, 0xfac, 0),
    (74, 1, 0, 171, 0xfac, 0),
    (74, 1, 1, 171, 0xfac, 0),
    (75, 1, 0, 172, 0xfac, 0),
    (75, 1, 1, 172, 0xfac, 0),
    (76, 1, 0, 173, 0xfac, 0),
    (76, 1, 1, 173, 0xfac, 0),
    (77, 1, 0, 173, 0xfac, 0),
    (77, 1, 1, 173, 0xfac, 0),
    (78, 1, 0, 174, 0xfac, 0),
    (78, 1, 1, 174, 0xfac, 0),
    (79, 1, 0, 175, 0x204, 0),
    (80, 1, 0, 175, 0x204, 0),
    (80, 1, 1, 175, 0x204, 0),
    (81, 1, 0, 176, 0x204, 0),
    (82, 1, 0, 177, 0x22c, 0),
    (82, 1, 1, 177, 0x22c, 0),
    (83, 1, 0, 177, 0x22c, 0),
    (83, 1, 1, 177, 0x22c, 0),
    (84, 1, 0, 178, 0x22c, 0),
    (84, 1, 1, 178, 0x22c, 0),
    (85, 1, 0, 133, 0x32e, 0),
    (85, 1, 1, 133, 0x32e, 0),
    (86, 1, 0, 134, 0xf2e, 0),
    (86, 1, 1, 134, 0xf2e, 0),
    (87, 1, 0, 56, 0xf2e, 0),
    (87, 1, 1, 56, 0xf2e, 0),
    (88, 1, 0, 56, 0x32e, 0),
    (88, 1, 1, 56, 0xf2e, 0),
    (89, 1, 0, 50, 0xfac, 0),
    (89, 1, 1, 50, 0xfac, 0),
    (90, 1, 0, 60, 0xf2e, 0),
    (90, 1, 1, 56, 0xf2e, 0),
    (91, 1, 0, 130, 0xf2e, 0),
    (91, 1, 1, 130, 0xf2e, 0),
    (92, 1, 0, 60, 0x32e, 0),
    (92, 1, 1, 56, 0xf2e, 0),
    (93, 1, 0, 130, 0x32e, 0),
    (93, 1, 1, 130, 0xf2e, 0),
    (94, 1, 0, 179, 0x3ac, 0),
    (94, 1, 1, 179, 0x3ac, 0),
    (95, 1, 0, 179, 0x3ac, 0),
    (95, 1, 1, 179, 0x3ac, 0),
    (96, 1, 0, 180, 0x3ac, 0),
    (96, 1, 1, 180, 0x3ac, 0),
    (97, 1, 0, 181, 0xfac, 0),
    (97, 1, 1, 181, 0xfac, 0),
    (98, 1, 0, 181, 0xfac, 0),
    (98, 1, 1, 181, 0xfac, 0),
    (99, 1, 0, 182, 0xfac, 0),
    (99, 1, 1, 182, 0xfac, 0),
    (100, 1, 0, 56, 0xfac, 0),
    (100, 1, 1, 56, 0xfac, 0),
    (101, 1, 0, 50, 0x3ac, 0),
    (101, 1, 1, 50, 0xfac, 0),
    (102, 1, 0, 65, 0x3ac, 0),
    (102, 1, 1, 65, 0xfac, 0),
    (103, 5, 0, 1, 0x204, 0),
    (103, 5, 1, 1, 0x204, 0),
    (103, 6, 0, 14, 0x22c, 0),
    (103, 6, 1, 14, 0x22c, 0),
    (104, 5, 0, 7, 0x204, 0),
    (104, 5, 1, 7, 0x204, 0),
    (104, 6, 0, 23, 0x22c, 0),
    (104, 6, 1, 23, 0x22c, 0),
    (105, 5, 0, 7, 0x204, 0),
    (105, 5, 1, 7, 0x204, 0),
    (105, 6, 0, 23, 0x22c, 0),
    (105, 6, 1, 23, 0x22c, 0),
    (106, 5, 0, 1, 0x204, 0),
    (106, 5, 1, 1, 0x204, 0),
    (106, 6, 0, 14, 0x22c, 0),
    (106, 6, 1, 14, 0x22c, 0),
    (107, 1, 0, 151, 0x3ac, 0),
    (110, 5, 0, 1, 0x204, 0),
    (110, 5, 1, 1, 0x204, 0),
    (110, 6, 0, 14, 0x22c, 0),
    (110, 6, 1, 14, 0x22c, 0),
    (111, 1, 0, 1, 0x204, 0),
    (111, 1, 1, 1, 0x204, 0),
    (112, 1, 0, 1, 0x204, 0),
    (112, 1, 1, 1, 0x204, 0),
    (113, 1, 0, 1, 0x204, 0),
    (113, 1, 1, 1, 0x204, 0),
    (114, 1, 0, 14, 0xa04, 0),
    (114, 1, 1, 14, 0x22c, 0),
    (115, 1, 0, 136, 0xf2e, 0),
    (115, 1, 1, 136, 0xf2e, 0),
    (116, 1, 0, 285, 0xfac, 0),
    (116, 1, 1, 285, 0xfac, 0),
    (117, 1, 0, 131, 0xfac, 0),
    (117, 1, 1, 131, 0xfac, 0),
    (118, 5, 0, 7, 0x204, 0),
    (118, 5, 1, 7, 0x204, 0),
    (118, 6, 0, 5, 0x204, 0),
    (118, 6, 1, 5, 0x204, 0),
    (118, 7, 2, 7, 0x204, 0),
    (118, 8, 2, 5, 0x004, 0),
    (119, 1, 0, 7, 0x204, 0),
    (119, 1, 1, 7, 0x204, 0),
    (119, 7, 2, 7, 0x204, 0),
    (119, 8, 2, 5, 0x004, 0),
    (120, 6, 0, 5, 0x220, 0),
    (120, 6, 1, 5, 0x220, 0),
    (120, 7, 2, 7, 0x204, 0),
    (120, 8, 2, 5, 0x004, 0),
    (189, 1, 0, 51, 0xfac, 0),
    (189, 1, 1, 51, 0xfac, 0),
    (190, 1, 0, 137, 0x22c, 0),
    (191, 1, 0, 141, 0x22c, 0),
    (191, 1, 1, 141, 0x22c, 0),
];

/// GFX10 image types.
const IMAGE_1D_ARRAY: u32 = 12;
const IMAGE_2D_ARRAY: u32 = 13;
const IMAGE_2D_MSAA: u32 = 15;
const IMAGE_3D: u32 = 10;

/// The view descriptor of a plane: GFX10's 8 dwords, with the format,
/// size, mip levels (sample count for multisampled data), mode, type and
/// depth or array size.
fn descriptor(c: &Computer, plane: u32) -> Option<[u32; 8]> {
    if c.degenerate || plane >= 3 {
        return None;
    }
    let placed = c.planes.get(plane as usize)?;
    let (w, h) = (c.desc.width as u32, c.desc.height);
    let depth = u32::from(c.desc.depth_or_array.max(1)) - 1;
    let (format, select, bc, w, h, levels, sw, ty) = match &placed.kind {
        PlaneKind::Data(i) => {
            let l = &c.texture.planes[*i].layout;
            if l.size == 0 {
                return None;
            }
            let ctx = if c.planes.iter().any(|p| p.usage == HTILE) {
                2
            } else {
                u32::from(c.desc.flags & ALLOW_RENDER_TARGET != 0)
            };
            let find = |ctx| {
                DESCRIPTOR_FORMATS
                    .iter()
                    .find(|e| e.0 == c.desc.format && e.1 == placed.usage && e.2 == ctx)
            };
            let &(_, _, _, format, select, bc) = find(ctx).or_else(|| find(0))?;
            let samples = l.desc.samples;
            let (levels, ty) = match c.desc.dimension {
                _ if samples > 1 => (samples.trailing_zeros(), IMAGE_2D_MSAA),
                TEXTURE_1D => (c.mips - 1, IMAGE_1D_ARRAY),
                TEXTURE_3D => (c.mips - 1, IMAGE_3D),
                _ => (c.mips - 1, IMAGE_2D_ARRAY),
            };
            (format, select, bc, l.desc.width, l.desc.height, levels, l.desc.swizzle.0, ty)
        }
        PlaneKind::Meta(_) => {
            // `CMASK` and `FMASK` as 32-bit and per-sample-count surfaces
            // of the texture's size in the depth XOR mode.
            let format = match (placed.usage, c.desc.samples) {
                (CMASK, _) => 5,
                (FMASK, 2) => 159,
                (FMASK, 4) => 161,
                (FMASK, _) => 166,
                _ => return None,
            };
            (format, 0x004, 0, w, h, 0, Swizzle::SW_64KB_Z_X.0, IMAGE_2D_ARRAY)
        }
        PlaneKind::Empty => return None,
    };
    let (w1, h1) = (w - 1, h - 1);
    Some([
        0,
        format << 20 | (w1 & 3) << 30,
        w1 >> 2 | h1 << 14 | 1 << 31,
        select | levels << 16 | sw << 20 | bc << 25 | ty << 28,
        depth,
        0x8040_0000 | levels << 4,
        0,
        0,
    ])
}

#[repr(C)]
struct Vtable([*const (); 14]);
// SAFETY: function addresses only.
unsafe impl Sync for Vtable {}

static VTABLE: Vtable = Vtable([
    add_ref as *const (),   // 0x00
    release as *const (),   // 0x08
    layout as *const (),    // 0x10
    size as *const (),      // 0x18
    alignment as *const (), // 0x20
    mip_offset as *const (), // 0x28
    element_offset as *const (), // 0x30
    element_at as *const (), // 0x38
    copy_into as *const (), // 0x40
    copy_from as *const (), // 0x48
    tiling as *const (),    // 0x50
    view_descriptor as *const (), // 0x58
    tiled_resource as *const (), // 0x60
    plane_info as *const (), // 0x68
]);

pub(crate) fn module() -> HleModule {
    HleModule {
        name: "xg",
        dlls: &["xg_xs.dll"],
        exports: vec![
            hle_export!(XGCreateTextureComputer),
            hle_export!(XGSuggestSwizzleMode),
            hle_export!(XGCreateTexture1DComputer),
            hle_export!(XGCreateTexture2DComputer),
            hle_export!(XGCreateTexture3DComputer),
            hle_export!(XGComputeTexture1DLayout),
            hle_export!(XGComputeTexture2DLayout),
            hle_export!(XGComputeTexture3DLayout),
            hle_export!(XGComputeOptimalSwizzleMode),
            hle_export!(XGComputeOptimalDepthStencilSwizzleMode),
            hle_export!(XGValidateDepthStencilSwizzleMode),
            hle_export!(XGInitializeLibrary),
            hle_export!(XGTerminateLibrary),
            hle_export!(XGSetHardwareVersion),
            hle_export!(XGEnableExclusiveLocking),
            hle_export!(XGEnterExclusiveLock),
            hle_export!(XGLeaveExclusiveLock),
            hle_export!(XGComputeTileShape),
            hle_export!(XGComputeBufferLayout),
        ],
    }
}

#[cfg(test)]
mod reference;

#[cfg(test)]
mod tests {
    use super::*;

    fn desc(width: u64, height: u32, mips: u16, format: u32, layout: u32) -> ResourceDesc {
        ResourceDesc {
            dimension: 3,
            alignment: 0,
            width,
            height,
            depth_or_array: 1,
            mips,
            format,
            samples: 1,
            quality: 0,
            layout,
            flags: 0,
        }
    }

    /// Method `index` of a computer, as the title calls it.
    fn slot<T: Copy>(c: This, index: usize) -> T {
        // SAFETY: the computer is live; T is the slot's function type.
        unsafe { std::mem::transmute_copy(&(*c).vtable.0[index]) }
    }

    fn create(d: &ResourceDesc) -> This {
        let mut c = std::ptr::null_mut();
        assert_eq!(XGCreateTextureComputer(d, &mut c), S_OK);
        c
    }

    /// Element offsets: linear rows of 1 KiB (128 elements); x and y carry
    /// on past the mip, a slice, sample, mip or plane it does not have
    /// gives `u64::MAX`.
    #[test]
    fn element_offsets_follow_the_layout() {
        type Offset = extern "system" fn(This, u32, u32, u64, u32, u32, u32, *mut u32) -> u64;
        let c = create(&desc(128, 4, 1, 11, 0x100));
        let offset: Offset = slot(c, 6);
        let mut bit = 7;
        assert_eq!(offset(c, 0, 0, 3, 1, 0, 0, &mut bit), (128 + 3) * 8);
        assert_eq!(bit, 0);
        assert_eq!(offset(c, 0, 0, 128, 0, 0, 0, std::ptr::null_mut()), 128 * 8);
        for (plane, mip, z, sample) in [(1, 0, 0, 0), (0, 1, 0, 0), (0, 0, 1, 0), (0, 0, 0, 1)] {
            assert_eq!(offset(c, plane, mip, 0, 0, z, sample, &mut bit), u64::MAX);
        }
        slot::<extern "system" fn(This) -> u32>(c, 1)(c);
    }

    #[test]
    fn suggestions_follow_the_use() {
        let mut out = 0;
        let mut d = desc(256, 256, 1, 98, 0);
        assert_eq!(XGSuggestSwizzleMode(&d, 0, &mut out), S_OK);
        assert_eq!(out, 0x105);
        d.flags = ALLOW_RENDER_TARGET;
        XGSuggestSwizzleMode(&d, 0, &mut out);
        assert_eq!(out, 0x11b);
        d.flags = ALLOW_DEPTH_STENCIL;
        XGSuggestSwizzleMode(&d, 0, &mut out);
        assert_eq!(out, 0x118);
        d.flags = ALLOW_DEPTH_STENCIL | ALLOW_RENDER_TARGET;
        assert_eq!(XGSuggestSwizzleMode(&d, 0, &mut out), E_INVALIDARG);
        // Unordered access: 4 KiB XOR up to 64 KiB of mip 0 (256x256 BC7
        // is 64 KiB), the 64 KiB render mode past it.
        d.flags = ALLOW_UNORDERED_ACCESS;
        XGSuggestSwizzleMode(&d, 0, &mut out);
        assert_eq!(out, 0x115);
        d.height = 257;
        XGSuggestSwizzleMode(&d, 0, &mut out);
        assert_eq!(out, 0x11b);
        // 1D is linear but for depth.
        d.dimension = TEXTURE_1D;
        XGSuggestSwizzleMode(&d, 0, &mut out);
        assert_eq!(out, 0x100);
        d.flags = ALLOW_DEPTH_STENCIL;
        XGSuggestSwizzleMode(&d, 0, &mut out);
        assert_eq!(out, 0x118);
    }

    /// The title's calls: layout, then a copy in and out of each mip.
    #[test]
    fn layout_and_copies_through_the_object() {
        // 256x256 BC7, full chain, 64 KiB display mode.
        let c = create(&desc(256, 256, 0, 98, 0x10a));
        // SAFETY: plain data, filled by the call.
        let mut l: Box<LayoutOut> = Box::new(unsafe { std::mem::zeroed() });
        let get: extern "system" fn(This, *mut LayoutOut) -> HRESULT = slot(c, 2);
        assert_eq!(get(c, &mut *l), S_OK);
        assert_eq!(
            (l.size, l.alignment, l.mips, l.planes),
            (131072, 65536, 9, 1)
        );
        let p = &l.plane[0];
        assert_eq!((p.standard_mips, p.tail_mips, p.element_bytes), (1, 8, 16));
        assert_eq!((p.mips[0].offset, p.mips[0].pitch_bytes), (65536, 1024));
        assert_eq!((p.mips[1].offset, p.mips[1].in_tail), (32768, 1));

        type Into = extern "system" fn(This, *mut u8, u32, u32, *const u8, u32, u32) -> HRESULT;
        type From =
            extern "system" fn(This, *mut u8, u32, u32, u32, *const u8, u32, u32) -> HRESULT;
        let (into, from): (Into, From) = (slot(c, 8), slot(c, 9));
        let mut tiled = vec![0u8; l.size as usize];
        for mip in 0..9u32 {
            let m = &p.mips[mip as usize];
            let row = m.elements[0] * 16;
            let rows: Vec<u8> = (0..row * m.elements[1]).map(|i| (i ^ mip) as u8).collect();
            assert_eq!(
                into(c, tiled.as_mut_ptr(), 0, mip, rows.as_ptr(), row, 0),
                S_OK
            );
            let mut back = vec![0u8; rows.len()];
            assert_eq!(
                from(c, back.as_mut_ptr(), 0, mip, 0, tiled.as_ptr(), row, 0),
                S_OK
            );
            assert_eq!(back, rows, "mip {mip}");
        }
        // 0x50: one tile for the tail, one for mip 0.
        type Tiling = extern "system" fn(
            This,
            *mut u32,
            *mut PackedMips,
            *mut [u32; 3],
            *mut u32,
            u32,
            *mut SubresourceTiling,
        ) -> HRESULT;
        let tiling: Tiling = slot(c, 10);
        let (mut tiles, mut shape, mut n) = (0, [0; 3], 4);
        let mut packed = PackedMips {
            _pad: 0,
            standard: 0,
            packed: 0,
            tiles: 0,
            start: 0,
        };
        let mut subs: Vec<SubresourceTiling> = (0..4)
            .map(|_| SubresourceTiling {
                width: 0,
                height: 0,
                depth: 0,
                start: 0,
            })
            .collect();
        assert_eq!(
            tiling(
                c,
                &mut tiles,
                &mut packed,
                &mut shape,
                &mut n,
                0,
                subs.as_mut_ptr()
            ),
            S_OK
        );
        assert_eq!(
            (tiles, packed.standard, packed.packed, packed.tiles),
            (2, 1, 8, 1)
        );
        assert_eq!((shape, n, subs[0].start), ([256, 256, 1], 1, 1));
        assert_eq!(release(c), 0);

        // 4 KiB modes have no 64 KiB tiling.
        let null = std::ptr::null_mut::<u8>();
        let c = create(&desc(256, 256, 0, 98, 0x105));
        assert_eq!(
            tiling(
                c,
                &mut tiles,
                null.cast(),
                null.cast(),
                null.cast(),
                0,
                null.cast()
            ),
            E_FAIL
        );
        assert_eq!(release(c), 0);
    }

    #[test]
    fn bad_descriptions_fail() {
        let mut c = std::ptr::null_mut();
        let bad = desc(256, 256, 1, 0, 0x10a);
        assert_eq!(XGCreateTextureComputer(&bad, &mut c), E_INVALIDARG);
        let bad = desc(256, 256, 1, 28, 0x103);
        assert_eq!(XGCreateTextureComputer(&bad, &mut c), E_INVALIDARG);
        assert!(module().exports.len() == 19);
    }
}
