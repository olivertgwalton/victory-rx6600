//! Disassembles the native programs of a shader container (its ILDB part,
//! else DXIL): `native_dump <file.cso>`, with each program's stage I/O.
fn main() {
    let path = std::env::args().nth(1).expect("container");
    let b = std::fs::read(path).unwrap();
    let u = |o: usize| u32::from_le_bytes(b[o..o + 4].try_into().unwrap()) as usize;
    let parts: Vec<(&[u8], &[u8])> = (0..u(28)).map(|k| { let o = u(32 + 4 * k); (&b[o..o + 4], &b[o + 8..o + 8 + u(o + 4)]) }).collect();
    let part = parts.iter().find(|p| p.0 == b"ILDB").or_else(|| parts.iter().find(|p| p.0 == b"DXIL")).expect("no DXIL").1;
    for p in victory_shader::native::native_programs(part).unwrap() {
        println!("; {:?} wave64={} vgprs={}\n; {:?}", p.stage, p.wave64, p.vgprs, p.io);
        for i in victory_shader::gfx10::decode(&p.code).unwrap() {
            println!("{:06x}: {}", i.offset, i.text(false));
        }
    }
}
