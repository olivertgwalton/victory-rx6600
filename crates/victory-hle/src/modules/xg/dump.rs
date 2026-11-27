//! Dev tool: the same records as the reference harness (xgh.c), from our
//! implementation. `XG_CASES=<cases.txt> XG_OUT=<out.bin> test exe
//! modules::xg::dump::dump --ignored`.
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

pub(super) fn record(index: u32, d: &ResourceDesc) -> Vec<u8> {
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
    let chr = XGCreateTextureComputer(d, &mut c);
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
        if lhr >= 0 && size != 0 && size < 512 << 20 && d.samples <= 1 && std::env::var_os("XGH_NOCOPY").is_none() {
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

#[test]
#[ignore]
fn dump() {
    let (Ok(cases), Ok(out)) = (std::env::var("XG_CASES"), std::env::var("XG_OUT")) else {
        return;
    };
    let text = std::fs::read_to_string(cases).unwrap();
    let mut bytes = vec![];
    for (i, d) in text.lines().filter_map(parse).enumerate() {
        bytes.extend_from_slice(&record(i as u32, &d));
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
