//! Device method 0x1b8: a variant of a pipeline the title made with
//! device method 0x1c8, `(base, count, records, iid, out)`. Each record is
//! 0x14c bytes, a kind then a D3D12 state description; the console's driver
//! (GameOS `xgs12_10.dll`, the 0x1b8 constructor at +0x493e0) copies the
//! base's register image and writes the description into it, as it does
//! for a pipeline's own state. The image's context registers are found as
//! [`crate::pipeline`]'s decoder finds them, by CB_COLOR_CONTROL (word `c`);
//! the driver's register block is at `c - 9` words, so its offsets 0x44..0x94
//! are words `c + 8` to `c + 28`.
//!
//! Kinds the title makes: 0 depth-stencil, 1 rasterizer, 2 blend, 5 the
//! depth-stencil extension (title+0x247a0ea..0x247a29f). Kinds 3, 4, 6 and 7
//! are not written here.

/// One record: a kind, then its description.
pub(crate) const RECORD: usize = 0x14c;

/// The word of CB_COLOR_CONTROL in a register image (ROP3 0xcc), with
/// PA_SU_SC_MODE_CNTL nine words on.
pub(crate) fn context_anchor(words: &[u32]) -> Option<usize> {
    (0..words.len().saturating_sub(30)).find(|&i| words[i] & 0xffff_ff8e == 0x00cc_0000 && words[i + 9] & 0xffc0_0000 == 0)
}

/// Words past the anchor of what the driver's block calls `offset`.
const fn at(offset: usize) -> usize {
    (offset - 0x24) / 4
}

/// The driver's D3D12_STENCIL_OP to hardware table (+0xe6830).
const STENCIL_OP: [u32; 9] = [0, 0, 1, 3, 5, 6, 7, 8, 9];
/// Its D3D12_BLEND_OP to hardware table (+0xe6880).
const BLEND_OP: [u32; 6] = [0, 0, 1, 4, 2, 3];

fn u(d: &[u8], o: usize) -> u32 {
    d.get(o..o + 4).map_or(0, |b| u32::from_le_bytes(b.try_into().unwrap()))
}
fn b(d: &[u8], o: usize) -> u32 {
    u32::from(d.get(o).copied().unwrap_or(0))
}

/// Writes `records` into the register image `regs`, as the driver does.
/// `Err(kind)`: a record of a kind not written here (or no image to
/// write into); `regs` is then as it was.
pub(crate) fn apply(regs: &mut [u8], records: &[u8]) -> Result<(), u32> {
    let mut words: Vec<u32> = regs.chunks_exact(4).map(|c| u32::from_le_bytes(c.try_into().unwrap())).collect();
    let c = context_anchor(&words).ok_or(u32::MAX)?;
    // The driver's block offset `o` as a word of the image.
    fn get(w: &[u32], c: usize, o: usize) -> u32 {
        w.get(c + at(o)).copied().unwrap_or(0)
    }
    fn put(w: &mut [u32], c: usize, o: usize, v: u32) {
        if let Some(x) = w.get_mut(c + at(o)) {
            *x = v;
        }
    }
    let w = &mut words;
    for r in records.chunks_exact(RECORD) {
        let kind = u(r, 0);
        // `d` is the description; the driver's offsets below are from the
        // record (description + 4).
        match kind {
            0 => {
                let control = (u(r, 8) << 2 | u(r, 4) << 1)
                    | ((u(r, 0x24) << 8).wrapping_sub(0x100) & 0x700)
                    | ((u(r, 0xc) << 4).wrapping_sub(0x10) & 0x70)
                    | ((u(r, 0x34) << 20).wrapping_sub(0x10_0000) & 0x70_0000)
                    | u(r, 0x10);
                let v = get(w, c, 0x60) & 0xff8f_f888 | control;
                put(w, c, 0x60, v);
                let masks = (b(r, 0x15) << 8 | b(r, 0x14)) << 8;
                let v = get(w, c, 0x68) & 0xff00_00ff | masks;
                put(w, c, 0x68, v);
                let v = get(w, c, 0x6c) & 0xff00_00ff | masks;
                put(w, c, 0x6c, v);
                let op = |o: usize| STENCIL_OP.get(u(r, o) as usize).copied().unwrap_or(0);
                let mut s = op(0x2c);
                for o in [0x30, 0x28, 0x1c, 0x20, 0x18] {
                    s = s << 4 | op(o);
                }
                put(w, c, 0x64, s);
            }
            1 => {
                let fill = u(r, 4);
                let mut mode = ((fill << 8).wrapping_sub(0x100)) | ((fill << 5).wrapping_sub(0x20));
                mode |= if u(r, 0xc) == 0 { 4 } else { 0 };
                mode |= if u(r, 8) == 3 { 2 } else { 0 };
                mode |= u32::from(u(r, 8) == 2);
                mode |= if fill == 2 { 0x120_1808 } else { 0x20_1800 };
                put(w, c, 0x48, mode);
                put(w, c, 0x44, if u(r, 0x1c) == 0 { 0xc08_0000 } else { 0x8_0000 });
                put(w, c, 0x5c, (u(r, 0x10) as i32 as f32).to_bits());
                put(w, c, 0x58, (f32::from_bits(u(r, 0x18)) * 16.0).to_bits());
                put(w, c, 0x54, u(r, 0x14));
                put(w, c, 0x50, (u(r, 0x20) | u(r, 0x24) | 8) << 9);
                put(w, c, 0x4c, if u(r, 0x20) != 0 { 0xb } else if u(r, 0x24) != 0 { 0x10 } else { 8 });
                put(w, c, 0xc8, if u(r, 0x2c) == 1 { 0xe0_0441 } else { 0x10_0000 });
            }
            2 => {
                // D3D12_BLEND_DESC at +4: alpha to coverage, independent
                // blend, then eight 40-byte targets (+0x24 the write mask).
                let d = &r[4..];
                put(w, c, 0x70, match u(d, 0) {
                    0 => 0,
                    2 => 0xaa01,
                    _ => 0x1_8701,
                });
                let target = |n: usize| &d[8 + 40 * n..8 + 40 * (n + 1)];
                let pack = |t: &[u8]| -> u32 {
                    let (enable, logic) = (u(t, 0), u(t, 4));
                    if enable == 0 {
                        return if logic == 0 { 0x8000_0000 } else { 0 };
                    }
                    let (op, aop) = (u(t, 0x10), u(t, 0x1c));
                    let colour = if op >= 4 { 0x101 } else { (u(t, 0xc) << 8).wrapping_sub(0x100) | u(t, 8).wrapping_sub(1) };
                    let alpha = if aop >= 4 { 0x101_0000 } else { (u(t, 0x18) << 24).wrapping_sub(0x100_0000) | (u(t, 0x14) << 16).wrapping_sub(0x1_0000) };
                    let table = |o: u32| BLEND_OP.get(o as usize).copied().unwrap_or(0);
                    let base = if logic == 0 { 0x500 } else { 0x100 };
                    (((base | table(aop)) << 16 | table(op)) << 5) | enable << 30 | alpha | colour
                };
                // Dual-source factors in target 0 (SRC1_COLOR..INV_SRC1_ALPHA).
                let t0 = target(0);
                let dual = [8, 0xc, 0x14, 0x18].iter().any(|&o| u(t0, o).wrapping_sub(0x10) <= 3);
                let independent = u(d, 4) != 0;
                for n in 0..8 {
                    put(w, c, 0x78 + 4 * n, pack(if independent { target(n) } else { t0 }));
                }
                let mut mask = 0;
                for n in (0..8).rev() {
                    let m = if n > 0 && dual { 0 } else { b(if independent { target(n) } else { t0 }, 0x24) };
                    mask = mask << 4 | m;
                }
                put(w, c, 0x74, mask);
            }
            5 => {
                let control = ((u(r, 0x10) << 1 | u(r, 0xc)) << 30)
                    | if u(r, 4) == 0 { 0x80 } else { 0 }
                    | if u(r, 8) == 0 { 8 } else { 0 };
                let v = get(w, c, 0x60) & 0x3fff_ff77 | control;
                put(w, c, 0x60, v);
                let v = get(w, c, 0x68) & 0x00ff_ff00 | b(r, 0x16) | b(r, 0x18) << 24;
                put(w, c, 0x68, v);
                put(w, c, 0x6c, b(r, 0x19) << 24 | b(r, 0x15) << 16 | b(r, 0x14) << 8 | b(r, 0x17));
            }
            other => return Err(other),
        }
    }
    for (chunk, x) in regs.chunks_exact_mut(4).zip(&words) {
        chunk.copy_from_slice(&x.to_le_bytes());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A register image with CB_COLOR_CONTROL at word 4.
    fn image() -> Vec<u8> {
        let mut w = vec![0u32; 48];
        w[4] = 0x00cc_0010;
        w[4 + 15] = 0x0000_0088; // depth bounds and backface bits, kept
        w.iter().flat_map(|x| x.to_le_bytes()).collect()
    }
    fn word(regs: &[u8], i: usize) -> u32 {
        u32::from_le_bytes(regs[4 * i..4 * i + 4].try_into().unwrap())
    }
    fn record(kind: u32, fields: &[(usize, u32)]) -> Vec<u8> {
        let mut r = vec![0u8; RECORD];
        r[..4].copy_from_slice(&kind.to_le_bytes());
        for &(o, v) in fields {
            r[o..o + 4].copy_from_slice(&v.to_le_bytes());
        }
        r
    }

    #[test]
    fn depth_stencil_record_writes_the_drivers_control_words() {
        let mut regs = image();
        // Depth on, writes all, LESS (2); stencil on, masks 0xff/0x0f;
        // front REPLACE/KEEP/INCR (fail, depth fail, pass), ALWAYS (8); back
        // KEEP everywhere, NEVER (1).
        let mut r = record(0, &[(4, 1), (8, 1), (0xc, 2), (0x10, 1), (0x18, 3), (0x1c, 1), (0x20, 7), (0x24, 8), (0x28, 1), (0x2c, 1), (0x30, 1), (0x34, 1)]);
        r[0x14] = 0xff;
        r[0x15] = 0x0f;
        apply(&mut regs, &r).unwrap();
        let c = 4;
        // STENCIL_ENABLE, Z_ENABLE, Z_WRITE, ZFUNC 1, STENCILFUNC 7, BF 0,
        // with bits 3 and 7 of the base kept.
        assert_eq!(word(&regs, c + 15), 0x1 | 0x2 | 0x4 | 0x10 | 0x700 | 0x88);
        // fail 3, z-pass 8, z-fail 0; back all 0.
        assert_eq!(word(&regs, c + 16), 0x083);
        assert_eq!(word(&regs, c + 17), 0x000f_ff00);
        assert_eq!(word(&regs, c + 18), 0x000f_ff00);
    }

    #[test]
    fn rasterizer_record_writes_cull_face_clip_and_bias() {
        let mut regs = image();
        // SOLID (3), cull BACK (3), clockwise front, bias 4, clamp 0.5,
        // slope 2.0, depth clip on.
        let r = record(1, &[(4, 3), (8, 3), (0xc, 0), (0x10, 4), (0x14, 0.5f32.to_bits()), (0x18, 2.0f32.to_bits()), (0x1c, 1)]);
        apply(&mut regs, &r).unwrap();
        let c = 4;
        assert_eq!(word(&regs, c + 9), 0x200 | 0x40 | 4 | 2 | 0x20_1800);
        assert_eq!(word(&regs, c + 8), 0x8_0000);
        assert_eq!(f32::from_bits(word(&regs, c + 14)), 4.0);
        assert_eq!(f32::from_bits(word(&regs, c + 13)), 32.0);
        assert_eq!(f32::from_bits(word(&regs, c + 12)), 0.5);
        // Still an image the decoder finds.
        let words: Vec<u32> = regs.chunks_exact(4).map(|c| u32::from_le_bytes(c.try_into().unwrap())).collect();
        assert_eq!(context_anchor(&words), Some(4));
    }

    #[test]
    fn blend_record_packs_every_target_from_the_first_unless_independent() {
        let mut regs = image();
        // Target 0: on, SRC_ALPHA (5) / INV_SRC_ALPHA (6) ADD (1), alpha
        // ONE (2) / ZERO (1) ADD, writes all (0xf).
        let mut r = record(2, &[]);
        let t = 4 + 8;
        for (o, v) in [(0, 1u32), (8, 5), (0xc, 6), (0x10, 1), (0x14, 2), (0x18, 1), (0x1c, 1)] {
            r[t + o..t + o + 4].copy_from_slice(&v.to_le_bytes());
        }
        r[t + 0x24] = 0xf;
        apply(&mut regs, &r).unwrap();
        let c = 4;
        let expected = 0x8000_0000 | 1 << 30 | 1 << 29 | 4 | 5 << 8 | 1 << 16;
        for n in 0..8 {
            assert_eq!(word(&regs, c + 21 + n), expected, "target {n}");
        }
        assert_eq!(word(&regs, c + 20), 0xffff_ffff);
        assert_eq!(word(&regs, c + 19), 0);
    }

    #[test]
    fn other_kinds_leave_the_image_as_it_was() {
        let mut regs = image();
        let before = regs.clone();
        let mut both = record(1, &[(4, 3)]);
        both.extend(record(7, &[]));
        assert_eq!(apply(&mut regs, &both), Err(7));
        assert_eq!(regs, before);
    }
}

/// Device 0x1b8 on a pipeline from 0x1c8: a new pipeline, one reference,
/// the base's stages and layout, its register image with the records
/// written (`apply`), the base untouched (+0x493e0 copies the image).
#[cfg(all(test, windows))]
mod made {
    use super::*;
    use crate::object::{S_OK, This};

    #[test]
    fn a_variant_is_its_base_with_the_records_written() {
        let regs: Vec<u8> = {
            let mut w = vec![0u32; 48];
            w[4] = 0x00cc_0010;
            w.iter().flat_map(|x| x.to_le_bytes()).collect()
        };
        let mut base: This = std::ptr::null();
        let iid = crate::pipeline::PIPELINE_IIDS[0];
        assert_eq!(crate::pipeline::hand_out_native_graphics(Vec::new(), regs.clone(), vec![1, 2, 3], &iid, &mut base), S_OK);
        // Rasterizer: cull front (2), clockwise.
        let mut record = vec![0u8; RECORD];
        record[0..4].copy_from_slice(&1u32.to_le_bytes());
        record[4..8].copy_from_slice(&3u32.to_le_bytes());
        record[8..12].copy_from_slice(&2u32.to_le_bytes());
        let mut variant: This = std::ptr::null();
        let d = crate::device::device();
        let f: extern "system" fn(This, This, u32, *const u8, *const crate::object::Iid, *mut This) -> crate::object::HRESULT =
            crate::object::slot(d, 0x1b8);
        assert_eq!(f(d, base, 1, record.as_ptr(), std::ptr::null(), &mut variant), S_OK);
        assert!(!variant.is_null() && variant != base);
        let (b, v) = (crate::pipeline::pipeline(base), crate::pipeline::pipeline(variant));
        let (bn, vn) = (b.native.as_ref().unwrap(), v.native.as_ref().unwrap());
        assert_eq!(bn.regs, regs);
        let mut expected = regs.clone();
        apply(&mut expected, &record).unwrap();
        assert_eq!(vn.regs, expected);
        assert_eq!(vn.layout, vec![1, 2, 3]);
        let release: extern "system" fn(This) -> u32 = crate::object::slot(variant, 0x10);
        assert_eq!(release(variant), 0);
        let release: extern "system" fn(This) -> u32 = crate::object::slot(base, 0x10);
        assert_eq!(release(base), 0);
    }
}
