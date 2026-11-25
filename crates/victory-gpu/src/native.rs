//! The title's own pipeline methods (device methods 0x1c8 and 0x218): the
//! console runtime's way of making a pipeline from programs it has already
//! linked into GPU memory. Each description carries, per stage, a table of
//! sections; one (typed 0x43) holds the program's 128-bit hash, which is the
//! digest of the `HASH` part of the shader container the title shipped (all
//! 41 programs checked in the first rooms: mip generation, skinning, the
//! compute graph, ray-tracing resolves, blits, the interface). The code in
//! GPU memory differs from the container's (the runtime relinks it), so the
//! pipeline is made from the container, as the title's other methods are.
//!
//! The containers are found where `victory translate` finds them: whole,
//! uncompressed, in the title's shader archive and executable. They are
//! indexed by digest the first time a native method is called (a scan of
//! some 500 MB, once).

use std::collections::HashMap;
use std::io::{Read, Seek, SeekFrom};
use std::path::PathBuf;
use std::sync::OnceLock;

/// Where a container lies: whole in a file (file, offset, bytes), or in
/// an SGD5 record (file, the record's marker, its bytes from the marker).
#[derive(Clone, Copy)]
enum Place {
    Raw(usize, u64, u32),
    Packed(usize, u64, u32),
}

struct Index {
    files: Vec<PathBuf>,
    by_digest: HashMap<[u8; 16], Place>,
}

/// The files searched, in the title's folder (`VICTORY_GAME_DIR`), and any
/// others listed in `VICTORY_NATIVE_FILES` (separated by `;`).
fn files() -> Vec<PathBuf> {
    let mut out = Vec::new();
    if let Some(dir) = std::env::var_os("VICTORY_GAME_DIR").map(PathBuf::from) {
        for name in ["shaders.rpf", "game_scarlett_master.exe"] {
            let p = dir.join(name);
            if p.is_file() {
                out.push(p);
            }
        }
    }
    if let Ok(list) = std::env::var("VICTORY_NATIVE_FILES") {
        out.extend(list.split(';').filter(|s| !s.is_empty()).map(PathBuf::from));
    }
    out
}

fn u32_at(b: &[u8], at: usize) -> Option<u32> {
    b.get(at..at + 4).map(|x| u32::from_le_bytes(x.try_into().unwrap()))
}

/// The digest of a container's `HASH` part (after its flags word), if
/// `b` starts with a whole container.
fn digest(b: &[u8]) -> Option<[u8; 16]> {
    if b.get(..4)? != b"DXBC" {
        return None;
    }
    let size = u32_at(b, 24)? as usize;
    let parts = u32_at(b, 28)? as usize;
    if size > b.len() || parts > 64 {
        return None;
    }
    (0..parts).find_map(|i| {
        let at = u32_at(b, 32 + 4 * i)? as usize;
        (b.get(at..at + 4)? == b"HASH").then(|| b.get(at + 12..at + 28)?.try_into().ok())?
    })
}

/// Indexes every container of the files: SGD5 records (unpacked once, to
/// read the digest) and containers stored as is (the executable's).
fn build() -> Index {
    let started = std::time::Instant::now();
    let files = files();
    let mut by_digest = HashMap::new();
    let (mut packed, mut raw) = (0usize, 0usize);
    for (n, path) in files.iter().enumerate() {
        let Ok(data) = std::fs::read(path) else { continue };
        if victory_game::sgd5::is_sgd5(&data) {
            for r in victory_game::sgd5::records(&data) {
                if let Some(c) = r.unpack()
                    && let Some(d) = digest(&c)
                {
                    packed += 1;
                    by_digest.entry(d).or_insert(Place::Packed(n, r.offset as u64, r.len_from_marker() as u32));
                }
            }
        }
        for at in memchr::memmem::find_iter(&data, b"DXBC") {
            if let Some(d) = digest(&data[at..]) {
                raw += 1;
                let size = u32_at(&data, at + 24).unwrap_or(0);
                by_digest.entry(d).or_insert(Place::Raw(n, at as u64, size));
            }
        }
    }
    tracing::info!(
        containers = by_digest.len(),
        packed,
        raw,
        files = files.len(),
        ms = started.elapsed().as_millis() as u64,
        "the title's pipeline methods: shader containers indexed by their hash"
    );
    Index { files, by_digest }
}

fn index() -> &'static Index {
    static INDEX: OnceLock<Index> = OnceLock::new();
    INDEX.get_or_init(build)
}

fn read_range(path: &PathBuf, at: u64, len: usize) -> Option<Vec<u8>> {
    let mut f = std::fs::File::open(path).ok()?;
    f.seek(SeekFrom::Start(at)).ok()?;
    let mut b = vec![0u8; len];
    f.read_exact(&mut b).ok()?;
    Some(b)
}

/// The container whose `HASH` digest is `digest`.
pub(crate) fn container(digest: &[u8; 16]) -> Option<Vec<u8>> {
    let ix = index();
    match *ix.by_digest.get(digest)? {
        Place::Raw(file, at, size) => read_range(&ix.files[file], at, size as usize),
        Place::Packed(file, marker, len) => {
            // The record with the bytes before its marker it reads (its
            // size word, kind and name).
            const BEFORE: u64 = 300;
            let from = marker.saturating_sub(BEFORE);
            let b = read_range(&ix.files[file], from, (marker - from) as usize + len as usize)?;
            victory_game::sgd5::record_at(&b, (marker - from) as usize)?.unpack()
        }
    }
}

/// The program hash in a stage's section table: the 16 bytes after the
/// record head `ffffffff 00000000 43000000`.
pub(crate) fn hash_in(table: &[u8]) -> Option<[u8; 16]> {
    const HEAD: [u8; 12] = [0xff, 0xff, 0xff, 0xff, 0, 0, 0, 0, 0x43, 0, 0, 0];
    let at = table.windows(12).position(|w| w == HEAD)? + 12;
    table.get(at..at + 16)?.try_into().ok()
}
