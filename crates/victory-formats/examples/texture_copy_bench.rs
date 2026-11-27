//! Detiling and tiling throughput of the console's texture layouts:
//! every subresource of a texture copied out of tiled memory and back.
//! Run with `cargo run --release -p victory-formats --example texture_copy_bench`.

use std::hint::black_box;
use std::time::Instant;

use victory_formats::texture::{Dimension, Swizzle, Texture, TextureDesc};

fn main() {
    let base = TextureDesc {
        dimension: Dimension::Tex2d,
        width: 2048,
        height: 2048,
        depth_or_array: 1,
        mips: 0,
        format: 98, // BC7
        samples: 1,
        swizzle: Swizzle::SW_64KB_D,
    };
    let cases = [
        ("BC7 2048 chain 64KB_D", base),
        ("BC1 2048 chain 64KB_S", TextureDesc { format: 71, swizzle: Swizzle::SW_64KB_S, ..base }),
        ("RGBA8 1024 chain 4KB_S", TextureDesc { width: 1024, height: 1024, format: 28, swizzle: Swizzle::SW_4KB_S, ..base }),
        ("R32F 256 64KB_R_X", TextureDesc { width: 256, height: 256, mips: 1, format: 41, swizzle: Swizzle::SW_64KB_R_X, ..base }),
        ("RGBA8 1920x1080 64KB_R_X", TextureDesc { width: 1920, height: 1080, mips: 1, format: 28, swizzle: Swizzle::SW_64KB_R_X, ..base }),
        ("RGBA16F 1920x1080 64KB_D_X", TextureDesc { width: 1920, height: 1080, mips: 1, format: 10, swizzle: Swizzle::SW_64KB_D_X, ..base }),
        ("R8 4096 4KB_S_X chain", TextureDesc { width: 4096, height: 4096, format: 61, swizzle: Swizzle(21), ..base }),
        (
            "RGBA8 128^3 64KB_S volume",
            TextureDesc { dimension: Dimension::Tex3d, width: 128, height: 128, depth_or_array: 128, format: 28, swizzle: Swizzle::SW_64KB_S, ..base },
        ),
        ("RGBA8 1024 linear chain", TextureDesc { width: 1024, height: 1024, format: 28, swizzle: Swizzle::LINEAR, ..base }),
    ];
    for (name, desc) in cases {
        let t = Texture::new(&desc).unwrap();
        let l = &t.planes[0].layout;
        if !l.addressable() {
            println!("{name:30} no copies in this build");
            continue;
        }
        let mut tiled = vec![0u8; l.size as usize];
        for (i, b) in tiled.iter_mut().enumerate() {
            *b = (i.wrapping_mul(0x9e37_79b9) >> 13) as u8;
        }
        let n = l.desc.element_bytes as usize;
        let subs: Vec<(u32, usize, usize)> = (0..l.mips.len() as u32)
            .map(|m| {
                let mip = &l.mips[m as usize];
                let pitch = mip.width as usize * n;
                (m, pitch, pitch * mip.height as usize * mip.depth as usize)
            })
            .collect();
        let mut linear = vec![0u8; subs.iter().map(|s| s.2).max().unwrap()];
        let bytes: usize = subs.iter().map(|s| s.2).sum();
        let mut best = [f64::MAX; 2];
        for _ in 0..7 {
            for (k, detile) in [true, false].into_iter().enumerate() {
                let start = Instant::now();
                let reps = (64 << 20) / bytes.max(1) + 1;
                for _ in 0..reps {
                    for &(m, pitch, len) in &subs {
                        let rows = len / pitch;
                        let depth = l.mips[m as usize].depth as usize;
                        let plane = pitch * (rows / depth);
                        if detile {
                            l.detile(black_box(&tiled), m, 0, &mut linear[..len], pitch, plane).unwrap();
                        } else {
                            l.tile(&mut tiled, m, 0, black_box(&linear[..len]), pitch, plane).unwrap();
                        }
                    }
                }
                let secs = start.elapsed().as_secs_f64();
                best[k] = best[k].min(secs / reps as f64);
            }
        }
        let gbs = |s: f64| bytes as f64 / s / 1e9;
        println!("{name:30} {:8.2} MB  detile {:6.2} GB/s  tile {:6.2} GB/s", bytes as f64 / 1e6, gbs(best[0]), gbs(best[1]));
    }
}
