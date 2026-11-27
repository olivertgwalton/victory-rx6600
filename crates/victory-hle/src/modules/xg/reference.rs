//! Reference records: what the console's library (`xg_xs.dll`) answers
//! for a description, written the way a harness that loads the library
//! under Wine records them, so our answers can be compared with its.
//!
//! The tests check our records against hashes of the library's, kept in
//! `tests/data/xg` (the library's answers only, no part of it), for
//! sweeps of descriptions made here: every format number, every
//! dimension, mode and size class, the use flags, samples, and the
//! metadata planes. `dump` and `probe` (ignored) write records and offset
//! grids for comparing by hand: `XG_CASES=<cases> XG_OUT=<out>`.
use super::*;

const LAYOUT_BYTES: usize = 0x2000;
const TILINGS: usize = 32;
const OFFSETS: usize = 24;
const MAXSUB: usize = 64;

fn pat(plane: u32, sub: u32, i: usize) -> u8 {
    let mut x = (i as u64)
        .wrapping_mul(0x9E3779B97F4A7C15)
        .wrapping_add(u64::from(plane).wrapping_mul(0x632BE59BD9B4E019))
        .wrapping_add(u64::from(sub).wrapping_mul(0x85EBCA77C2B2AE63));
    x ^= x >> 29;
    x = x.wrapping_mul(0xBF58476D1CE4E5B9);
    x ^= x >> 32;
    x as u8
}

fn fnv(p: &[u8]) -> u64 {
    let mut h = 0xcbf29ce484222325u64;
    for w in p.chunks_exact(8) {
        h ^= u64::from_le_bytes(w.try_into().unwrap());
        h = h.wrapping_mul(0x100000001b3);
    }
    h
}

fn slot<T: Copy>(c: This, index: usize) -> T {
    // SAFETY: the computer is live; T is the slot's function type.
    unsafe { std::mem::transmute_copy(&(*c).vtable.0[index]) }
}

fn desc_bytes(d: &ResourceDesc) -> [u8; 56] {
    let mut b = [0u8; 56];
    b[0..4].copy_from_slice(&d.dimension.to_le_bytes());
    b[8..16].copy_from_slice(&d.alignment.to_le_bytes());
    b[16..24].copy_from_slice(&d.width.to_le_bytes());
    b[24..28].copy_from_slice(&d.height.to_le_bytes());
    b[28..30].copy_from_slice(&d.depth_or_array.to_le_bytes());
    b[30..32].copy_from_slice(&d.mips.to_le_bytes());
    b[32..36].copy_from_slice(&d.format.to_le_bytes());
    b[36..40].copy_from_slice(&d.samples.to_le_bytes());
    b[40..44].copy_from_slice(&d.quality.to_le_bytes());
    b[44..48].copy_from_slice(&d.layout.to_le_bytes());
    b[48..52].copy_from_slice(&d.flags.to_le_bytes());
    b
}

pub(super) fn record(index: u32, d: &ResourceDesc, copy_below: u64) -> Vec<u8> {
    record_with(index, d, copy_below, |d, c| XGCreateTextureComputer(d, c))
}

/// A record with the computer made by `create`.
fn record_with(index: u32, d: &ResourceDesc, copy_below: u64, create: impl Fn(&ResourceDesc, *mut This) -> HRESULT) -> Vec<u8> {
    let mut r = Vec::with_capacity(0x2a00);
    let put32 = |r: &mut Vec<u8>, v: u32| r.extend_from_slice(&v.to_le_bytes());
    put32(&mut r, 0x31484758);
    put32(&mut r, index);
    r.extend_from_slice(&desc_bytes(d));
    let mut sug = 0u32;
    let shr = XGSuggestSwizzleMode(d, 0, &mut sug);
    put32(&mut r, shr as u32);
    put32(&mut r, sug);
    let mut c: This = std::ptr::null_mut();
    let chr = create(d, &mut c);
    put32(&mut r, chr as u32);
    let mut layout = vec![0u8; LAYOUT_BYTES];
    let mut lhr = 0;
    let mut tail = vec![0u8; 0];
    if chr >= 0 && !c.is_null() {
        layout.fill(0xcd);
        let get: extern "system" fn(This, *mut u8) -> HRESULT = slot(c, 2);
        lhr = get(c, layout.as_mut_ptr());
        let rd = |o: usize| u32::from_le_bytes(layout[o..o + 4].try_into().unwrap());
        let size = u64::from_le_bytes(layout[0..8].try_into().unwrap());
        let (lmips, planes) = (rd(0x10), rd(0x14));
        // Tiling.
        type Tiling =
            extern "system" fn(This, *mut u32, *mut u8, *mut u32, *mut u32, u32, *mut u8) -> HRESULT;
        let tiling: Tiling = slot(c, 10);
        let (mut tiles, mut packed, mut shape, mut count) = (0u32, [0u8; 12], [0u32; 3], TILINGS as u32);
        let mut subs = vec![0u8; TILINGS * 12];
        let thr = tiling(
            c,
            &mut tiles,
            packed.as_mut_ptr(),
            shape.as_mut_ptr(),
            &mut count,
            0,
            subs.as_mut_ptr(),
        );
        put32(&mut tail, thr as u32);
        put32(&mut tail, tiles);
        tail.extend_from_slice(&packed);
        for s in shape {
            put32(&mut tail, s);
        }
        put32(&mut tail, count);
        tail.extend_from_slice(&subs);
        // Offsets.
        type Offset = extern "system" fn(This, u32, u32, u64, u32, u32, u32, *mut u32) -> u64;
        let off: Offset = slot(c, 6);
        let (w, h, dd) = (d.width as u32, d.height, u32::from(d.depth_or_array));
        let mut offq = [[0u32; 5]; OFFSETS];
        let mut offs = [0u64; OFFSETS];
        let mut bits = [0u32; OFFSETS];
        let mut k = 0;
        let lastmip = lmips.saturating_sub(1);
        let mlist = [0, 1, lastmip / 2, lastmip];
        'p: for p in 0..planes.min(2) {
            for &mm in &mlist {
                if k + 3 > OFFSETS {
                    break 'p;
                }
                if mm >= lmips {
                    continue;
                }
                let mw = (w >> mm).max(1);
                let mh = (h >> mm).max(1);
                let md = if d.dimension == 4 { (dd >> mm).max(1) } else { dd.max(1) };
                let q = [[0, 0, 0], [mw - 1, mh - 1, md - 1], [mw / 2 + u32::from(mw > 2), mh / 3, md / 2]];
                for j in q {
                    offq[k] = [p, mm, j[0], j[1], j[2]];
                    bits[k] = 0xffff_ffff;
                    offs[k] = off(c, p, mm, u64::from(j[0]), j[1], j[2], 0, &mut bits[k]);
                    k += 1;
                }
            }
        }
        for q in offq {
            for v in q {
                put32(&mut tail, v);
            }
        }
        for v in offs {
            tail.extend_from_slice(&v.to_le_bytes());
        }
        for v in bits {
            put32(&mut tail, v);
        }
        // Copies.
        let mut hrs = [0i32; MAXSUB];
        let mut hashes = [0u64; MAXSUB];
        let mut n = 0usize;
        if lhr >= 0 && size != 0 && size < copy_below && d.samples <= 1 {
            type Into = extern "system" fn(This, *mut u8, u32, u32, *const u8, u32, u32) -> HRESULT;
            let into: Into = slot(c, 8);
            let mut tiled = vec![0u8; size as usize];
            let lsize = 16 * w.max(1) as usize * h.max(1) as usize
                * if d.dimension == 4 { dd.max(1) as usize } else { 1 }
                + 64;
            let mut lin = vec![0u8; lsize];
            let base: Vec<u8> = (0..lsize).map(|i| pat(0, 0, i)).collect();
            let arr = if d.dimension == 4 { 1 } else { dd.max(1) };
            'c: for p in 0..planes {
                for s in 0..lmips * arr {
                    if n >= MAXSUB {
                        break 'c;
                    }
                    let mm = s % lmips;
                    let (mw, mh) = ((w >> mm).max(1), (h >> mm).max(1));
                    let row = 16 * mw;
                    let k = (p.wrapping_mul(0x3b).wrapping_add(s.wrapping_mul(0x9d)).wrapping_add(1)) as u8;
                    for (l, b) in lin.iter_mut().zip(&base) {
                        *l = b ^ k;
                    }
                    tiled.fill(0);
                    hrs[n] = into(c, tiled.as_mut_ptr(), p, s, lin.as_ptr(), row, row * mh);
                    hashes[n] = fnv(&tiled);
                    n += 1;
                }
            }
        }
        put32(&mut tail, n as u32);
        for v in hrs {
            put32(&mut tail, v as u32);
        }
        for v in hashes {
            tail.extend_from_slice(&v.to_le_bytes());
        }
        release(c);
    }
    put32(&mut r, lhr as u32);
    if tail.is_empty() {
        // Not created: the harness leaves the rest zero.
        r.extend_from_slice(&vec![0u8; LAYOUT_BYTES]);
        let rest = 4 + 4 + 12 + 12 + 4 + TILINGS * 12 + OFFSETS * 20 + OFFSETS * 8 + OFFSETS * 4 + 4 + MAXSUB * 12;
        r.extend_from_slice(&vec![0u8; rest]);
    } else {
        r.extend_from_slice(&layout);
        r.extend_from_slice(&tail);
    }
    r
}

pub(super) fn parse(line: &str) -> Option<ResourceDesc> {
    let v: Vec<u64> = line.split_whitespace().map(|s| s.parse().ok()).collect::<Option<_>>()?;
    let [dim, w, h, d, mips, fmt, smp, lay, flg] = v[..] else {
        return None;
    };
    Some(ResourceDesc {
        dimension: dim as u32,
        alignment: 0,
        width: w,
        height: h as u32,
        depth_or_array: d as u16,
        mips: mips as u16,
        format: fmt as u32,
        samples: smp as u32,
        quality: 0,
        layout: lay as u32,
        flags: flg as u32,
    })
}

/// The computer's other slots for a description, in the harness's
/// layout: size, alignment, mip offsets of 4 planes by 16 mips, the tiled
/// flag, plane info and view descriptors of 4 planes, and the element at
/// each offset the main record asks for.
pub(super) fn slots_record(index: u32, d: &ResourceDesc) -> Vec<u8> {
    let mut r = Vec::with_capacity(1536);
    let p32 = |r: &mut Vec<u8>, v: u32| r.extend_from_slice(&v.to_le_bytes());
    let p64 = |r: &mut Vec<u8>, v: u64| r.extend_from_slice(&v.to_le_bytes());
    p32(&mut r, 0x3248_4758);
    p32(&mut r, index);
    let mut c: This = std::ptr::null_mut();
    let chr = XGCreateTextureComputer(d, &mut c);
    p32(&mut r, chr as u32);
    if chr < 0 || c.is_null() {
        r.resize(1536, 0);
        return r;
    }
    p64(&mut r, slot::<extern "system" fn(This) -> u64>(c, 3)(c));
    p64(&mut r, slot::<extern "system" fn(This) -> u64>(c, 4)(c));
    p32(&mut r, u32::from(slot::<extern "system" fn(This) -> u8>(c, 12)(c)));
    let mip_offset: extern "system" fn(This, u32, u32) -> u64 = slot(c, 5);
    for p in 0..4 {
        for m in 0..16 {
            p64(&mut r, mip_offset(c, p, m));
        }
    }
    let info: extern "system" fn(This, u32, *mut u8) -> HRESULT = slot(c, 13);
    let srd: extern "system" fn(This, u32, *mut u8) -> HRESULT = slot(c, 11);
    let (mut ih, mut iv, mut sh, mut sv) = ([0i32; 4], [[0u8; 16]; 4], [0i32; 4], [[0u8; 32]; 4]);
    for p in 0..4 {
        ih[p] = info(c, p as u32, iv[p].as_mut_ptr());
        sh[p] = srd(c, p as u32, sv[p].as_mut_ptr());
    }
    for h in ih {
        p32(&mut r, h as u32);
    }
    iv.iter().for_each(|v| r.extend_from_slice(v));
    for h in sh {
        p32(&mut r, h as u32);
    }
    sv.iter().for_each(|v| r.extend_from_slice(v));
    // The element at each offset the main record asks for.
    let mut layout = vec![0u8; LAYOUT_BYTES];
    slot::<extern "system" fn(This, *mut u8) -> HRESULT>(c, 2)(c, layout.as_mut_ptr());
    let rd = |o: usize| u32::from_le_bytes(layout[o..o + 4].try_into().unwrap());
    let (lmips, planes) = (rd(0x10), rd(0x14));
    type Offset = extern "system" fn(This, u32, u32, u64, u32, u32, u32, *mut u32) -> u64;
    type At = extern "system" fn(This, u64, *mut u32, *mut u32, *mut u64, *mut u32, *mut u32, *mut u32) -> HRESULT;
    let (off, at): (Offset, At) = (slot(c, 6), slot(c, 7));
    let (w, h, dd) = (d.width as u32, d.height, u32::from(d.depth_or_array));
    let mut hr = [0i32; OFFSETS];
    let (mut pl, mut mp, mut xs, mut ys, mut zs, mut ss) = ([0u32; OFFSETS], [0u32; OFFSETS], [0u64; OFFSETS], [0u32; OFFSETS], [0u32; OFFSETS], [0u32; OFFSETS]);
    let mut k = 0;
    let lastmip = lmips.saturating_sub(1);
    'p: for p in 0..planes.min(2) {
        for mm in [0, 1, lastmip / 2, lastmip] {
            if k + 3 > OFFSETS {
                break 'p;
            }
            if mm >= lmips {
                continue;
            }
            let (mw, mh) = ((w >> mm).max(1), (h >> mm).max(1));
            let md = if d.dimension == 4 { (dd >> mm).max(1) } else { dd.max(1) };
            for j in [[0, 0, 0], [mw - 1, mh - 1, md - 1], [mw / 2 + u32::from(mw > 2), mh / 3, md / 2]] {
                let mut bit = 0;
                let o = off(c, p, mm, u64::from(j[0]), j[1], j[2], 0, &mut bit);
                let base = 0x18 + p as usize * 0x6c8 + 0x18;
                let poff = u64::from_le_bytes(layout[base..base + 8].try_into().unwrap());
                hr[k] = at(c, o.wrapping_add(poff), &mut pl[k], &mut mp[k], &mut xs[k], &mut ys[k], &mut zs[k], &mut ss[k]);
                k += 1;
            }
        }
    }
    hr.iter().for_each(|&v| p32(&mut r, v as u32));
    pl.iter().chain(&mp).for_each(|&v| p32(&mut r, v));
    xs.iter().for_each(|&v| p64(&mut r, v));
    ys.iter().chain(&zs).chain(&ss).for_each(|&v| p32(&mut r, v));
    release(c);
    debug_assert_eq!(r.len(), 1536);
    r
}

#[test]
#[ignore]
fn dump() {
    let (Ok(cases), Ok(out)) = (std::env::var("XG_CASES"), std::env::var("XG_OUT")) else {
        return;
    };
    let text = std::fs::read_to_string(cases).unwrap();
    let mut bytes = vec![];
    if std::env::var_os("XG_SLOTS").is_some() {
        for (i, d) in text.lines().filter_map(parse).enumerate() {
            bytes.extend_from_slice(&slots_record(i as u32, &d));
        }
        std::fs::write(out, bytes).unwrap();
        return;
    }
    for (i, d) in text.lines().filter_map(parse).enumerate() {
        let below = match (std::env::var_os("XGH_NOCOPY"), std::env::var("XG_COPY_BELOW")) {
            (Some(_), _) => 0,
            (None, Ok(v)) => v.parse().unwrap(),
            _ => 512 << 20,
        };
        bytes.extend_from_slice(&record(i as u32, &d, below));
    }
    std::fs::write(out, bytes).unwrap();
}

/// `XG_PROBE=<queries> XG_OUT=<out>`: the harness's probe mode.
#[test]
#[ignore]
fn probe() {
    let (Ok(queries), Ok(out)) = (std::env::var("XG_PROBE"), std::env::var("XG_OUT")) else {
        return;
    };
    let text = std::fs::read_to_string(queries).unwrap();
    let mut o = String::new();
    for line in text.lines() {
        let v: Vec<u64> = line.split_whitespace().filter_map(|s| s.parse().ok()).collect();
        if v.len() < 16 {
            continue;
        }
        let d = parse(&v[..9].iter().map(u64::to_string).collect::<Vec<_>>().join(" ")).unwrap();
        o.push_str(&format!("# {line}\n"));
        let mut c: This = std::ptr::null_mut();
        let hr = XGCreateTextureComputer(&d, &mut c);
        if hr < 0 {
            o.push_str(&format!("create {:x}\n\n", hr as u32));
            continue;
        }
        type Offset = extern "system" fn(This, u32, u32, u64, u32, u32, u32, *mut u32) -> u64;
        let off: Offset = slot(c, 6);
        let sample = v.get(16).copied().unwrap_or(0) as u32;
        for y in v[14]..v[15] {
            for x in v[12]..v[13] {
                let mut bit = 0;
                let at = off(c, v[9] as u32, v[10] as u32, x, y as u32, v[11] as u32, sample, &mut bit);
                o.push_str(&format!("{x} {y} {at} {bit}\n"));
            }
        }
        o.push('\n');
        release(c);
    }
    std::fs::write(out, o).unwrap();
}

/// Textures copied in the checked records: up to 256 KiB.
const CHECKED_COPIES: u64 = 256 << 10;

/// FNV-1a over a record past its magic and index.
fn record_hash(r: &[u8]) -> u64 {
    r[8..].iter().fold(0xcbf2_9ce4_8422_2325u64, |h, &b| (h ^ u64::from(b)).wrapping_mul(0x100_0000_01b3))
}

fn d(dim: u32, w: u64, h: u32, depth: u16, mips: u16, format: u32, samples: u32, layout: u32, flags: u32) -> ResourceDesc {
    ResourceDesc { dimension: dim, alignment: 0, width: w, height: h, depth_or_array: depth, mips, format, samples, quality: 0, layout, flags }
}

const MODES: [u32; 18] = [
    0, 1, 0x100, 0x101, 0x102, 0x105, 0x106, 0x109, 0x10a, 0x111, 0x112, 0x115, 0x116, 0x117, 0x118, 0x119, 0x11a, 0x11b,
];
/// One format per element size and kind, then the depth formats.
const COLOR: [u32; 16] = [61, 49, 56, 28, 24, 10, 2, 6, 71, 77, 80, 83, 95, 98, 41, 54];
const DEPTH: [u32; 6] = [40, 45, 20, 55, 19, 44];
const RENDER_TARGET_XOR: u32 = ALLOW_RENDER_TARGET | RENDER_XOR;

/// Every format number, one 2D chain in several modes and uses.
fn formats() -> Vec<ResourceDesc> {
    let mut v = vec![];
    for f in 1..192 {
        for lay in [0, 1, 0x105, 0x10a, 0x109, 0x11b, 0x118] {
            v.push(d(3, 256, 256, 1, 0, f, 1, lay, 0));
        }
        v.push(d(3, 256, 256, 1, 1, f, 1, 0, 1));
        v.push(d(3, 256, 256, 1, 1, f, 1, 0, 2));
    }
    v
}

/// Every element size and kind in every mode: 2D sizes with mips and
/// arrays, 1D, 3D.
fn geometry() -> Vec<ResourceDesc> {
    let sizes = [(1, 1), (3, 5), (17, 9), (64, 64), (100, 60), (129, 257), (256, 256), (511, 300), (1024, 1024), (2048, 1), (1, 640), (1920, 1080)];
    let mut v = vec![];
    for f in COLOR.into_iter().chain(DEPTH) {
        for lay in MODES {
            for (w, h) in sizes {
                for mips in [1, 0, 3] {
                    for arr in [1u16, 6, 7] {
                        if w * u64::from(h) * u64::from(arr) > 1 << 21 && arr > 1 {
                            continue;
                        }
                        v.push(d(3, w, h, arr, mips, f, 1, lay, 0));
                    }
                }
            }
            for w in [1, 7, 64, 333, 4096] {
                for mips in [1, 0] {
                    for arr in [1, 3] {
                        v.push(d(2, w, 1, arr, mips, f, 1, lay, 0));
                    }
                }
            }
            for (w, h, depth) in [(16, 16, 16), (33, 17, 9), (128, 128, 64), (5, 3, 70), (64, 64, 1), (1, 1, 32)] {
                for mips in [1, 0] {
                    v.push(d(4, w, h, depth, mips, f, 1, lay, 0));
                }
            }
        }
    }
    v
}

/// The use flags (render target, depth, unordered access, the console's
/// render XOR and others) in the modes they meet, and samples.
fn flags() -> Vec<ResourceDesc> {
    let sizes = [(1, 1), (64, 64), (100, 60), (256, 256), (1920, 1080), (640, 360)];
    let mut v = vec![];
    for f in COLOR.into_iter().chain(DEPTH) {
        for flg in [1, 2, 4, 5, 0x40_0000, 0x40_0001, 0x40_0004, 8, 0x10, 0x20] {
            for lay in [0, 0x100, 0x105, 0x109, 0x10a, 0x118, 0x119, 0x11a, 0x11b] {
                for (w, h) in sizes {
                    for mips in [1, 0] {
                        v.push(d(3, w, h, 1, mips, f, 1, lay, flg));
                    }
                }
                v.push(d(3, 256, 256, 6, 0, f, 1, lay, flg));
                v.push(d(4, 64, 64, 16, 0, f, 1, lay, flg));
            }
        }
    }
    for f in COLOR.into_iter().chain(DEPTH) {
        for samples in [2, 4, 8] {
            for flg in [0, 1, 2] {
                for lay in [0, 0x109, 0x10a, 0x118, 0x11b] {
                    for (w, h) in [(256, 256), (1920, 1080), (17, 9)] {
                        v.push(d(3, w, h, 1, 1, f, samples, lay, flg));
                    }
                }
            }
        }
    }
    v
}

/// `CMASK`, `HTILE` and `FMASK` across sizes, samples, arrays and mips.
fn metadata() -> Vec<ResourceDesc> {
    let sizes = [1, 8, 64, 65, 128, 129, 200, 256, 257, 384, 512, 513, 640, 1024, 1025, 1080, 1920, 2048, 2049, 3840, 4096, 8192];
    let mut v = vec![];
    for w in sizes {
        for h in sizes {
            if w * h > 8192 * 4096 {
                continue;
            }
            v.push(d(3, u64::from(w), h, 1, 1, 61, 1, 0x11b, 1));
            v.push(d(3, u64::from(w), h, 1, 1, 40, 1, 0x118, 2));
            v.push(d(3, u64::from(w), h, 1, 1, 45, 1, 0x118, 2));
        }
    }
    for (w, h) in [(64, 64), (256, 256), (1920, 1080), (1000, 700), (4096, 4096)] {
        for samples in [2, 4, 8] {
            v.push(d(3, w, h, 1, 1, 61, samples, 0x11b, 1));
            v.push(d(3, w, h, 1, 1, 28, samples, 0x11b, 1));
            v.push(d(3, w, h, 1, 1, 10, samples, 0x11b, 1));
            v.push(d(3, w, h, 1, 1, 40, samples, 0x118, 2));
            v.push(d(3, w, h, 1, 1, 55, samples, 0x118, 2));
        }
        for (arr, mips) in [(1, 0), (6, 1), (6, 0)] {
            v.push(d(3, w, h, arr, mips, 40, 1, 0x118, 2));
            v.push(d(3, w, h, arr, mips, 45, 1, 0x118, 2));
            v.push(d(3, w, h, arr, mips, 28, 1, 0x11b, 1));
        }
    }
    v
}

/// `DCC` of render targets with the render XOR flag.
fn dcc() -> Vec<ResourceDesc> {
    let sizes = [1, 64, 256, 257, 512, 1024, 1025, 1920, 2048, 2049, 4096];
    let mut v = vec![];
    for f in [61, 49, 28, 10, 2] {
        for w in sizes {
            for h in sizes {
                if w * h > 4096 * 2048 {
                    continue;
                }
                v.push(d(3, u64::from(w), h, 1, 1, f, 1, 0x11b, RENDER_TARGET_XOR));
            }
        }
        for (w, h) in [(1920, 1080), (1000, 700), (256, 256), (4096, 4096), (64, 64)] {
            for (arr, mips) in [(1, 0), (6, 1), (6, 0), (1, 3)] {
                v.push(d(3, w, h, arr, mips, f, 1, 0x11b, RENDER_TARGET_XOR));
            }
            for samples in [2, 4, 8] {
                v.push(d(3, w, h, 1, 1, f, samples, 0x11b, RENDER_TARGET_XOR));
            }
        }
    }
    v
}

/// 1D and 3D textures of depth-stencil formats, which D3D12 has none of
/// and the library answers inconsistently (plane fields of another plane).
fn excluded(d: &ResourceDesc) -> bool {
    d.dimension != 3 && matches!(d.format, 19 | 20 | 44 | 45)
}

fn suite(name: &str) -> Vec<ResourceDesc> {
    match name {
        "formats" => formats(),
        "geometry" => geometry(),
        "flags" => flags(),
        "metadata" => metadata(),
        "dcc" => dcc(),
        _ => unreachable!(),
    }
}

/// Our records against the library's, case by case.
fn check(name: &str, vectors: &[u8]) {
    let cases = suite(name);
    assert_eq!(cases.len() * 8, vectors.len(), "{name}: cases and vectors");
    let mut bad = vec![];
    for (i, (d, want)) in cases.iter().zip(vectors.chunks_exact(8)).enumerate() {
        if excluded(d) {
            continue;
        }
        let got = record_hash(&record(i as u32, d, CHECKED_COPIES));
        if got != u64::from_le_bytes(want.try_into().unwrap()) {
            bad.push((i, *d));
        }
    }
    assert!(bad.is_empty(), "{name}: {} of {} differ from the library, e.g. {:?}", bad.len(), cases.len(), &bad[..bad.len().min(5)]);
}

/// Two-plane formats, whose second plane's element lookups (slot 0x38)
/// are left out of the checked slots records: the library reads those
/// offsets in a way not reproduced here.
const TWO_PLANES: [u32; 10] = [19, 20, 44, 45, 103, 104, 105, 106, 110, 118];

fn slots_hash(d: &ResourceDesc, r: &mut [u8]) -> u64 {
    const INV: usize = 1536 - 24 * 32;
    if TWO_PLANES.contains(&d.format) {
        for k in 12..24 {
            for (off, n) in [(0, 4), (96, 4), (192, 4), (288, 8), (480, 4), (576, 4), (672, 4)] {
                let o = INV + off + k * n;
                r[o..o + n].fill(0);
            }
        }
    }
    record_hash(r)
}

/// The other slots against the library's.
fn check_slots(name: &str, vectors: &[u8]) {
    let cases = suite(name);
    assert_eq!(cases.len() * 8, vectors.len(), "{name}: cases and vectors");
    let mut bad = vec![];
    for (i, (d, want)) in cases.iter().zip(vectors.chunks_exact(8)).enumerate() {
        if excluded(d) {
            continue;
        }
        let got = slots_hash(d, &mut slots_record(i as u32, d));
        if got != u64::from_le_bytes(want.try_into().unwrap()) {
            bad.push((i, *d));
        }
    }
    assert!(bad.is_empty(), "{name} slots: {} of {} differ from the library, e.g. {:?}", bad.len(), cases.len(), &bad[..bad.len().min(5)]);
}

macro_rules! vectors {
    ($name:literal) => {
        include_bytes!(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/data/xg/", $name, ".hashes"))
    };
}

#[test]
fn every_format_matches_the_library() {
    check("formats", vectors!("formats"));
}

#[test]
fn every_dimension_mode_and_size_matches_the_library() {
    check("geometry", vectors!("geometry"));
}

#[test]
fn use_flags_and_samples_match_the_library() {
    check("flags", vectors!("flags"));
}

#[test]
fn metadata_planes_match_the_library() {
    check("metadata", vectors!("metadata"));
}

#[test]
fn dcc_matches_the_library() {
    check("dcc", vectors!("dcc"));
}

/// `XG_SUITE=<name> XG_OUT=<cases.txt>`: a suite's descriptions as the
/// harness reads them.
#[test]
#[ignore]
fn cases() {
    let (Ok(name), Ok(out)) = (std::env::var("XG_SUITE"), std::env::var("XG_OUT")) else {
        return;
    };
    let text: String = suite(&name)
        .iter()
        .map(|d| {
            format!(
                "{} {} {} {} {} {} {} {} {}\n",
                d.dimension, d.width, d.height, d.depth_or_array, d.mips, d.format, d.samples, d.layout, d.flags
            )
        })
        .collect();
    std::fs::write(out, text).unwrap();
}

macro_rules! slot_vectors {
    ($name:literal) => {
        include_bytes!(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/data/xg/", $name, ".slots.hashes"))
    };
}

#[test]
fn other_slots_match_the_library_for_every_format() {
    check_slots("formats", slot_vectors!("formats"));
}

#[test]
fn other_slots_match_the_library_for_every_dimension_mode_and_size() {
    check_slots("geometry", slot_vectors!("geometry"));
}

#[test]
fn other_slots_match_the_library_for_use_flags_and_samples() {
    check_slots("flags", slot_vectors!("flags"));
}

#[test]
fn other_slots_match_the_library_for_metadata() {
    check_slots("metadata", slot_vectors!("metadata"));
    check_slots("dcc", slot_vectors!("dcc"));
}

/// The computer `XGCreateTexture{1,2,3}DComputer` makes for the texture a
/// D3D12-style description stands for (as the harness builds the XG
/// description: bind flags D3D11-style, the console's flags as misc, the
/// mode itself).
fn create_xg(d: &ResourceDesc, out: *mut This) -> HRESULT {
    let bind = if d.flags & ALLOW_RENDER_TARGET != 0 { BIND_RENDER_TARGET } else { 0 }
        | if d.flags & ALLOW_DEPTH_STENCIL != 0 { BIND_DEPTH_STENCIL } else { 0 }
        | if d.flags & ALLOW_UNORDERED_ACCESS != 0 { BIND_UNORDERED_ACCESS } else { 0 };
    let misc = d.flags & !7 & 0xffff_0000;
    let swizzle = if d.layout == 1 { 0 } else { d.layout & 0xff };
    let (w, h, z, mips) = (d.width as u32, d.height, u32::from(d.depth_or_array), u32::from(d.mips));
    match d.dimension {
        2 => {
            let x = Texture1dDesc { width: w, mips, array: z, format: d.format, usage: 0, bind_flags: bind, cpu_access: 0, misc_flags: misc, swizzle, reserved: 0 };
            XGCreateTexture1DComputer(&x, out)
        }
        4 => {
            let x = Texture3dDesc { width: w, height: h, depth: z, mips, format: d.format, usage: 0, bind_flags: bind, cpu_access: 0, misc_flags: misc, swizzle, reserved: 0 };
            XGCreateTexture3DComputer(&x, out)
        }
        _ => {
            let x = Texture2dDesc { width: w, height: h, mips, array: z, format: d.format, samples: d.samples, quality: 0, usage: 0, bind_flags: bind, cpu_access: 0, misc_flags: misc, swizzle, reserved: 0 };
            XGCreateTexture2DComputer(&x, out)
        }
    }
}

/// Records through the XG descriptions against the library's, for the
/// cases of a suite that name a mode.
fn check_xg(name: &str, vectors: &[u8]) {
    let cases: Vec<ResourceDesc> = suite(name).into_iter().filter(|d| d.layout != 0).collect();
    assert_eq!(cases.len() * 8, vectors.len(), "{name}: cases and vectors");
    let mut bad = vec![];
    for (i, (d, want)) in cases.iter().zip(vectors.chunks_exact(8)).enumerate() {
        if excluded(d) {
            continue;
        }
        let got = record_hash(&record_with(i as u32, d, CHECKED_COPIES, create_xg));
        if got != u64::from_le_bytes(want.try_into().unwrap()) {
            bad.push((i, *d));
        }
    }
    assert!(bad.is_empty(), "{name} XG descriptions: {} of {} differ from the library, e.g. {:?}", bad.len(), cases.len(), &bad[..bad.len().min(5)]);
}

macro_rules! desc_vectors {
    ($name:literal) => {
        include_bytes!(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/data/xg/", $name, ".desc.hashes"))
    };
}

#[test]
fn xg_descriptions_match_the_library() {
    check_xg("formats", desc_vectors!("formats"));
    check_xg("flags", desc_vectors!("flags"));
}

#[test]
fn xg_descriptions_match_the_library_in_every_dimension_mode_and_size() {
    check_xg("geometry", desc_vectors!("geometry"));
}

/// `XGComputeOptimalSwizzleMode` and `XGValidateDepthStencilSwizzleMode`
/// over the sweep the harness ran, against the library's answers (one byte
/// each, in order).
#[test]
fn small_exports_match_the_library() {
    let want = include_bytes!(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/data/xg/small-exports.bytes"));
    let sizes = [(1, 1), (64, 64), (128, 128), (256, 256), (256, 257), (512, 256), (1024, 1024), (4096, 16), (4096, 17), (181, 181), (182, 182), (2048, 2048)];
    let mut got = vec![];
    for dim in 0..=5 {
        for fmt in 0..192 {
            for (s, &(w, h)) in sizes.iter().enumerate() {
                for smp in 1..=2 {
                    for bind in (0..0x100).step_by(0x20) {
                        for misc in [0, 0x800_0000] {
                            if dim != 3 && (s > 1 || smp > 1 || fmt > 2) {
                                continue;
                            }
                            got.push(XGComputeOptimalSwizzleMode(dim, fmt, w, h, 1, smp, bind | 8, misc) as u8);
                        }
                    }
                }
            }
        }
    }
    for a in 0..4 {
        for b in 0..4 {
            for mode in 0..32 {
                got.push(u8::from(XGValidateDepthStencilSwizzleMode(a, b, mode) == S_OK));
            }
        }
    }
    assert_eq!(got.len(), want.len());
    let bad = got.iter().zip(want.iter()).filter(|(a, b)| a != b).count();
    assert_eq!(bad, 0, "{bad} answers differ from the library");
    let mut mode = 0;
    XGComputeOptimalDepthStencilSwizzleMode(3, 40, 256, 256, 1, 1, 1, 0x40, &mut mode);
    assert_eq!(mode, 24);
}

/// `XGComputeTileShape` and `XGComputeBufferLayout` over the harness's
/// sweeps, against one hash of the library's answers in order.
#[test]
fn tile_shapes_and_buffer_layouts_match_the_library() {
    let want: [u64; 2] = {
        let b = include_bytes!(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/data/xg/tiles-buffers.hashes"));
        [u64::from_le_bytes(b[..8].try_into().unwrap()), u64::from_le_bytes(b[8..].try_into().unwrap())]
    };
    let mut h = 0xcbf2_9ce4_8422_2325u64;
    let mut mix = |v: u32| {
        for b in v.to_le_bytes() {
            h = (h ^ u64::from(b)).wrapping_mul(0x100_0000_01b3);
        }
    };
    for dim in 0..=5 {
        for fmt in 0..192 {
            for a in 0..=5 {
                for s in 0..=8u32 {
                    if s > 2 && !s.is_power_of_two() {
                        continue;
                    }
                    let (mut x, mut y) = ([7u32; 3], [7u32; 3]);
                    let hr = XGComputeTileShape(dim, fmt, a, s, &mut x, &mut y);
                    mix(hr as u32);
                    x.iter().chain(&y).for_each(|&v| mix(v));
                }
            }
        }
    }
    let shapes = h;
    let mut h = 0xcbf2_9ce4_8422_2325u64;
    for width in [0, 1, 4, 65535, 65536, 65537, 131072, 1_000_000] {
        for bind in [0, 1, 8, 0x20, 0x40, 0x80, 0x100, 0x200, 0x400, 0x6ff, 0x700, 0x800] {
            for misc in [0, 1, 0x40_0000, 0x400_0000, 0x800_0000, 0xc00_0000] {
                let d = BufferDesc { width, usage: 0, bind_flags: bind, cpu_access: 0, misc_flags: misc, stride: 0 };
                let mut out = vec![0xcdu8; 0x2000];
                let hr = XGComputeBufferLayout(&d, out.as_mut_ptr().cast());
                let lh = out[..0x1b40].iter().fold(0xcbf2_9ce4_8422_2325u64, |a, &b| (a ^ u64::from(b)).wrapping_mul(0x100_0000_01b3));
                for b in (hr as u32).to_le_bytes().into_iter().chain(lh.to_le_bytes()) {
                    h = (h ^ u64::from(b)).wrapping_mul(0x100_0000_01b3);
                }
            }
        }
    }
    assert_eq!([shapes, h], want, "tile shapes, buffer layouts");
}
