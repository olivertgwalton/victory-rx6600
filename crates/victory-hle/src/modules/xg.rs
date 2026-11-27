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

#[derive(Debug, Clone, Copy)]
struct Meta {
    size: u64,
    slice_size: u64,
    alignment: u64,
    /// The mode its mip reports (`FMASK` only).
    swizzle: u32,
}

/// Usages the library reports.
const DATA: u32 = 1;
const CMASK: u32 = 2;
const FMASK: u32 = 3;
const HTILE: u32 = 4;
const FIRST_OF_TWO: u32 = 5;
const DEPTH: u32 = 7;

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
        // Laid out as the depth format; other formats keep their own
        // layout (if any) for addresses only.
        return Some(match depth_format(desc.format) {
            Some(format) => (Texture::new(&TextureDesc { format, ..texture_desc(desc, swizzle)? }).ok()?, false),
            None => (texture.unwrap_or(Texture { planes: vec![], size: 0, alignment: 0 }), true),
        });
    }
    let texture = texture?;
    let compressed = victory_formats::texture::planes(desc.format)
        .and_then(|p| p.first())
        .is_some_and(|f| f.block == 4);
    if flags & ALLOW_RENDER_TARGET != 0 && flags & RENDER_XOR == 0 && surface && compressed {
        let small = texture.planes.first().is_some_and(|p| p.layout.desc.element_bytes <= 8);
        return Some((texture, small || desc.samples > 1));
    }
    Some((texture, false))
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
    let htile = depth && surface;
    let first = texture.planes.first().map(|p| &p.layout);
    let target = flags & ALLOW_RENDER_TARGET != 0
        && flags & RENDER_XOR == 0
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
        let meta = Meta { size, slice_size: 0, alignment: HTILE_BLOCK, swizzle: 0 };
        put(HTILE, PlaneKind::Meta(meta), size, HTILE_BLOCK, &mut placed);
    }
    if empty {
        // Nothing laid out for the data: a depth buffer of a format without
        // depth is all of size 0, a compressed target its metadata alone.
        placed.push(Placed { usage: if depth { DEPTH } else { DATA }, offset: 0, kind: PlaneKind::Empty });
        if depth {
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
                let meta = Meta { size: l.size, slice_size: l.slice_size, alignment: l.alignment, swizzle: Swizzle::SW_64KB_Z_X.0 };
                put(FMASK, PlaneKind::Meta(meta), l.size, l.alignment, &mut placed);
            }
        }
        let size = CMASK_BLOCK * regions * slices;
        let meta = Meta { size, slice_size: CMASK_BLOCK * regions, alignment: CMASK_BLOCK, swizzle: 0 };
        put(CMASK, PlaneKind::Meta(meta), size, CMASK_BLOCK, &mut placed);
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
    let mips = requested_mips(&desc);
    tracing::debug!(
        width = desc.width,
        height = desc.height,
        format = desc.format,
        layout = format_args!("{:#x}", desc.layout),
        size = texture.size,
        "texture computer"
    );
    let (planes, size, alignment) = place(&desc, &texture, mips, empty);
    let degenerate = victory_formats::texture::planes(desc.format).is_some_and(|p| p.is_empty());
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
        bytes: degenerate
            .then(|| Texture::new(&TextureDesc { format: R8_UNORM, ..texture_desc(&desc, swizzle)? }).ok())
            .flatten(),
        texture,
    });
    // SAFETY: checked non-null.
    unsafe { out.write(Box::into_raw(computer)) };
    S_OK
}

const R8_UNORM: u32 = 61;

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
    pub bank_rotation: [u64; 2],
    pub slice_depth: u32,
    pub in_tail: u32,
    /// Where a tail mip starts in elements: x bits 0-10, y 11-21.
    pub tail_position: u32,
}

#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct PlaneOut {
    /// 1 data, 7 depth, 8 stencil (metadata planes are not made).
    pub usage: u32,
    pub size: u64,
    pub slice_size: u64,
    pub offset: u64,
    pub alignment: u64,
    pub element_bytes: u32,
    pub standard_mips: u32,
    pub tail_mips: u32,
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
    if c.degenerate {
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
        // Nothing laid out, yet the texture's own addresses.
        return c.texture.planes.first().and_then(|p| p.layout.address(mip, x as u32, y, z, sample).ok()).unwrap_or(u64::MAX);
    }
    if let Some(Placed { usage, kind: PlaneKind::Meta(_), .. }) = c.planes.get(plane as usize) {
        let at = meta_address(c, *usage, mip, x, y, z, sample);
        if let (Some(at), Some(b)) = (at, unsafe { bit.as_mut() }) {
            *b = (at % 8) as u32;
        }
        return at.map_or(u64::MAX, |at| at / 8);
    }
    let layout = match &c.bytes {
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

/// The bit address of a texel's `CMASK` or `HTILE` entry in its plane;
/// `None` for `FMASK` and what the texture does not have.
fn meta_address(c: &Computer, usage: u32, mip: u32, x: u64, y: u32, z: u32, sample: u32) -> Option<u64> {
    let (xs, ys, block) = match usage {
        CMASK => (&CMASK_X, &CMASK_Y, CMASK_BLOCK),
        HTILE => (&HTILE_X, &HTILE_Y, HTILE_BLOCK),
        _ => return None,
    };
    if mip != 0 || z >= u32::from(c.desc.depth_or_array.max(1)) || sample >= c.desc.samples.max(1) {
        return None;
    }
    let flip = |masks: &[u32], v: u64| masks.iter().enumerate().filter(|&(i, _)| v >> i & 1 != 0).fold(0, |a, (_, m)| a ^ m);
    let inside = flip(xs, x) ^ flip(ys, u64::from(y)) ^ flip(&META_SLICE, u64::from(z));
    let row = c.desc.width.div_ceil(1024);
    let regions = row * u64::from(c.desc.height.div_ceil(1024));
    let index = u64::from(z) * regions + u64::from(y / 1024) * row + x / 1024;
    Some(index * block * 8 + u64::from(inside))
}

/// Slots the title does not call: size, alignment, offsets, coordinates,
/// view descriptor, tiled-resource query.
extern "system" fn unused(this: This) -> HRESULT {
    tracing::warn!(desc = ?computer(this).desc, "unimplemented texture computer method");
    E_NOTIMPL
}

#[repr(C)]
struct Vtable([*const (); 13]);
// SAFETY: function addresses only.
unsafe impl Sync for Vtable {}

static VTABLE: Vtable = Vtable([
    add_ref as *const (),   // 0x00
    release as *const (),   // 0x08
    layout as *const (),    // 0x10
    unused as *const (),    // 0x18 size
    unused as *const (),    // 0x20 alignment
    unused as *const (),    // 0x28 mip offset
    element_offset as *const (), // 0x30
    unused as *const (),    // 0x38 element coordinate
    copy_into as *const (), // 0x40
    copy_from as *const (), // 0x48
    tiling as *const (),    // 0x50
    unused as *const (),    // 0x58 view descriptor
    unused as *const (),    // 0x60 tiled resource?
]);

pub(crate) fn module() -> HleModule {
    HleModule {
        name: "xg",
        dlls: &["xg_xs.dll"],
        exports: vec![
            hle_export!(XGCreateTextureComputer),
            hle_export!(XGSuggestSwizzleMode),
        ],
    }
}

#[cfg(test)]
mod dump;

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
        assert!(module().exports.len() == 2);
    }
}
