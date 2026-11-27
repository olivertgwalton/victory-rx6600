//! Decoding the requests a title streams: zlib and BCPack, on real data.
//!
//! The corpus is not in the repository (it is the title's data): a file
//! named by `VICTORY_BENCH_CORPUS`, `VBC1` then per request its options
//! (u64: bit 0 zlib, bits 1-4 the BC format), source and destination sizes
//! (u32 each) and source bytes. Without it the benches say so and stop.
//! Every request is decoded once and checked before timing.

use criterion::{Criterion, Throughput, criterion_group, criterion_main};
use victory_formats::{bcpack, zlib};

struct Request {
    options: u64,
    source: Vec<u8>,
    size: usize,
}

fn corpus() -> Option<Vec<Request>> {
    let path = std::env::var_os("VICTORY_BENCH_CORPUS")?;
    let bytes = std::fs::read(path).expect("the corpus reads");
    assert_eq!(&bytes[..4], b"VBC1", "a VBC1 corpus");
    let mut at = 4;
    let mut out = Vec::new();
    while at < bytes.len() {
        let options = u64::from_le_bytes(bytes[at..at + 8].try_into().unwrap());
        let source = u32::from_le_bytes(bytes[at + 8..at + 12].try_into().unwrap()) as usize;
        let size = u32::from_le_bytes(bytes[at + 12..at + 16].try_into().unwrap()) as usize;
        at += 16;
        out.push(Request { options, source: bytes[at..at + source].to_vec(), size });
        at += source;
    }
    Some(out)
}

fn decode(r: &Request, z: &mut zlib::Inflater, t: &bcpack::Tables, out: &mut [u8]) {
    let bc = ((r.options >> 1) & 0xf) as u8;
    if bc == 0 {
        z.inflate(&r.source, &mut out[..r.size]).expect("the stream inflates");
    } else {
        bcpack::decode(bc, &r.source, &mut out[..r.size], t).expect("the stream unpacks");
    }
}

fn benches(c: &mut Criterion) {
    let Some(corpus) = corpus() else {
        eprintln!("decode benches: set VICTORY_BENCH_CORPUS to a VBC1 corpus");
        return;
    };
    let tables = bcpack::Tables::builtin().expect("the built-in tables");
    let mut z = zlib::Inflater::new();
    let mut out = vec![0u8; corpus.iter().map(|r| r.size).max().unwrap_or(0)];
    for r in &corpus {
        decode(r, &mut z, &tables, &mut out);
    }
    let kinds: [(&str, fn(u64) -> bool); 5] = [
        ("zlib", |o| o & 0x1e == 0),
        ("bcpack bc1", |o| (o >> 1) & 0xf == 1),
        ("bcpack bc5", |o| (o >> 1) & 0xf == 5),
        ("bcpack bc6", |o| (o >> 1) & 0xf == 6),
        ("bcpack bc7", |o| (o >> 1) & 0xf == 7),
    ];
    let mut group = c.benchmark_group("decode");
    for (name, pick) in kinds {
        let set: Vec<&Request> = corpus.iter().filter(|r| pick(r.options)).collect();
        if set.is_empty() {
            continue;
        }
        group.throughput(Throughput::Bytes(set.iter().map(|r| r.size as u64).sum()));
        group.bench_function(name, |b| {
            b.iter(|| {
                for r in &set {
                    decode(r, &mut z, &tables, &mut out);
                }
            })
        });
    }
    group.finish();
}

criterion_group! {
    name = decode_benches;
    config = Criterion::default().sample_size(20).measurement_time(std::time::Duration::from_secs(8));
    targets = benches
}
criterion_main!(decode_benches);
