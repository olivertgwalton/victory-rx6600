//! Disassembles native programs from a `VICTORY_NATIVE_CODE` dump (u32
//! length, then the code) whose index key (FNV-1a of the first 256 bytes)
//! is listed: `sp3_dump <code file> <out dir> <key>...`. Wave64 assumed off.
fn key(code: &[u8]) -> u64 {
    code[..code.len().min(256)].iter().fold(0xcbf2_9ce4_8422_2325u64, |h, &b| (h ^ u64::from(b)).wrapping_mul(0x0100_0000_01b3))
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let data = std::fs::read(&args[1]).expect("code file");
    let out = std::path::Path::new(&args[2]);
    std::fs::create_dir_all(out).unwrap();
    let wanted: std::collections::HashSet<u64> = args[3..].iter().map(|k| u64::from_str_radix(k, 16).unwrap()).collect();
    let mut at = 0;
    let mut found = 0;
    while at + 4 <= data.len() {
        let n = u32::from_le_bytes(data[at..at + 4].try_into().unwrap()) as usize;
        let code = &data[at + 4..at + 4 + n];
        at += 4 + n;
        let k = key(code);
        if !wanted.contains(&k) {
            continue;
        }
        found += 1;
        let mut text = String::new();
        match victory_shader::gfx10::decode(code) {
            Ok(insts) => {
                for i in insts {
                    text.push_str(&format!("{:06x}: {}\n", i.offset, i.text(false)));
                }
            }
            Err(e) => text.push_str(&format!("; decode: {e}\n")),
        }
        std::fs::write(out.join(format!("{k:016x}.s")), text).unwrap();
    }
    eprintln!("{found} programs");
}
