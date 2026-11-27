//! Checks a walk trace (`VICTORY_WALK_TRACE`, 48-dword records) against the
//! reference node test (`victory_shader::bvh::intersect`): each record's
//! node words and ray, tested on the CPU, against what the card returned.
//! `walk_check <walktrace.bin>`.
use victory_shader::bvh;

fn f(u: u32) -> f32 {
    f32::from_bits(u)
}

fn main() {
    let path = std::env::args().nth(1).expect("trace file");
    let data = std::fs::read(path).expect("read");
    let word = |i: usize| u32::from_le_bytes(data[4 * i..4 * i + 4].try_into().unwrap());
    let n = (word(0) as usize).min((data.len() / 4 - 16) / 48);
    let (mut boxes, mut tris) = (0u64, 0u64);
    let (mut box_set, mut box_order, mut tri_hit, mut tri_value) = (0u64, 0u64, 0u64, 0u64);
    let (mut gpu_hits, mut cpu_hits) = (0u64, 0u64);
    let mut shown = 0;
    for r in 0..n {
        let b = 16 + 48 * r;
        let rec: Vec<u32> = (0..48).map(|k| word(b + k)).collect();
        if rec[2] == 0 && rec[3] == 0 {
            continue;
        }
        let kind = rec[2] & 7;
        if kind > 5 {
            continue;
        }
        let ray = bvh::Ray::new([f(rec[8]), f(rec[9]), f(rec[10])], [f(rec[11]), f(rec[12]), f(rec[13])], f(rec[14]));
        let words: [u32; 28] = rec[16..44].try_into().unwrap();
        let sort = rec[44] != 0;
        let want = bvh::intersect(&words, kind, &ray, sort);
        let got = [rec[4], rec[5], rec[6], rec[7]];
        if kind >= 4 {
            boxes += 1;
            let mut a: Vec<u32> = want.iter().copied().filter(|&p| p != bvh::NONE).collect();
            let mut g: Vec<u32> = got.iter().copied().filter(|&p| p != bvh::NONE).collect();
            if want != got {
                a.sort_unstable();
                g.sort_unstable();
                if a != g {
                    box_set += 1;
                    if shown < 15 {
                        shown += 1;
                        println!("box differs: record {r} kind {kind} sort {sort} gpu {got:08x?} cpu {want:08x?} ray o {:?} d {:?} ext {}", [f(rec[8]), f(rec[9]), f(rec[10])], [f(rec[11]), f(rec[12]), f(rec[13])], f(rec[14]));
                    }
                } else {
                    box_order += 1;
                }
            }
        } else {
            tris += 1;
            let hit = |w: [u32; 4]| {
                let (num, den) = (f(w[0]), f(w[1]));
                den != 0.0 && num.is_finite() && { let t = num / den; t >= 0.0 && t < ray_extent(&rec) }
            };
            let (h1, h2) = (hit(got), hit(want));
            gpu_hits += u64::from(h1);
            cpu_hits += u64::from(h2);
            if h1 != h2 {
                tri_hit += 1;
                if shown < 30 {
                    shown += 1;
                    println!("triangle hit differs: record {r} k {kind} gpu {:?} cpu {:?}", got.map(f), want.map(f));
                }
            } else if got != want {
                tri_value += 1;
            }
        }
    }
    println!("records {n}: boxes {boxes} (children differ {box_set}, order only {box_order}); triangles {tris} (hit/miss differs {tri_hit}, values only {tri_value}); hits gpu {gpu_hits} cpu {cpu_hits}");
}

fn ray_extent(rec: &[u32]) -> f32 {
    f(rec[14])
}
