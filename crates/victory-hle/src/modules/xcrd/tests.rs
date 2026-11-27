//! The port against the console's library. Differential tests (`both`)
//! need `VICTORY_REFERENCE_DIR` (the folder holding `xcrdapi.dll`); without
//! it they say so and pass. The fixed-value tests need nothing: their
//! expected values come from reading the library's code (the address is
//! named).

use std::ffi::c_void;

use super::reference::{self, FAKE, Record, Session, accepting, session};
use super::*;

/// The reference's and the layer's export `name`, as `F`.
fn pair<F: Copy>(name: &str) -> Option<(F, F)> {
    let reference = reference::reference(name)?;
    let layer = reference::layer(name);
    // SAFETY: both are the export `name`, whose signature `F` is.
    unsafe { Some((std::mem::transmute_copy(&reference), std::mem::transmute_copy(&layer))) }
}

macro_rules! both {
    ($name:literal: $ty:ty) => {
        match pair::<$ty>($name) {
            Some(p) => p,
            None => {
                eprintln!("{}: no reference (VICTORY_REFERENCE_DIR); skipped", $name);
                return;
            }
        }
    };
}

/// Both sides print every message class.
fn trace_all(s: &Session) {
    let (r, l) = both_or_none::<extern "system" fn(u32) -> HRESULT>("XCrdSetTraceFlags").unwrap();
    r(7);
    l(7);
    s.take();
}

fn both_or_none<F: Copy>(name: &str) -> Option<(F, F)> {
    pair(name)
}

fn wz(s: &str) -> Vec<u16> {
    s.encode_utf16().chain([0]).collect()
}

/// Runs `f` once per side (reference first), each against a fresh fake
/// driver set up by `setup`, and returns each side's result and record.
fn run<T>(s: &Session, setup: impl Fn(&Session), f: impl Fn(bool) -> T) -> ((T, Record), (T, Record)) {
    let trace = std::env::var_os("VICTORY_FUZZ_TRACE").is_some();
    setup(s);
    let r = f(true);
    if trace {
        eprintln!("  reference done");
    }
    let rr = s.take();
    setup(s);
    let l = f(false);
    let lr = s.take();
    ((r, rr), (l, lr))
}

fn same<T: std::fmt::Debug + PartialEq>(what: &str, ((r, rr), (l, lr)): ((T, Record), (T, Record))) {
    if std::env::var_os("VICTORY_FUZZ_TRACE").is_some() {
        eprintln!("{what}: done");
    }
    assert_eq!(rr, lr, "{what}: what the driver saw");
    assert_eq!(r, l, "{what}: result");
}

// Fixed values, from the library's code.

#[test]
fn ntstatus_mapping_as_0x18000c814() {
    assert_eq!(srb::hresult(0), 0);
    // Customer bit: dropped.
    assert_eq!(srb::hresult(0xe000_1234_u32 as i32), 0xc000_1234_u32 as i32);
    // The driver's own: facility 0x924.
    assert_eq!(srb::hresult(0xc05e_0017_u32 as i32), 0x8924_0017_u32 as i32);
    // STATUS_INVALID_PARAMETER -> ERROR_INVALID_PARAMETER.
    assert_eq!(srb::hresult(0xc000_000d_u32 as i32), E_INVALIDARG);
    // A status with no Win32 error: HRESULT_FROM_NT.
    assert_eq!(srb::hresult(0xc0ff_0001_u32 as i32), 0xd0ff_0001_u32 as i32);
}

#[test]
fn payload_header_as_0x18000e7d0() {
    let p = Payload::new(0x7800_0114, 0x22);
    assert_eq!(p.bytes.len(), 0x1c + 0x22);
    assert_eq!(&p.bytes[0..4], &0x1cu32.to_le_bytes());
    assert_eq!(&p.bytes[4..12], b"MSFT_XVD");
    assert_eq!(&p.bytes[0x10..0x14], &0x7800_0114u32.to_le_bytes());
    assert_eq!(&p.bytes[0x18..0x1c], &0x22u32.to_le_bytes());
    assert!(p.bytes[12..16].iter().chain(&p.bytes[0x14..0x18]).all(|&b| b == 0));
}

#[test]
fn string_copy_as_0x180001ba0() {
    let mut body = [0xffu8; 16];
    assert_eq!(srb::copy_string(&mut body, 0, 8, &wz("abc")), 0);
    assert_eq!(&body[..8], &[b'a', 0, b'b', 0, b'c', 0, 0, 0]);
    assert_eq!(srb::copy_string(&mut body, 0, 6, &wz("abcd")), 0x8000_0005_u32 as i32);
    assert_eq!(&body[..6], &[b'a', 0, b'b', 0, 0, 0]);
    assert_eq!(srb::copy_string(&mut body, 0, 0, &wz("a")), 0xc000_000d_u32 as i32);
}

#[test]
fn paths_as_0x180001ce0_and_0x1800041b0() {
    let mut out = std::ptr::null_mut();
    assert_eq!(XCrdBuildPath(&mut out, 1, wz("a\\b").as_ptr(), 0), S_OK);
    // SAFETY: a buffer we just got.
    assert_eq!(unsafe { wide_str(out) }, "[XUC:]\\a\\b");
    assert_eq!(XCrdFreeDevicePathBuffer(out.cast()), S_OK);
    assert_eq!(XCrdBuildPath(&mut out, 0, wz("x").as_ptr(), 1), S_OK);
    assert_eq!(unsafe { wide_str(out) }, "[XVE:]\\[XTE:]\\x");
    assert_eq!(XCrdBuildPath(&mut out, 0x25, wz("x").as_ptr(), 0), E_INVALIDARG);
    assert_eq!(XCrdBuildPath(&mut out, 0, wz("x").as_ptr(), 2), E_INVALIDARG);
    let mut d = 99;
    assert_eq!(XCrdParsePath(wz("[xss:]\\q").as_ptr(), &mut d), S_OK);
    assert_eq!(d, 2);
    assert_eq!(XCrdParsePath(wz("[XS").as_ptr(), &mut d), E_INVALIDARG);
    assert!(XCrdIsCorruptionError(0x8007_0571_u32 as i32));
    assert!(!XCrdIsCorruptionError(0x8007_0572_u32 as i32));
}

// Against the reference.

#[test]
fn trace_flags() {
    let s = session();
    let (r, l) = both!("XCrdSetTraceFlags": extern "system" fn(u32) -> HRESULT);
    for f in [0u32, 1, 2, 3, 4, 7, 8, 9, 0x10, 0x8000_0000, u32::MAX] {
        same(&format!("flags {f:#x}"), run(&s, |_| {}, |rf| if rf { r(f) } else { l(f) }));
    }
    r(0);
    l(0);
}

#[test]
fn build_path_everywhere() {
    let s = session();
    let (r, l) = both!("XCrdBuildPath": extern "system" fn(*mut *mut u16, u32, *const u16, u32) -> HRESULT);
    let paths = ["", "a", "dir\\file.xvd", "[XUC:]\\x"];
    for domain in (0..0x28).chain([u32::MAX, 0x8000_0000]) {
        for flags in [0u32, 1, 2, 3, 0x100] {
            for path in paths.iter().map(|p| Some(wz(p))).chain([None]) {
                let p = path.as_ref().map_or(std::ptr::null(), |p| p.as_ptr());
                same(
                    &format!("domain {domain} flags {flags} path {path:?}"),
                    run(&s, |_| {}, |rf| {
                        let mut out = std::ptr::null_mut();
                        let hr = if rf { r(&mut out, domain, p, flags) } else { l(&mut out, domain, p, flags) };
                        // SAFETY: a buffer the call returned, or null.
                        let text = (!out.is_null()).then(|| unsafe { wide_str(out) });
                        heap_free(out.cast());
                        (hr, text)
                    }),
                );
            }
        }
    }
}

#[test]
fn parse_and_update_paths() {
    let s = session();
    let (rp, lp) = both!("XCrdParsePath": extern "system" fn(*const u16, *mut u32) -> HRESULT);
    let (ru, lu) = both!("XCrdUpdatePath": extern "system" fn(*mut u16, u32) -> HRESULT);
    let mut inputs: Vec<String> = DOMAINS.iter().filter(|d| !d.is_empty()).map(|d| format!("{d}\\p")).collect();
    inputs.extend(DOMAINS.iter().filter(|d| !d.is_empty()).map(|d| d.to_lowercase()));
    inputs.extend(["", "[", "[XU", "[XUC:", "[XUC:]", "XUC:]", "c:\\x", "[XQQ:]\\x", "[xUc:]rest"].map(String::from));
    for text in &inputs {
        let w = wz(text);
        same(text, run(&s, |_| {}, |rf| {
            let mut d = 0xdead;
            let hr = if rf { rp(w.as_ptr(), &mut d) } else { lp(w.as_ptr(), &mut d) };
            (hr, d)
        }));
        for domain in (0..0x26).chain([u32::MAX]) {
            same(&format!("update {text} to {domain}"), run(&s, |_| {}, |rf| {
                let mut buffer = wz(text);
                buffer.resize(64, 0xffff);
                let hr = if rf { ru(buffer.as_mut_ptr(), domain) } else { lu(buffer.as_mut_ptr(), domain) };
                (hr, buffer)
            }));
        }
    }
    same("null path", run(&s, |_| {}, |rf| { let mut d = 0; if rf { rp(std::ptr::null(), &mut d) } else { lp(std::ptr::null(), &mut d) } }));
    same("null out", run(&s, |_| {}, |rf| { let w = wz("[XUC:]"); if rf { rp(w.as_ptr(), std::ptr::null_mut()) } else { lp(w.as_ptr(), std::ptr::null_mut()) } }));
    same("update null", run(&s, |_| {}, |rf| if rf { ru(std::ptr::null_mut(), 1) } else { lu(std::ptr::null_mut(), 1) }));
}

#[test]
fn error_classes_devices_and_keys() {
    let s = session();
    let (r, l) = both!("XCrdIsCorruptionError": extern "system" fn(HRESULT) -> bool);
    for base in [0x8007_01e7u32, 0x8007_0570, 0x8007_045d, 0, 0x8000_4005] {
        for d in -2i64..=3 {
            let hr = (i64::from(base) + d) as u32 as i32;
            same(&format!("{hr:#x}"), run(&s, |_| {}, |rf| if rf { r(hr) } else { l(hr) }));
        }
    }
    let (rh, lh) = both!("XCrdHasInternalDeviceId": extern "system" fn(u32, *mut [u8; 16]) -> bool);
    for id in (0..0x40).chain([u32::MAX]) {
        same(&format!("id {id}"), run(&s, |_| {}, |rf| {
            let mut g = [0xaa; 16];
            let found = if rf { rh(id, &mut g) } else { lh(id, &mut g) };
            let found_null = if rf { rh(id, std::ptr::null_mut()) } else { lh(id, std::ptr::null_mut()) };
            (found, found_null, g)
        }));
    }
    let (ri, li) = both!("XCrdIsInternalStorageDevice": extern "system" fn(*const [u8; 16], *mut u32) -> bool);
    let mut guids: Vec<[u8; 16]> = INTERNAL.iter().map(|(g, _)| *g).collect();
    guids.push([0; 16]);
    let mut near = INTERNAL[0].0;
    near[15] ^= 1;
    guids.push(near);
    for g in &guids {
        same(&format!("{g:x?}"), run(&s, |_| {}, |rf| {
            let mut id = 0xdead;
            let found = if rf { ri(g, &mut id) } else { li(g, &mut id) };
            let found_null = if rf { ri(g, std::ptr::null_mut()) } else { li(g, std::ptr::null_mut()) };
            (found, found_null, id)
        }));
    }
    let (rk, lk) = both!("XCrdXasKeyQueryValue": extern "system" fn(*const u16, u32) -> u64);
    for text in ["", "PRST=1", "NRM=1", "A=0;PRST=1;NRM=1", "prst=1", "PRST=0", "NRM=", "xNRM=1x"] {
        let w = wz(text);
        for flag in [0u32, 1, 2, 3, 4, 5, 8, 16, 0x8000_0000, u32::MAX] {
            same(&format!("{text} {flag}"), run(&s, |_| {}, |rf| if rf { rk(w.as_ptr(), flag) } else { lk(w.as_ptr(), flag) }));
        }
    }
    let (rf_, lf_) = both!("XCrdFreeDevicePathBuffer": extern "system" fn(*mut c_void) -> HRESULT);
    same("free null", run(&s, |_| {}, |rf| if rf { rf_(std::ptr::null_mut()) } else { lf_(std::ptr::null_mut()) }));
}

#[test]
fn unsupported_and_unimplemented() {
    let s = session();
    for name in [
        "XCrdQueryEncryptedInfo",
        "XCrdQueryUnlockInfo",
        "XCrdRepairFiles",
        "XCrdTrimDeallocatedClusters",
        "XCrdShuffle",
        "XCrdWaitForAllRemovableDiskRemoval",
        "XCrdWaitForAnyRemovableDiskArrival",
    ] {
        let Some((r, l)) = pair::<extern "system" fn(usize, usize, usize, usize) -> HRESULT>(name) else {
            eprintln!("{name}: no reference; skipped");
            return;
        };
        same(name, run(&s, |_| {}, |rf| if rf { r(1, 2, 3, 4) } else { l(1, 2, 3, 4) }));
    }
}

/// A fake adapter on a fake handle.
fn adapter() -> Adapter {
    Adapter { handle: FAKE as Handle, port: 0 }
}

#[test]
fn open_adapter() {
    let s = session();
    trace_all(&s);
    let (r, l) = both!("XCrdOpenAdapter": extern "system" fn(*mut *mut Adapter) -> HRESULT);
    let (rc, lc) = both!("XCrdCloseAdapter": extern "system" fn(*mut Adapter) -> HRESULT);
    let call = |rf: bool| {
        let mut out: *mut Adapter = std::ptr::null_mut();
        let hr = if rf { r(&mut out) } else { l(&mut out) };
        let got = (!out.is_null()).then(|| {
            // SAFETY: the adapter just returned.
            let a = unsafe { (out.cast::<usize>().read(), out.cast::<u32>().add(2).read()) };
            let closed = if rf { rc(out) } else { lc(out) };
            (a, closed)
        });
        (hr, got)
    };
    // No such device at all (the layer's case), and access refused.
    for error in [2u32, 5, 0x15] {
        same(&format!("open fails {error}"), run(&s, move |s| s.open(move |_| Err(error)), call));
    }
    // Port 3 opens and answers.
    same("port 3", run(&s, |s| {
        s.open(|n| if n.ends_with("Scsi3:") { Ok(FAKE + 6) } else { Err(2) });
        s.reply(accepting(|_, _| {}));
    }, call));
    // Every port opens; the probe fails in different ways.
    for (what, status, code, sign) in [
        ("nt failure", 0xc000_0001_u32 as i32, 0, true),
        ("return code", 0, 0xc000_0185_u32, true),
        ("no signature", 0, 0, false),
        ("positive code", 0, 0x103, true),
    ] {
        same(what, run(&s, move |s| {
            s.open(|_| Ok(FAKE + 8));
            s.reply(move |_, b| {
                b[0x14..0x18].copy_from_slice(&code.to_le_bytes());
                if sign {
                    b[4..12].copy_from_slice(&srb::SIGNATURE);
                }
                status
            });
        }, call));
    }
}

#[test]
fn cancel_update() {
    let s = session();
    trace_all(&s);
    let (r, l) = both!("XCrdCancelUpdate": extern "system" fn(*const Adapter, *const u16) -> HRESULT);
    let a = adapter();
    let long = "x".repeat(0x80000);
    let cases: Vec<(&str, *const Adapter, Option<Vec<u16>>)> = vec![
        ("null adapter", std::ptr::null(), Some(wz("[XUC:]\\a"))),
        ("null path", &a, None),
        ("path", &a, Some(wz("[XUC:]\\some\\content"))),
        ("empty path", &a, Some(wz(""))),
        ("too long", &a, Some(wz(&long))),
    ];
    for (what, adapter, path) in &cases {
        let p = path.as_ref().map_or(std::ptr::null(), |p| p.as_ptr());
        for (driver, status, code) in [("ok", 0, 0u32), ("refused", 0, 0xc05e_0003), ("nt", 0xc000_0001_u32 as i32, 0), ("both", 0xc000_0001_u32 as i32, 0xc000_0022)] {
            same(&format!("{what} {driver}"), run(&s, move |s| s.reply(move |_, b| {
                b[0x14..0x18].copy_from_slice(&code.to_le_bytes());
                status
            }), |rf| if rf { r(*adapter, p) } else { l(*adapter, p) }));
        }
    }
    // Busy three times, then done: three sleeps of 100 ms.
    same("busy", run(&s, |s| {
        let mut n = 0;
        s.reply(move |_, b| {
            n += 1;
            let code: u32 = if n <= 3 { 0x8000_0011 } else { 0 };
            b[0x14..0x18].copy_from_slice(&code.to_le_bytes());
            0
        });
    }, |rf| { let p = wz("[XUC:]\\a"); if rf { r(&a, p.as_ptr()) } else { l(&a, p.as_ptr()) } }));
    // Busy for good: 100 tries.
    same("always busy", run(&s, |s| s.reply(|_, b| { b[0x14..0x18].copy_from_slice(&0x8000_0011u32.to_le_bytes()); 0 }),
        |rf| { let p = wz("[XUC:]\\a"); if rf { r(&a, p.as_ptr()) } else { l(&a, p.as_ptr()) } }));
}

#[test]
fn metadata_handles() {
    let s = session();
    trace_all(&s);
    let (ro, lo) = both!("XCrdOpenMetadataHandle": extern "system" fn(*mut *mut Metadata, *const Adapter, *const u16) -> HRESULT);
    let (rc, lc) = both!("XCrdCloseMetadataHandle": extern "system" fn(*mut *mut Metadata, *const Adapter) -> HRESULT);
    let a = adapter();
    let path = wz("[XUC:]\\pkg");
    for (what, status) in [("ok", 0), ("fails", 0xc000_0034_u32 as i32)] {
        same(what, run(&s, move |s| {
            let mut answer = accepting(|code, body| {
                if code == 0x7800_00d0 {
                    body[8..12].copy_from_slice(&0x1234u32.to_le_bytes());
                }
            });
            s.reply(move |c, b| if status < 0 { status } else { answer(c, b) });
        }, |rf| {
            let mut m: *mut Metadata = std::ptr::null_mut();
            let hr = if rf { ro(&mut m, &a, path.as_ptr()) } else { lo(&mut m, &a, path.as_ptr()) };
            // SAFETY: the handle just returned, if any.
            let id = (!m.is_null()).then(|| unsafe { (*m).id });
            let closed = (!m.is_null()).then(|| if rf { rc(&mut m, &a) } else { lc(&mut m, &a) });
            (hr, id, closed, m.is_null())
        }));
    }
    for (what, ad, out, p) in [
        ("null adapter", std::ptr::null(), true, true),
        ("null out", &a as *const Adapter, false, true),
        ("null path", &a as *const Adapter, true, false),
    ] {
        same(what, run(&s, |_| {}, |rf| {
            let mut m: *mut Metadata = std::ptr::null_mut();
            let o = if out { &mut m as *mut _ } else { std::ptr::null_mut() };
            let pp = if p { path.as_ptr() } else { std::ptr::null() };
            if rf { ro(o, ad, pp) } else { lo(o, ad, pp) }
        }));
    }
}

/// The differential tests are not vacuous: the reference really reaches
/// the fake driver, with the block read from its code (0x180001e30).
#[test]
fn the_reference_reaches_the_fake_driver() {
    let s = session();
    let (r, _) = both!("XCrdCancelUpdate": extern "system" fn(*const Adapter, *const u16) -> HRESULT);
    s.reply(accepting(|_, _| {}));
    let a = adapter();
    let p = wz("[XUC:]\\ab");
    assert_eq!(r(&a, p.as_ptr()), S_OK);
    let record = s.take();
    assert_eq!(record.calls.len(), 1);
    let call = &record.calls[0];
    assert_eq!((call.handle, call.code), (FAKE, srb::IOCTL_SCSI_MINIPORT));
    let mut expected = Payload::new(0x7800_0114, 9 * 2 + 0xa);
    expected.u32(0, 8);
    expected.u32(4, 18);
    srb::copy_string(expected.body(), 8, 20, &p);
    assert_eq!(call.input, expected.bytes);
}

// A differential fuzzer over an export's arguments: each argument has a
// kind; from a baseline of valid values, each argument is varied alone
// through its kind's values, and every case runs under each way the driver
// can answer. Results (at the export's width), every buffer passed, and
// every call the driver saw must match.

#[derive(Clone, Copy, Debug)]
pub enum Kind {
    /// An adapter (fake handle), or null.
    Adp,
    /// A wide path, null, empty, or past the library's limit.
    Path,
    /// A structure read (and maybe written) of n bytes: zeros, a pattern,
    /// small values, all ones; never null (the library does not check).
    In(usize),
    /// As `In`, and null (the library checks).
    InOpt(usize),
    /// A buffer written: n bytes the call may fill; never null.
    Out(usize),
    /// As `Out`, and null.
    OutOpt(usize),
    /// A wide path the library does not check for null.
    PathReq,
    /// As `PathReq`, never past the library's limit (where it would loop).
    PathShort,
    /// A pointer out-parameter that receives a heap object of n bytes:
    /// compared by null-ness and the object's bytes.
    Object(usize),
    /// A pointer out-parameter that receives a heap wide string: compared
    /// by null-ness and text.
    WideObject,
    /// An integer from this list (the first is the baseline).
    Int(&'static [u64]),
}

#[derive(Clone, Copy, Debug)]
pub enum Width {
    W8,
    W32,
    W64,
}

const BUFFER: usize = 0x4000;

/// One argument's value: what is passed, and the memory behind it.
#[derive(Clone)]
enum Value {
    Null,
    Bytes(Vec<u8>),
    Int(u64),
}

fn values(kind: Kind) -> Vec<Value> {
    let fill = |n: usize, f: &dyn Fn(usize) -> u8| {
        let mut v: Vec<u8> = (0..BUFFER).map(|i| 0xa5 ^ (i as u8)).collect();
        for (i, b) in v.iter_mut().take(n).enumerate() {
            *b = f(i);
        }
        Value::Bytes(v)
    };
    let text = |t: &str| {
        let mut v: Vec<u8> = t.encode_utf16().chain([0]).flat_map(u16::to_le_bytes).collect();
        v.resize(v.len().max(BUFFER), 0xee);
        Value::Bytes(v)
    };
    match kind {
        Kind::Adp => {
            let mut v = vec![0u8; 16];
            v[..8].copy_from_slice(&(FAKE as u64).to_le_bytes());
            vec![Value::Bytes(v), Value::Null]
        }
        Kind::Path => vec![text("[XUC:]\\pkg\\content"), Value::Null, text(""), text(&"y".repeat(0x80010))],
        Kind::PathShort => vec![text("[XUC:]\\pkg\\content"), text("")],
        Kind::PathReq => vec![text("[XUC:]\\pkg\\content"), text(""), text(&"y".repeat(0x80010))],
        Kind::In(n) | Kind::InOpt(n) => {
            let mut v = vec![
                fill(n, &|i| (i as u8).wrapping_mul(7).wrapping_add(1)),
                fill(n, &|_| 0),
                fill(n, &|i| if i % 4 == 0 { 1 } else { 0 }),
                fill(n, &|_| 0xff),
            ];
            if matches!(kind, Kind::InOpt(_)) {
                v.insert(1, Value::Null);
            }
            v
        }
        Kind::Out(n) => vec![fill(n, &|_| 0xcc)],
        Kind::OutOpt(n) | Kind::Object(n) => vec![fill(n, &|_| 0xcc), Value::Null],
        Kind::WideObject => vec![fill(8, &|_| 0xcc), Value::Null],
        Kind::Int(list) => list.iter().map(|&v| Value::Int(v)).collect(),
    }
}

/// What a case left behind on one side.
#[derive(Debug, PartialEq)]
struct Outcome {
    result: u64,
    buffers: Vec<Option<Vec<u8>>>,
    record: Record,
}

type Any8 = extern "system" fn(usize, usize, usize, usize, usize, usize, usize, usize) -> u64;

fn call_case(f: Any8, kinds: &[Kind], case: &[Value], width: Width, s: &Session) -> Outcome {
    let mut memory: Vec<Option<Vec<u8>>> = case.iter().map(|v| match v { Value::Bytes(b) => Some(b.clone()), _ => None }).collect();
    let mut args = [0usize; 8];
    for (i, v) in case.iter().enumerate() {
        args[i] = match v {
            Value::Null => 0,
            Value::Int(x) => *x as usize,
            Value::Bytes(_) => memory[i].as_mut().unwrap().as_mut_ptr() as usize,
        };
    }
    let r = f(args[0], args[1], args[2], args[3], args[4], args[5], args[6], args[7]);
    let result = match width { Width::W8 => r & 0xff, Width::W32 => r & 0xffff_ffff, Width::W64 => r };
    // Objects: what the pointer points at, not the address.
    for (i, k) in kinds.iter().enumerate() {
        if let (Kind::Object(n), Some(m)) = (k, memory[i].as_mut()) {
            let p = usize::from_le_bytes(m[..8].try_into().unwrap());
            let object = if p == 0 || p == 0xcccc_cccc_cccc_cccc { None } else {
                // SAFETY: a heap object the call returned.
                Some(unsafe { std::slice::from_raw_parts(p as *const u8, *n) }.to_vec())
            };
            m[..8].copy_from_slice(&[u8::from(object.is_some()); 8]);
            m.extend(object.unwrap_or_default());
        }
        if let (Kind::WideObject, Some(m)) = (k, memory[i].as_mut()) {
            let p = usize::from_le_bytes(m[..8].try_into().unwrap());
            let text = if p == 0 || p == 0xcccc_cccc_cccc_cccc { None } else {
                // SAFETY: a heap string the call returned.
                Some(unsafe { wide_str(p as *const u16) })
            };
            m[..8].copy_from_slice(&[u8::from(text.is_some()); 8]);
            m.extend(text.unwrap_or_default().into_bytes());
        }
    }
    Outcome { result, buffers: memory, record: s.take() }
}

/// The driver answers this fuzzer replays every case under.
fn drivers(keep: &'static [std::ops::Range<usize>]) -> Vec<(&'static str, Box<dyn Fn(&Session)>)> {
    vec![
        ("accepts, pattern body", Box::new(move |s: &Session| s.reply(accepting(move |_, body| {
            for (i, b) in body.iter_mut().enumerate() {
                if !keep.iter().any(|r| r.contains(&i)) {
                    *b = (i as u8).wrapping_mul(13).wrapping_add(3);
                }
            }
        })))),
        ("accepts as sent", Box::new(|s: &Session| s.reply(accepting(|_, _| {})))),
        ("nt failure", Box::new(|s: &Session| s.reply(|_, _| 0xc000_0001_u32 as i32))),
        ("return code", Box::new(|s: &Session| s.reply(|_, b| { b[0x14..0x18].copy_from_slice(&0xc05e_0007u32.to_le_bytes()); 0 }))),
        ("busy then done", Box::new(|s: &Session| {
            let mut n = 0;
            s.reply(move |_, b| { n += 1; b[0x14..0x18].copy_from_slice(&(if n == 1 { 0x8000_0011u32 } else { 0 }).to_le_bytes()); 0 });
        })),
    ]
}

/// Fuzzes export `name` with these argument kinds.
pub fn fuzz(name: &str, kinds: &[Kind], width: Width) {
    fuzz_pair(name, name, kinds, width);
}

/// As `fuzz`; the pattern answer leaves these body bytes as sent (fields
/// the library trusts, such as offsets it reads back).
pub fn fuzz_keeping(name: &str, kinds: &[Kind], width: Width, keep: &'static [std::ops::Range<usize>]) {
    fuzz_impl(name, name, kinds, width, keep);
}

/// As `fuzz`, the reference's `name` against the layer's `layer_name`.
pub fn fuzz_pair(name: &str, layer_name: &str, kinds: &[Kind], width: Width) {
    fuzz_impl(name, layer_name, kinds, width, &[]);
}

fn fuzz_impl(name: &str, layer_name: &str, kinds: &[Kind], width: Width, keep: &'static [std::ops::Range<usize>]) {
    let s = session();
    trace_all(&s);
    let Some(r) = reference::reference(name) else {
        eprintln!("{name}: no reference; skipped");
        return;
    };
    // SAFETY: exports of up to eight register-sized arguments.
    let (r, l): (Any8, Any8) = unsafe { (std::mem::transmute(r), std::mem::transmute(reference::layer(layer_name))) };
    let options: Vec<Vec<Value>> = kinds.iter().map(|&k| values(k)).collect();
    let baseline: Vec<Value> = options.iter().map(|o| o[0].clone()).collect();
    let mut cases = vec![baseline.clone()];
    for (i, o) in options.iter().enumerate() {
        for v in &o[1..] {
            let mut c = baseline.clone();
            c[i] = v.clone();
            cases.push(c);
        }
    }
    for (what, driver) in drivers(keep) {
        for (n, case) in cases.iter().enumerate() {
            if std::env::var_os("VICTORY_FUZZ_TRACE").is_some() {
                eprintln!("{name} case {n} ({what})");
            }
            driver(&s);
            let a = call_case(r, kinds, case, width, &s);
            if std::env::var_os("VICTORY_FUZZ_TRACE").is_some() {
                eprintln!("  reference done");
            }
            driver(&s);
            let b = call_case(l, kinds, case, width, &s);
            if a != b {
                let diff = |x: &Option<Vec<u8>>, y: &Option<Vec<u8>>| match (x, y) {
                    (Some(x), Some(y)) => x.iter().zip(y).position(|(p, q)| p != q),
                    _ => None,
                };
                let first: Vec<_> = a.buffers.iter().zip(&b.buffers).map(|(x, y)| diff(x, y)).collect();
                let calls = |r: &Record| r.calls.iter().map(|c| (c.code, c.input.len())).collect::<Vec<_>>();
                let first_call = a.record.calls.iter().zip(&b.record.calls).map(|(x, y)| x.input.iter().zip(&y.input).position(|(p, q)| p != q)).collect::<Vec<_>>();
                panic!(
                    "{name}, case {n} ({what}): reference result {:#x} layer {:#x}; first differing byte per buffer {first:?}; calls {:?} vs {:?}, first differing input byte {first_call:?}; messages {:?} vs {:?}",
                    a.result, b.result, calls(&a.record), calls(&b.record), a.record.messages, b.record.messages
                );
            }
        }
    }
}

use Kind::*;
use Width::*;

const BYTES: &[u64] = &[1, 0, 2, 0xff, 0x1234];
const FLAG1: &[u64] = &[0, 1, 2, 3, 0x8000_0000, 0xffff_ffff];
const KINDS: &[u64] = &[0, 1, 2, 0xffff_ffff, 0x8000_0000];

#[test]
fn fuzz_fixed_bodies() {
    fuzz("XCrdOddPmAcquireReleaseReference", &[OutOpt(4), Adp, Int(BYTES)], W32);
    fuzz("XCrdIsSideloadedKeyPresent", &[Adp, In(16), OutOpt(1)], W32);
    fuzz("XCrdQueryAttestationBlob", &[Adp, In(16), Out(0x80)], W32);
    fuzz("XCrdQueryWriteStats", &[Adp, Int(KINDS), OutOpt(0x20)], W32);
    fuzz("XCrdRemapReload", &[Adp], W32);
    fuzz("XCrdSetTelemetry", &[Adp, Int(BYTES), Int(BYTES)], W32);
    fuzz("XCrdSetTitleQuirks", &[Adp, Int(FLAG1)], W32);
    fuzz("XCrdStreamingQueryActiveInstanceId", &[Adp, Out(8)], W32);
}

/// The fuzzer finds a difference when there is one.
#[test]
#[should_panic(expected = "XCrdSetTelemetry")]
fn fuzz_catches_a_wrong_port() {
    if reference::reference("XCrdSetTelemetry").is_none() {
        panic!("XCrdSetTelemetry: no reference, nothing to catch");
    }
    fuzz_pair("XCrdSetTelemetry", "XCrdRemapReload", &[Adp, Int(BYTES), Int(BYTES)], W32);
}

#[test]
fn fuzz_streaming_stop() {
    fuzz("XCrdStreamingStop", &[Adp, Int(&[7, 0, u64::MAX]), Int(&[0, 1, 0x8000_0000_0000_0000])], W32);
}
#[test]
fn fuzz_io_balancer() {
    fuzz("XCrdSuspendResumeIoBalancer", &[Adp, Int(BYTES)], W32);
}
#[test]
fn fuzz_xpf_op() {
    fuzz("XCrdXpfOp", &[Adp, Int(&[1, 0, 2, 3, 0xffff_ffff]), Int(&[0, 1]), Int(&[0, 0x10])], W32);
}
const SIZES32: &[u64] = &[0x10, 0, 1, 0x20, 0x21, 0xffff_ffff];
#[test]
fn fuzz_derive_key() {
    fuzz("XCrdDeriveMsixvc2Key", &[Adp, InOpt(0x40), Int(SIZES32), Int(&[5, 0, 0xffff_ffff]), InOpt(0x40), Int(&[0x20, 0, 0x40, 0x41]), OutOpt(0x40), Int(SIZES32)], W32);
}
#[test]
fn fuzz_encrypt_buffer() {
    fuzz_keeping("XCrdEncryptBuffer", &[Adp, InOpt(0x2000), Int(&[0x1000, 0, 0x800, 0x2000, 0x1001]), Int(&[0x3000, 0, 0x10, u64::MAX & !0xfff]), InOpt(32), InOpt(16)], W32, &[0x3c..0x40]);
}
#[test]
fn fuzz_insert_region() {
    fuzz("XCrdStreamingQueueInsertRegion", &[Adp, Int(&[9, 0]), Int(&[3, 0, 1, 64]), In(0x100), Int(&[0, 1])], W32);
}


#[test]
fn fuzz_path_bodies() {
    fuzz("XCrdDeleteXVD", &[Adp, Path, Int(FLAG1)], W32);
    fuzz("XCrdEnableSharedPls", &[Adp, Path, In(16)], W32);
    fuzz("XCrdGrowDynamicXvd", &[Adp, Path, Int(&[0x1000, 0, u64::MAX])], W32);
    fuzz("XCrdIsCorrupt", &[Adp, Path, OutOpt(1)], W32);
    fuzz("XCrdXBlobDelete", &[Adp, Path], W32);
    fuzz("XCrdXBlobCreate", &[Adp, Path, Int(&[0x1000, 0, u64::MAX]), Int(&[0, 1])], W32);
}

#[test]
fn fuzz_corrupt_xvc() {
    // The driver's string must end inside the body (the library reads on).
    fuzz_keeping("XCrdQueryCorruptXvc", &[Adp, WideObject], W32, &[0x40..0x42]);
}

#[test]
fn is_corrupt_not_found_is_not_corrupt() {
    let s = session();
    let (r, l) = both!("XCrdIsCorrupt": extern "system" fn(*const Adapter, *const u16, *mut u8) -> HRESULT);
    let a = adapter();
    let p = wz("[XUC:]\\x");
    // STATUS_NOT_FOUND -> ERROR_NOT_FOUND (0x80070490): S_OK, not corrupt.
    same("not found", run(&s, |s| s.reply(|_, b| { b[0x14..0x18].copy_from_slice(&0xc000_0225u32.to_le_bytes()); 0 }), |rf| {
        let mut c = 9;
        ((if rf { r(&a, p.as_ptr(), &mut c) } else { l(&a, p.as_ptr(), &mut c) }), c)
    }));
}

const EA: &[u64] = &[1, 0, 2, 4, 8, 9, 0x10, 0x8000_0000, 0x8000_0009, 0xffff_ffff];

#[test]
fn fuzz_path_bodies_2() {
    fuzz("XCrdDefragXvc", &[Adp, Path, Int(FLAG1), OutOpt(4), OutOpt(4), OutOpt(4)], W32);
    fuzz("XCrdCopyExtendedAttributes", &[Adp, Path, Path, Int(EA)], W32);
    fuzz("XCrdMoveSidecarFiles", &[Adp, Path, Path, Int(FLAG1)], W32);
    fuzz("XCrdSetExtendedAttributes", &[Adp, Path, Int(EA), Int(&[0x55, 0, u64::MAX])], W32);
    fuzz("XCrdSetEncryptedEa", &[Adp, Path, Int(&[0x55, 0]), InOpt(32), InOpt(16)], W32);
}

#[test]
fn fuzz_remap_test() {
    fuzz("XCrdRemapTest", &[Adp, Path, OutOpt(0x100), Int(&[0x40, 0, 1, 2, 0x100])], W32);
}

#[test]
fn repair_xvc_every_kind() {
    let s = session();
    trace_all(&s);
    let (r, l) = both!("XCrdRepairXvc": extern "system" fn(*const Adapter, *const super::paths::Repair) -> HRESULT);
    let a = adapter();
    let (x, y) = (wz("[XUC:]\\a.xvc"), wz("[XTE:]\\src"));
    for kind in [0u32, 1, 2, 3, 4, 5, 6, 0xffff_ffff] {
        for flags in [0u32, 1, 2, 3, 4] {
            for (xvc, src) in [(true, true), (true, false), (false, true), (false, false)] {
                let info = super::paths::Repair {
                    kind,
                    xvc: if xvc { x.as_ptr() } else { std::ptr::null() },
                    source: if src { y.as_ptr() } else { std::ptr::null() },
                    value: 0x77,
                    flags,
                };
                for (what, driver) in drivers(&[]) {
                    same(&format!("kind {kind} flags {flags} {xvc} {src} {what}"), run(&s, |s| driver(s), |rf| if rf { r(&a, &info) } else { l(&a, &info) }));
                }
            }
        }
    }
    same("null info", run(&s, |_| {}, |rf| if rf { r(&a, std::ptr::null()) } else { l(&a, std::ptr::null()) }));
}

const SFLAGS: &[u64] = &[1, 0, 8, 9, 0x1_0000, 0x1_0009, 2, 0x2_0000, 0x1_ffff];

#[test]
fn fuzz_streaming_queries() {
    fuzz("XCrdSetFlatContentPath", &[Adp, Path, Path], W32);
    fuzz("XCrdStorageQueryBlobAttributes", &[Adp, Path, OutOpt(0x34)], W32);
    fuzz("XCrdStreamingAdjustStreamingFlags", &[Adp, Int(&[5, 0]), Int(SFLAGS), Int(SFLAGS)], W32);
    fuzz("XCrdStreamingQueryInformationEx", &[Out(0x108), Out(0x30), Adp, Int(&[0x44, 0, u64::MAX])], W32);
    fuzz("XCrdStreamingQueryInformation", &[Out(0x108), Adp, Int(&[0x44, 0, u64::MAX])], W32);
    fuzz("XCrdStreamingQueryInformationByPath", &[Out(0x108), Adp, Path, Int(FLAG1)], W32);
}

const COUNTS: &[u64] = &[3, 0, 1, 8];

#[test]
fn fuzz_regions() {
    fuzz("XCrdStreamingQueryRegionIdByOffset", &[Out(0x20), Adp, Path, Int(&[0, 1, 2, 0xffff_ffff]), Int(&[0x1000, 0])], W32);
    fuzz("XCrdStreamingQueryRegionInformation", &[OutOpt(0x180), Adp, Int(&[5, 0]), Int(COUNTS), InOpt(32)], W32);
    // A path past the limit alone makes the library halve its chunk to 0
    // and loop for good (0x180008bd5); not a case to run.
    fuzz("XCrdStreamingQueryRegionInformationByPath", &[OutOpt(0x180), Adp, PathShort, Int(COUNTS), InOpt(32)], W32);
}

#[test]
fn fuzz_specifiers() {
    fuzz("XCrdStreamingQueryRegionSpecifiers", &[OutOpt(0x188 * 8), OutOpt(4), Adp, Int(&[0x1234, 0]), Int(COUNTS)], W32);
    fuzz("XCrdStreamingQueryRegionSpecifiersByPath", &[OutOpt(0x188 * 8), OutOpt(4), Adp, Path, Int(COUNTS)], W32);
}

#[test]
fn fuzz_region_queue() {
    let kinds: &[u64] = &[0, 1, 3, 4];
    fuzz("XCrdStreamingQueueQueryRegionList", &[OutOpt(64), Out(4), Adp, Int(&[7, 0]), Int(&[4, 0, 1, 16]), Int(kinds)], W32);
    fuzz("XCrdStreamingQueueQueryRegionListByPath", &[OutOpt(64), Out(4), Adp, Path, Int(&[4, 0, 1, 16]), Int(kinds)], W32);
}

const IDS: &[u64] = &[0x13, 0, 1, 0xb, 0x12, 0x1a, 0x1b, 0x24, 0x25, 0xffff_ffff];

#[test]
fn fuzz_storage_devices() {
    fuzz("XCrdFindStorageDevice", &[Adp, In(16), OutOpt(4)], W32);
    fuzz("XCrdGetDeviceIdMapping", &[Adp, InOpt(16), OutOpt(16)], W32);
    fuzz("XCrdSetDeviceIdMapping", &[Adp, InOpt(16), InOpt(16)], W32);
    fuzz("XCrdSetEraLaunchMapping", &[Adp, Path, Int(IDS)], W32);
    fuzz("XCrdSetExternalPlsMapping", &[Adp, Path, Int(IDS)], W32);
    fuzz("XCrdStorageAttachExternalDevice", &[Out(8), Adp, Int(IDS), Int(BYTES)], W32);
    fuzz("XCrdStorageCheck", &[Adp, Int(IDS), Int(&[0, 1, 2, 3, 4])], W32);
    fuzz("XCrdStorageConnectDevice", &[Adp, Path], W32);
    fuzz("XCrdStorageCreateBlob", &[Adp, Path, Int(&[0x1000, 0]), Int(&[0, 1])], W32);
    fuzz("XCrdStorageCreateDirectory", &[Adp, Path, Int(&[0, 1])], W32);
    fuzz("XCrdStorageDeleteBlob", &[Adp, Path], W32);
    fuzz("XCrdStorageDeleteDirectory", &[Adp, Path], W32);
    fuzz("XCrdStorageFormat", &[Adp, Int(IDS), Int(FLAG1), Path, OutOpt(16)], W32);
    fuzz("XCrdStorageMoveBlob", &[Adp, Path, Path, Int(&[0, 1])], W32);
    fuzz("XCrdStorageRemapDevice", &[Adp, Int(IDS), Int(IDS)], W32);
    fuzz("XCrdStorageSetConsoleAssociation", &[Adp, Int(IDS), Int(&[1, 0, 2, 3, 0xffff_ffff])], W32);
    fuzz("XCrdStorageSetDeviceName", &[Adp, Int(IDS), Path], W32);
    fuzz("XCrdStorageSetLastUsedConsole", &[Adp, Int(IDS)], W32);
}

#[test]
fn fuzz_disconnect_device() {
    // The volume path must end inside the answer (the library reads on).
    fuzz_keeping("XCrdStorageDisconnectDevice", &[Adp, Int(IDS), OutOpt(0x200), InOpt(4)], W32, &[0x40..0x42]);
    fuzz_keeping("XCrdStorageDisconnectDevice", &[Adp, Int(IDS), OutOpt(0x200), Int(&[0])], W32, &[0x40..0x42]);
}

#[test]
fn storage_refuses_internal_devices() {
    let s = session();
    trace_all(&s);
    let a = adapter();
    let (rf, lf) = both!("XCrdStorageFormat": extern "system" fn(*const Adapter, u32, u32, *const u16, *mut [u8; 16]) -> HRESULT);
    let (rc, lc) = both!("XCrdStorageCheck": extern "system" fn(*const Adapter, u32, u32) -> HRESULT);
    let name = wz("Disk");
    for (g, _) in INTERNAL {
        for flags in [0, 1] {
            same("format internal", run(&s, |s| s.reply(accepting(|_, _| {})), |r| {
                let mut guid = g;
                ((if r { rf(&a, 0x13, flags, name.as_ptr(), &mut guid) } else { lf(&a, 0x13, flags, name.as_ptr(), &mut guid) }), guid)
            }));
        }
    }
    for id in [1u32, 0x24] {
        same("check internal", run(&s, |s| s.reply(accepting(|_, _| {})), |r| if r { rc(&a, id, 1) } else { lc(&a, id, 1) }));
    }
}


/// The blob driver: data a pattern, the count it read as asked, or short,
/// or "buffer too small".
fn blob_driver(mode: u8) -> impl FnMut(&reference::Call, &mut [u8]) -> i32 + Send + 'static {
    move |_, b| {
        b[0x14..0x18].fill(0);
        let body = &mut b[0x1c..];
        let path = u32::from_le_bytes(body[8..12].try_into().unwrap()) as usize;
        let n = u32::from_le_bytes(body[0xc..0x10].try_into().unwrap()) as usize;
        let start = 0x20 + path;
        for (i, x) in body[start..].iter_mut().enumerate().take(n) {
            *x = (i as u8).wrapping_mul(29).wrapping_add(7);
        }
        match mode {
            1 => body[0xc..0x10].copy_from_slice(&((n / 2) as u32).to_le_bytes()),
            2 => {
                b[0x14..0x18].copy_from_slice(&0xc000_0023u32.to_le_bytes());
                b[0x1c + 0xc..0x1c + 0x10].copy_from_slice(&7u32.to_le_bytes());
            }
            _ => {}
        }
        0
    }
}

#[test]
fn blobs_read_and_write() {
    let s = session();
    trace_all(&s);
    let (rr, lr) = both!("XCrdStorageReadBlobEx": extern "system" fn(*const Adapter, *const u16, u64, *mut u8, *mut u32) -> HRESULT);
    let (rw, lw) = both!("XCrdStorageWriteBlobEx": extern "system" fn(*const Adapter, *const u16, u64, *const u8, u32, u32) -> HRESULT);
    let (rr0, lr0) = both!("XCrdStorageReadBlob": extern "system" fn(*const Adapter, *const u16, *mut u8, *mut u32) -> HRESULT);
    let (rw0, lw0) = both!("XCrdStorageWriteBlob": extern "system" fn(*const Adapter, *const u16, *const u8, u32) -> HRESULT);
    let a = adapter();
    let long = "z".repeat(0x7f900);
    let paths = [Some(wz("[XUC:]\\blob")), Some(wz(&long)), None];
    let data: Vec<u8> = (0..0x30_0000).map(|i| (i % 251) as u8).collect();
    for len in [0u32, 0x10, 0x1234, 0x20_0000] {
        for path in &paths {
            // Past 0xff000 bytes of path the library sends a full 1 MiB
            // chunk whatever is left, so a shorter length wraps its count and
            // reads or writes past the buffer (0x180007e4e, 0x1800081d1):
            // the port does the same; not a case to run.
            if path.as_ref().is_some_and(|p| p.len() > 0x7f000) && len != 0 && len < 0x10_0000 {
                continue;
            }
            let pp = path.as_ref().map_or(std::ptr::null(), |p| p.as_ptr());
            for mode in [0u8, 1, 2] {
                for with_buffer in [true, false] {
                    same(&format!("read {len:#x} {mode} {with_buffer} {}", path.as_ref().map_or(0, |p| p.len())), run(&s, |s| s.reply(blob_driver(mode)), |r| {
                        let mut buf = vec![0xccu8; 0x30_0000];
                        let b = if with_buffer { buf.as_mut_ptr() } else { std::ptr::null_mut() };
                        let mut n = len;
                        let hr = if r { rr(&a, pp, 0x5000, b, &mut n) } else { lr(&a, pp, 0x5000, b, &mut n) };
                        let mut m = len;
                        let hr0 = if r { rr0(&a, pp, b, &mut m) } else { lr0(&a, pp, b, &mut m) };
                        (hr, n, hr0, m, buf)
                    }));
                }
                for flags in [0u32, 1, 2, 3, 4] {
                    same(&format!("write {len:#x} {mode} {flags}"), run(&s, |s| s.reply(blob_driver(mode)), |r| {
                        let hr = if r { rw(&a, pp, 0x5000, data.as_ptr(), len, flags) } else { lw(&a, pp, 0x5000, data.as_ptr(), len, flags) };
                        let hr0 = if r { rw0(&a, pp, data.as_ptr(), len) } else { lw0(&a, pp, data.as_ptr(), len) };
                        (hr, hr0)
                    }));
                }
            }
        }
    }
    let (rq, lq) = both!("XCrdStorageQueryExternalDeviceAddress": extern "system" fn(*mut [u8; 8], u32) -> HRESULT);
    for v in [0u32, 1, 0x1234_5678, u32::MAX] {
        same("address", run(&s, |_| {}, |r| { let mut o = [0xcc; 8]; ((if r { rq(&mut o, v) } else { lq(&mut o, v) }), o) }));
    }
}

/// A small deterministic generator for header fields.
struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
    /// Interesting values: small, page edges, big, random.
    fn value(&mut self) -> u64 {
        match self.next() % 8 {
            0 => 0,
            1 => self.next() % 8,
            2 => 0x1000 * (self.next() % 64),
            3 => 0x1000 * (self.next() % 64) + 1,
            4 => 0xa_a000 * (self.next() % 300) + self.next() % 3,
            5 => u64::from(u32::MAX) - self.next() % 4,
            6 => self.next() >> (self.next() % 64),
            _ => self.next(),
        }
    }
}

fn put(b: &mut [u8], at: usize, v: &[u8]) {
    b[at..at + v.len()].copy_from_slice(v);
}

#[test]
fn headers_against_the_library() {
    let s = session();
    trace_all(&s);
    let (rm, lm) = both!("XCrdGetStreamingRoamableXvdConsoleMode": extern "system" fn(*const u8, *mut u16) -> HRESULT);
    let (ro, lo) = both!("XCrdGetXvcOffsetFromHeader": extern "system" fn(*const u8, *mut u64) -> HRESULT);
    let (rb, lb) = both!("XCrdGetUabBitmapOffset": extern "system" fn(*const u8, u64, u8, *mut *const u8, *mut u32, *mut *const u8, *mut u32) -> HRESULT);
    let (rv, lv) = both!("XCrdValidateUab": extern "system" fn(*const u8, u64, u8, *const u8) -> HRESULT);
    let mut rng = Rng(0x9e37_79b9_7f4a_7c15);
    let (mut modes, mut offsets, mut valid) = (0, 0, 0);
    for i in 0..4000 {
        let mut h = vec![0u8; 0x1000];
        for chunk in h.chunks_mut(8).take(0x60) {
            let v = rng.value();
            chunk.copy_from_slice(&v.to_le_bytes());
        }
        if i % 7 != 0 {
            put(&mut h, 0, b"msft-xvd");
        }
        put(&mut h, 8, &((rng.next() as u32) & if i % 2 == 0 { 0x204 } else { u32::MAX }).to_le_bytes());
        put(&mut h, 0x80, &((rng.next() % 3) as u32).to_le_bytes());
        put(&mut h, 0x84, &((rng.next() % 0x2c) as u32).to_le_bytes());
        for at in [0x88usize, 0x8c, 0x90, 0x94] {
            put(&mut h, at, &(rng.value() as u32).to_le_bytes());
        }
        put(&mut h, 0x18, &rng.value().to_le_bytes());
        h[0x270] = rng.next() as u8;
        h[0x276] = (rng.next() % 6) as u8;
        let mut o = 0u64;
        modes += i32::from(rm(h.as_ptr(), &mut 0) == 0);
        offsets += i32::from(ro(h.as_ptr(), &mut o) == 0);
        s.take();
        same(&format!("mode {i}"), run(&s, |_| {}, |r| { let mut m = 0xcccc; ((if r { rm(h.as_ptr(), &mut m) } else { lm(h.as_ptr(), &mut m) }), m) }));
        same(&format!("offset {i}"), run(&s, |_| {}, |r| { let mut o = 0xcccc; ((if r { ro(h.as_ptr(), &mut o) } else { lo(h.as_ptr(), &mut o) }), o) }));
        // UAB data: magic, sizes and the duplicated layout, sometimes right.
        let mut u = vec![0u8; 0x1000];
        put(&mut u, 0x200, if i % 9 == 0 { b"MS-XUAX " } else { b"MS-XUAB " });
        let total = rng.value() % 0x20_0000_0000;
        let meta = total.min(rng.value() % 0x8_0000_0000);
        put(&mut u, 0x2a0, &total.to_le_bytes());
        put(&mut u, 0x2c0, &meta.to_le_bytes());
        let md = (meta.wrapping_add(0xa_9fff) / 0xa_a000 + 7) >> 3;
        let data = (total.wrapping_sub(meta).wrapping_add(0xa_9fff) / 0xa_a000 + 7) >> 3;
        let pages = ((md + data + 0xfff) & !0xfff) as u32;
        let doubled = u64::from(pages) * 2 + 0x1000;
        let double = rng.next() % 2 == 0;
        put(&mut u, 0x208, &(if i % 11 == 0 { 0x2000u64 } else { 0x1000 }).to_le_bytes());
        put(&mut u, 0x218, &(if i % 13 == 0 { pages + 1 } else { pages }).to_le_bytes());
        if double {
            put(&mut u, 0x21c, &pages.to_le_bytes());
            put(&mut u, 0x210, &(0x1000 + u64::from(pages)).to_le_bytes());
            put(&mut u, 0x284, &(pages * 2 + 0x1000).to_le_bytes());
        } else {
            put(&mut u, 0x284, &(pages + 0x1000).to_le_bytes());
        }
        let size = match rng.next() % 4 { 0 => doubled, 1 => 0x800, 2 => u64::from(pages) + 0x1000, _ => rng.value() };
        let mut info = vec![0u8; 0x100];
        put(&mut info, 0x78, &(if i % 5 == 0 { total + 1 } else { total }).to_le_bytes());
        let id: [u8; 16] = std::array::from_fn(|k| (k as u8).wrapping_mul(i as u8));
        put(&mut info, 0x30, &id);
        put(&mut info, 0x40, &id);
        put(&mut u, 0x220, &id);
        put(&mut u, 0x230, &id);
        put(&mut u, 0x240, &id);
        if i % 17 == 0 {
            u[0x245] ^= 1;
        }
        for strict in [0u8, 1] {
            valid += i32::from(rv(u.as_ptr(), size, strict, info.as_ptr()) == 0);
            s.take();
            same(&format!("validate {i} {strict}"), run(&s, |_| {}, |r| if r { rv(u.as_ptr(), size, strict, info.as_ptr()) } else { lv(u.as_ptr(), size, strict, info.as_ptr()) }));
        }
        for alt in [0u8, 1] {
            same(&format!("bitmaps {i}"), run(&s, |_| {}, |r| {
                let (mut a, mut b) = (std::ptr::null(), std::ptr::null());
                let (mut al, mut bl) = (0u32, 0u32);
                let hr = if r { rb(u.as_ptr(), size, alt, &mut a, &mut al, &mut b, &mut bl) } else { lb(u.as_ptr(), size, alt, &mut a, &mut al, &mut b, &mut bl) };
                (hr, (a as usize).wrapping_sub(u.as_ptr() as usize), al, (b as usize).wrapping_sub(u.as_ptr() as usize), bl)
            }));
        }
    }
    // The cases reach the success paths, not only the early refusals.
    assert!(modes > 100 && offsets > 300 && valid > 100, "modes {modes} offsets {offsets} valid {valid}");
    // The null checks, in order.
    let h = [0u8; 0x300];
    same("mode null header", run(&s, |_| {}, |r| { let mut m = 0u16; if r { rm(std::ptr::null(), &mut m) } else { lm(std::ptr::null(), &mut m) } }));
    same("mode null out", run(&s, |_| {}, |r| if r { rm(h.as_ptr(), std::ptr::null_mut()) } else { lm(h.as_ptr(), std::ptr::null_mut()) }));
    same("offset null", run(&s, |_| {}, |r| if r { ro(h.as_ptr(), std::ptr::null_mut()) } else { lo(h.as_ptr(), std::ptr::null_mut()) }));
    same("validate null info", run(&s, |_| {}, |r| if r { rv(h.as_ptr(), 0x1000, 0, std::ptr::null()) } else { lv(h.as_ptr(), 0x1000, 0, std::ptr::null()) }));
}

/// Prints, for an information export's fixed classes, which answer bytes
/// land where in the caller's structure (as copy runs `dst <- src, len`),
/// by answering with bodies whose bytes name their own offsets.
fn learn(name: &str, classes: &[(u32, usize)], call: &dyn Fn(usize, u32, *mut u8, u32) -> HRESULT) {
    let s = session();
    let _ = reference::reference(name).expect("reference");
    for &(class, size) in classes {
        // Room past the minimum, to see writes beyond it.
        let room = size + 0x200;
        let mut seen: Vec<[Option<u8>; 2]> = vec![[None; 2]; room];
        for half in 0..2 {
            let mut outs = Vec::new();
            for fill in [0xccu8, 0x33] {
                s.reply(accepting(move |_, body| {
                    for (i, b) in body.iter_mut().enumerate() {
                        if i >= 0x10 {
                            *b = if half == 0 { i as u8 } else { (i >> 8) as u8 };
                        }
                    }
                }));
                let mut out = vec![fill; room];
                let hr = call(0, class, out.as_mut_ptr(), size as u32);
                assert!(hr >= 0, "{name} class {class}: {hr:#x}");
                outs.push(out);
                s.take();
            }
            for o in 0..room {
                let (x, y) = (outs[0][o], outs[1][o]);
                seen[o][half] = if x != 0xcc { Some(x) } else if y != 0x33 { Some(y) } else { None };
            }
        }
        let mut runs: Vec<(usize, usize, usize)> = Vec::new();
        for (o, v) in seen.iter().enumerate() {
            if let [Some(lo), Some(hi)] = v {
                let src = usize::from(*lo) | usize::from(*hi) << 8;
                match runs.last_mut() {
                    Some((d, sr, n)) if *d + *n == o && *sr + *n == src => *n += 1,
                    _ => runs.push((o, src, 1)),
                }
            }
        }
        let text: Vec<String> = runs.iter().map(|(d, s, n)| format!("({d:#x}, {s:#x}, {n:#x})")).collect();
        eprintln!("MAP {name} {class:#x} {size:#x}: [{}]", text.join(", "));
    }
}

#[test]
#[ignore = "prints the reference's field maps"]
fn learn_xvd_information() {
    let Some(r) = reference::reference("XCrdQueryXvdInformation") else { return };
    let f: extern "system" fn(*const Adapter, *const u16, u32, *mut u8, u32) -> HRESULT = unsafe { std::mem::transmute(r) };
    let a = adapter();
    let p = wz("[XUC:]\\x");
    let classes = [(0, 0x80), (2, 8), (3, 0x80), (4, 8), (5, 0x18), (6, 1), (7, 0x168), (8, 0x158), (9, 1), (0xa, 0x40), (0xb, 0x150), (0xc, 1), (0xd, 0x1f8), (0xe, 0x1f8), (0xf, 0x1f8), (0x10, 0x1f8), (0x11, 1), (0x12, 8), (0x14, 0x40)];
    learn("XCrdQueryXvdInformation", &classes, &|_, c, o, n| f(&a, p.as_ptr(), c, o, n));
}

/// Prints, for a setter's classes, which bytes of the caller's structure
/// land where in the body sent (`body <- info, len`), and the body's other
/// non-zero bytes before `upto`.
fn learn_set(name: &str, classes: &[(u32, usize)], upto: usize, call: &dyn Fn(u32, *const u8, u32) -> HRESULT) {
    let s = session();
    for &(class, size) in classes {
        let mut bodies = Vec::new();
        for half in 0..2 {
            let info: Vec<u8> = (0..0x400usize).map(|i| if half == 0 { i as u8 } else { (i >> 8) as u8 ^ 0x80 }).collect();
            s.reply(accepting(|_, _| {}));
            let hr = call(class, info.as_ptr(), size as u32);
            let r = s.take();
            let body = r.calls.first().map(|c| c.input[0x1c..].to_vec()).unwrap_or_default();
            eprintln!("  {name} class {class:#x}: hr {hr:#x}, body {:#x} bytes", body.len());
            bodies.push(body);
        }
        let (a, b) = (&bodies[0], &bodies[1]);
        let mut runs: Vec<(usize, usize, usize)> = Vec::new();
        let mut consts = Vec::new();
        for o in 0..a.len().min(b.len()).min(upto) {
            if a[o] != b[o] {
                let src = usize::from(a[o]) | usize::from(b[o] ^ 0x80) << 8;
                match runs.last_mut() {
                    Some((d, sr, n)) if *d + *n == o && *sr + *n == src => *n += 1,
                    _ => runs.push((o, src, 1)),
                }
            } else if a[o] != 0 {
                consts.push((o, a[o]));
            }
        }
        let text: Vec<String> = runs.iter().map(|(d, s, n)| format!("({d:#x}, {s:#x}, {n:#x})")).collect();
        eprintln!("SET {name} {class:#x}: [{}] consts {consts:x?}", text.join(", "));
    }
}

#[test]
#[ignore = "prints the reference's field maps"]
fn learn_set_xvd_information() {
    let Some(r) = reference::reference("XCrdSetXvdInformation") else { return };
    let f: extern "system" fn(*const Adapter, *const u16, u32, *const u8, u32) -> HRESULT = unsafe { std::mem::transmute(r) };
    let a = adapter();
    let p = wz("[XUC:]\\x");
    let classes = [(0, 8), (1, 0x10), (2, 0x10), (3, 0), (5, 4), (6, 4), (7, 4), (0xa, 8), (0xb, 8)];
    learn_set("XCrdSetXvdInformation", &classes, 0x110, &|c, i, n| f(&a, p.as_ptr(), c, i, n));
}

const XVD_CLASSES: &[u64] = &[0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 0xa, 0xb, 0xc, 0xd, 0xe, 0xf, 0x10, 0x11, 0x12, 0x13, 0x14, 0x15, 0xffff_ffff, 0x8000_0000];
const INFO_SIZES: &[u64] = &[0x800, 0, 1, 8, 0x10, 0x54, 0x7f, 0x1f8, 0x20a];

#[test]
fn fuzz_xvd_information() {
    for class in XVD_CLASSES {
        let c: &'static [u64] = Box::leak(Box::new([*class]));
        // The answer's string (class 0x13) ends inside it.
        // Class 1's count (+0x10) is kept small elsewhere: the library copies
        // that many entries whatever the buffer.
        fuzz_keeping("XCrdQueryXvdInformation", &[Adp, Path, Int(c), OutOpt(0x800), Int(INFO_SIZES)], W32, &[0x10..0x12, 0x100..0x102]);
        fuzz("XCrdSetXvdInformation", &[Adp, Path, Int(c), InOpt(0x800), Int(INFO_SIZES)], W32);
    }
    fuzz("XCloudQueryFeatureBits", &[Adp, Out(8)], W32);
    fuzz("XCloudSetFeatureBits", &[Adp, In(8)], W32);
}

#[test]
fn xvd_information_entry_counts() {
    let s = session();
    let (r, l) = both!("XCrdQueryXvdInformation": extern "system" fn(*const Adapter, *const u16, i32, *mut u8, u32) -> HRESULT);
    let a = adapter();
    let p = wz("[XUC:]\\x");
    for n in [0u16, 1, 2, 0x3f, 0x40] {
        same(&format!("{n} entries"), run(&s, |s| s.reply(accepting(move |_, body| {
            for (i, b) in body.iter_mut().enumerate().skip(0x12) {
                *b = (i as u8).wrapping_mul(13);
            }
            body[0x10..0x12].copy_from_slice(&n.to_le_bytes());
        })), |side| {
            let mut out = vec![0xccu8; 0x800];
            ((if side { r(&a, p.as_ptr(), 1, out.as_mut_ptr(), 0x402) } else { l(&a, p.as_ptr(), 1, out.as_mut_ptr(), 0x402) }), out)
        }));
    }
}


#[test]
#[ignore = "prints the reference's field maps"]
fn learn_xrsc_information() {
    let Some(r) = reference::reference("XCrdQueryXRscInformation") else { return };
    let f: extern "system" fn(*const Adapter, *const u16, u32, *mut u8, u32) -> HRESULT = unsafe { std::mem::transmute(r) };
    let a = adapter();
    let p = wz("[XUC:]\\x");
    learn("XCrdQueryXRscInformation", &[(0, 0xb8), (1, 0x90)], &|_, c, o, n| f(&a, p.as_ptr(), c, o, n));
}

#[test]
fn fuzz_xrsc_and_crd_information() {
    let classes: &[u64] = &[0, 1, 2, 3, 4, 5, 6, 0xffff_ffff];
    let ids: &[u64] = &[3, 0, 0x24, 0x25, 0xffff_ffff];
    fuzz("XCrdQueryXRscInformation", &[Adp, Path, Int(classes), OutOpt(0x800), Int(INFO_SIZES)], W32);
    for c in classes {
        let c: &'static [u64] = Box::leak(Box::new([*c]));
        fuzz("XCrdSetXRscInformation", &[Adp, Int(c), InOpt(0x40), Int(&[8, 0, 3, 4])], W32);
        fuzz("XCrdQueryXRscInformation", &[Adp, Path, Int(c), OutOpt(0x800), Int(INFO_SIZES)], W32);
        // Class 1's strings are placed by (offset, length) pairs at +0x4c.
        fuzz_keeping("XCrdQueryCrdInformation", &[Adp, Int(ids), Int(c), OutOpt(0x800), Int(&[0x670, 0, 1, 2, 0x10])], W32, &[0x4c..0x64]);
    }
    let flags: &[u64] = &[0, 1, 2, 0xff];
    fuzz_keeping("XCrdQueryCrdLogInformation", &[Adp, Int(ids), Int(BYTES), Int(&[7, 0]), Int(BYTES), Int(flags), OutOpt(0x100), Int(&[0x40, 0, 1])], W32, &[0x24..0x2c]);
    fuzz_keeping("XCrdQueryCrdVendorInformation", &[Adp, Int(ids), Int(BYTES), Int(&[7, 0]), Int(&[1, 0, 0xffff]), Int(&[2, 0]), OutOpt(0x100), Int(&[0x40, 0, 1])], W32, &[0x1c..0x24]);
}

#[test]
fn crd_information_strings() {
    let s = session();
    let (r, l) = both!("XCrdQueryCrdInformation": extern "system" fn(*const Adapter, i32, i32, *mut u8, u32) -> HRESULT);
    let a = adapter();
    for (off, len) in [(0x80u32, 0x20u32), (0, 0x20), (0x80, 0x104), (0x80, 0x103), (0x80, 0)] {
        same(&format!("{off:#x} {len:#x}"), run(&s, |s| s.reply(accepting(move |_, body| {
            for (i, b) in body.iter_mut().enumerate().skip(0x10) {
                *b = (i as u8).wrapping_mul(7);
            }
            for f in [0x4c, 0x54, 0x5c] {
                body[f..f + 4].copy_from_slice(&off.to_le_bytes());
                body[f + 4..f + 8].copy_from_slice(&len.to_le_bytes());
            }
        })), |side| {
            let mut out = vec![0xccu8; 0x800];
            ((if side { r(&a, 3, 1, out.as_mut_ptr(), 0x670) } else { l(&a, 3, 1, out.as_mut_ptr(), 0x670) }), out)
        }));
    }
}

#[test]
fn fuzz_blob_copy_and_trim() {
    fuzz("XCrdXBlobCopy", &[Adp, Path, Path, Int(&[0, 1]), Int(&[0, 1]), Int(&[0, 1, 2, 3, 4, 0xffff_ffff]), OutOpt(8)], W32);
    fuzz("XCrdTrimXvd", &[Adp, Path, Int(FLAG1), OutOpt(8), OutOpt(1)], W32);
}

#[test]
fn trim_xvc_with_specifiers() {
    let s = session();
    trace_all(&s);
    let (r, l) = both!("XCrdTrimXvc": extern "system" fn(*const Adapter, *const u16, u32, *const [*const u16; 2], u32, *mut u64, *mut u8) -> HRESULT);
    let a = adapter();
    let path = wz("[XUC:]\\pkg.xvc");
    let words: Vec<Vec<u16>> = ["", "a", "region", "lang-en-US", &"q".repeat(0x300)].iter().map(|w| wz(w)).collect();
    let specs: Vec<[*const u16; 2]> = (0..6).map(|i| [words[i % 5].as_ptr(), words[(i * 3 + 1) % 5].as_ptr()]).collect();
    for count in [0u32, 1, 2, 6] {
        for with_specs in [true, false] {
            for flags in [0u32, 1, 2] {
                for (what, driver) in drivers(&[]) {
                    same(&format!("{count} {with_specs} {flags} {what}"), run(&s, |s| driver(s), |side| {
                        let sp = if with_specs { specs.as_ptr() } else { std::ptr::null() };
                        let (mut t, mut d) = (0xccccu64, 0xccu8);
                        let hr = if side { r(&a, path.as_ptr(), count, sp, flags, &mut t, &mut d) } else { l(&a, path.as_ptr(), count, sp, flags, &mut t, &mut d) };
                        let hr2 = if side { r(&a, path.as_ptr(), count, sp, flags, std::ptr::null_mut(), std::ptr::null_mut()) } else { l(&a, path.as_ptr(), count, sp, flags, std::ptr::null_mut(), std::ptr::null_mut()) };
                        (hr, t, d, hr2)
                    }));
                }
            }
        }
    }
}

#[test]
fn fuzz_sparse_cache() {
    fuzz("XCrdPopulateSparseCacheBlocks", &[Adp, Path, InOpt(0x40), Int(&[0x10, 0, 0x40]), InOpt(0x40), Int(&[0x20, 0, 0x40]), In(16), OutOpt(4)], W32);
}

/// The XVD driver: data a pattern; the count (+0x20 for the table, +0xc for
/// user data) as asked, or "buffer too small".
#[test]
fn xvd_reads() {
    let s = session();
    trace_all(&s);
    let (rb, lb) = both!("XCrdReadBatInfoXVD": extern "system" fn(*const Adapter, *const u16, *mut u8, *mut u32, *mut [u8; 0x34]) -> HRESULT);
    let (ru, lu) = both!("XCrdReadUserDataXVD": extern "system" fn(*const Adapter, *const u16, u64, *mut u8, *mut u32) -> HRESULT);
    let a = adapter();
    let path = wz("[XUC:]\\a.xvd");
    for small in [false, true] {
        let driver = move |s: &Session| s.reply(move |_, b| {
            b[0x14..0x18].fill(0);
            for (i, x) in b[0x1c + 0x10..].iter_mut().enumerate() {
                if !(0x10..0x14).contains(&i) {
                    *x = (i as u8).wrapping_mul(31).wrapping_add(5);
                }
            }
            if small {
                b[0x14..0x18].copy_from_slice(&0xc000_0023u32.to_le_bytes());
            }
            0
        });
        for len in [0u32, 0x10, 0x1000, 0x1001, 0x3000] {
            for with_buffer in [true, false] {
                same(&format!("bat {len:#x} {with_buffer} {small}"), run(&s, driver, |side| {
                    let mut buf = vec![0xccu8; 0x8000];
                    let mut info = [0xccu8; 0x34];
                    let b = if with_buffer { buf.as_mut_ptr() } else { std::ptr::null_mut() };
                    let mut n = len;
                    let hr = if side { rb(&a, path.as_ptr(), b, &mut n, &mut info) } else { lb(&a, path.as_ptr(), b, &mut n, &mut info) };
                    (hr, n, info, buf)
                }));
                for offset in [0u64, 0x1000, 0x10] {
                    same(&format!("user {len:#x} {with_buffer} {offset:#x} {small}"), run(&s, |s| s.reply(blob_driver(if small { 2 } else { 0 })), |side| {
                        let mut buf = vec![0xccu8; 0x8000];
                        let b = if with_buffer { buf.as_mut_ptr() } else { std::ptr::null_mut() };
                        let mut n = len;
                        let hr = if side { ru(&a, path.as_ptr(), offset, b, &mut n) } else { lu(&a, path.as_ptr(), offset, b, &mut n) };
                        (hr, n, buf)
                    }));
                }
            }
        }
    }
}

type Create = extern "system" fn(*const Adapter, *const u16, *const [u8; 16], u32, u64, u32, u32, *mut [u8; 16], *const [u8; 16], *const u8, u32, *const u16) -> HRESULT;

#[test]
fn create_xvd() {
    let s = session();
    trace_all(&s);
    let (r, l) = both!("XCrdCreateXVD": Create);
    let a = adapter();
    let path = wz("[XUC:]\\new.xvd");
    let second = [wz("[XTE:]\\other"), wz("")];
    let id = [7u8; 16];
    let id2 = [9u8; 16];
    let sandbox = *b"SANDBOX-ID-0123456789";
    let mut cases: Vec<(u32, u64, u32, u32)> = Vec::new();
    for bit in 0..32 {
        cases.push((1 << bit, 0x1000, 1, 0x22));
    }
    for flags in [0u32, 0x80, 0x1000, 0x1080, 0x2000, 0x8080, 0x30000, 0x420000, 0x10_0000, 0x10_0002, 0x7f_f3e7] {
        cases.push((flags, 0x1000, 1, 0x22));
        cases.push((flags, 0x1000, 0, 0x22));
        cases.push((flags, 0x1000, 1, 3));
    }
    for pages in [0u64, 0x9ff, 0xa00, 0x1000_0000, 0x1000_0001, u64::MAX >> 12, u64::MAX] {
        cases.push((0, pages, 0, 0));
    }
    for (kind, content) in [(2, 0), (0, 0x28), (0, 0x29), (u32::MAX, 0)] {
        cases.push((0, 0x1000, kind, content));
    }
    for (flags, pages, kind, content) in cases {
        for (with_ids, sb_len, sec) in [(true, 16, 0), (false, 0, 2), (true, 17, 1), (true, 3, 0)] {
            for (what, driver) in drivers(&[]) {
                same(&format!("{flags:#x} {pages:#x} {kind} {content} {with_ids} {sb_len} {sec} {what}"), run(&s, |s| driver(s), |side| {
                    let mut out = [0xccu8; 16];
                    let (i1, i2): (*const [u8; 16], *const [u8; 16]) = if with_ids { (&id, &id2) } else { (std::ptr::null(), std::ptr::null()) };
                    let sb = if sb_len == 0 { std::ptr::null() } else { sandbox.as_ptr() };
                    let sp = if sec == 2 { std::ptr::null() } else { second[sec].as_ptr() };
                    let f = if side { r } else { l };
                    (f(&a, path.as_ptr(), i1, flags, pages, kind, content, &mut out, i2, sb, sb_len, sp), out)
                }));
            }
        }
    }
}

#[test]
fn create_xvd_pointer() {
    let s = session();
    trace_all(&s);
    let (r, l) = both!("XCrdCreateXvdPointer": extern "system" fn(*const Adapter, *const u16, *const u16, u64, u32, *const u8, *const u8, *const u8) -> HRESULT);
    let a = adapter();
    let paths = [wz("[XUC:]\\ptr"), wz("")];
    let target = wz("[XTE:]\\target.xvd");
    let parts: Vec<Vec<u8>> = (0..3).map(|k| (0..0xe00).map(|i| (i as u8).wrapping_mul(k + 3)).collect()).collect();
    for flags in 0..9u32 {
        for mask in 0..8 {
            for (with_target, blob, path) in [(true, 0x1000u64, 0), (false, 0x1000, 0), (true, 0, 0), (true, 0x1000, 1)] {
                let part = |k: usize| if mask & (1 << k) != 0 { parts[k].as_ptr() } else { std::ptr::null() };
                same(&format!("{flags} {mask} {with_target} {blob} {path}"), run(&s, |s| s.reply(accepting(|_, _| {})), |side| {
                    let t = if with_target { target.as_ptr() } else { std::ptr::null() };
                    let f = if side { r } else { l };
                    f(&a, paths[path].as_ptr(), t, blob, flags, part(0), part(1), part(2))
                }));
            }
        }
    }
}

/// A fake machine: physical drives 0-5 open (drive 4 is USB, drive 2's
/// descriptor query fails), and drive `disk`'s SCSI address matches the
/// content's (port 0, path 1, target 2, lun 3); partitions open unless
/// `refuse` names one.
fn machine(s: &Session, disk: u32, refuse: &'static str) {
    s.open(move |name| {
        if name.starts_with("\\\\.\\PhysicalDrive") {
            let n: u32 = name.trim_start_matches("\\\\.\\PhysicalDrive").parse().unwrap();
            return if n < 6 { Ok(FAKE + 0x100 + 2 * n as usize) } else { Err(2) };
        }
        if !refuse.is_empty() && name.contains(refuse) {
            return Err(5);
        }
        if name.contains("Harddisk") { Ok(FAKE + 0x400) } else { Err(2) }
    });
    s.reply(machine_reply(disk));
}

/// The machine's answers to the device-path lookups.
fn machine_reply(disk: u32) -> impl FnMut(&reference::Call, &mut [u8]) -> i32 + Send + 'static {
    move |call, b| {
        let drive = (call.handle.wrapping_sub(FAKE + 0x100) / 2) as u32;
        match call.code {
            srb::IOCTL_SCSI_MINIPORT => {
                b[0x14..0x18].fill(0);
                b[0x1c + 0x18..0x1c + 0x1b].copy_from_slice(&[3, 2, 1]);
                0
            }
            0x2d_1400 => {
                if drive == 2 { return 0xc000_0001_u32 as i32 }
                b[0x1c..0x20].copy_from_slice(&(if drive == 4 { 7u32 } else { 0x11 }).to_le_bytes());
                0
            }
            0x4_1018 => {
                let addr = if drive == disk { [8, 0, 0, 0, 0, 1, 2, 3] } else { [8, 0, 0, 0, 0, 1, 2, 9] };
                b[..8].copy_from_slice(&addr);
                0
            }
            0x7_0024 => 0,
            0x56_001c => { b[..4].copy_from_slice(&0x4242u32.to_le_bytes()); 0 }
            _ => 0xc000_0010_u32 as i32,
        }
    }
}

#[test]
fn device_paths() {
    let s = session();
    trace_all(&s);
    let (r, l) = both!("XCrdQueryDevicePathByPath": extern "system" fn(*const Adapter, *const u16, *mut *mut u16) -> HRESULT);
    let a = adapter();
    let p = wz("[XUC:]\\mounted");
    for (disk, refuse) in [(3u32, ""), (1, ""), (4, ""), (2, ""), (9, ""), (3, "Partition1\\"), (3, "Partition1")] {
        same(&format!("disk {disk} refuse {refuse}"), run(&s, |s| machine(s, disk, refuse), |side| {
            let mut out = std::ptr::null_mut();
            let hr = if side { r(&a, p.as_ptr(), &mut out) } else { l(&a, p.as_ptr(), &mut out) };
            // SAFETY: a heap string or null.
            let text = (!out.is_null()).then(|| unsafe { wide_str(out) });
            heap_free(out.cast());
            (hr, text)
        }));
    }
    same("null path", run(&s, |_| {}, |side| { let mut o = std::ptr::null_mut(); if side { r(&a, std::ptr::null(), &mut o) } else { l(&a, std::ptr::null(), &mut o) } }));
    let (rq, lq) = both!("XCrdQueryDevicePath": extern "system" fn(*mut *mut u16, *const super::disks::Device) -> HRESULT);
    let name = wz("\\\\?\\GLOBALROOT\\Device\\Harddisk7\\Partition1");
    let dev = super::disks::Device { name: name.as_ptr(), address: [0; 8], disk: 7, volume: 0 };
    same("device path", run(&s, |_| {}, |side| {
        let mut out = std::ptr::null_mut();
        let hr = if side { rq(&mut out, &dev) } else { lq(&mut out, &dev) };
        let text = (!out.is_null()).then(|| unsafe { wide_str(out) });
        heap_free(out.cast());
        (hr, text)
    }));
    same("null device", run(&s, |_| {}, |side| { let mut o = std::ptr::null_mut(); if side { rq(&mut o, std::ptr::null()) } else { lq(&mut o, std::ptr::null()) } }));
}

type XasEx = extern "system" fn(*mut *mut u16, *const Adapter, *const u16, *const u16, *const u16, *const u16, *const u16, *const u16, u32, u64, *const u16, u16) -> HRESULT;
type Xas = extern "system" fn(*mut *mut u16, *const Adapter, *const u16, *const u16, *const u16, *const u16, *const u16, *const u16, u32, u64, u64) -> HRESULT;

#[test]
fn xas_paths() {
    let s = session();
    trace_all(&s);
    let (rx, lx) = both!("XCrdXasBuildPathEx": XasEx);
    let (rp, lp) = both!("XCrdXasBuildPath": Xas);
    let a = adapter();
    let text = |t: &str| wz(t);
    let (ip, host, acct, cont, name) = (text("10.0.0.1"), text("example.blob"), text("acct"), text("cont"), text("dir/file.xvc"));
    let sas_list = [text("?sv=1&sig=a%2Fb%%c%"), text(""), text("%%%")];
    let vcsa = text("vcsa-name");
    let opt = |on: bool, v: &Vec<u16>| if on { v.as_ptr() } else { std::ptr::null() };
    for flags in [0u64, 1, 2, 3, 4, 5, 6, 7, 8] {
        for kind in [0u32, 1, 2] {
            for mask in [0u32, 0b1_1111, 0b1_1110, 0b1_1101, 0b1_1011, 0b1_0111, 0b0_1111, 0b10_1111] {
                for (si, sas) in sas_list.iter().enumerate() {
                    for (with_vcsa, port) in [(false, 0u16), (true, 0), (true, 443), (false, 443)] {
                        let args = (opt(mask & 1 != 0, &ip), opt(mask & 2 != 0, &host), opt(mask & 4 != 0, &acct), opt(mask & 8 != 0, &cont), opt(mask & 16 != 0, &name), opt(mask & 32 == 0, sas));
                        same(&format!("{flags} {kind} {mask:b} {si} {with_vcsa} {port}"), run(&s, |_| {}, |side| {
                            let mut out = std::ptr::null_mut();
                            let f = if side { rx } else { lx };
                            let hr = f(&mut out, &a, args.0, args.1, args.2, args.3, args.4, args.5, kind, flags, opt(with_vcsa, &vcsa), port);
                            let r = (!out.is_null()).then(|| unsafe { wide_str(out) });
                            heap_free(out.cast());
                            let mut out2 = std::ptr::null_mut();
                            let g = if side { rp } else { lp };
                            let hr2 = g(&mut out2, &a, args.0, args.1, args.2, args.3, args.4, args.5, kind, flags, port as u64);
                            let r2 = (!out2.is_null()).then(|| unsafe { wide_str(out2) });
                            heap_free(out2.cast());
                            (hr, r, hr2, r2)
                        }));
                    }
                }
            }
        }
    }
    same("no adapter", run(&s, |_| {}, |side| {
        let mut out = std::ptr::null_mut();
        let f = if side { rx } else { lx };
        f(&mut out, std::ptr::null(), ip.as_ptr(), host.as_ptr(), acct.as_ptr(), cont.as_ptr(), name.as_ptr(), sas_list[0].as_ptr(), 1, 0, std::ptr::null(), 0)
    }));
}

static FIRED: std::sync::Mutex<Vec<(usize, u32)>> = std::sync::Mutex::new(Vec::new());
extern "system" fn fired(context: *mut c_void, value: u32) {
    FIRED.lock().unwrap().push((context as usize, value));
}

#[test]
fn storage_notifications() {
    let s = session();
    type Reg2 = extern "system" fn(*const c_void, *mut *mut super::notify::Registration) -> HRESULT;
    type Reg3 = extern "system" fn(*const c_void, *mut c_void, *mut *mut super::notify::Registration) -> HRESULT;
    type Unreg = extern "system" fn(*mut super::notify::Registration) -> HRESULT;
    let (rc, lc) = both!("XCrdRegisterCorruptionCallback": Reg2);
    let three = [both!("XCrdRegisterExternalStorageChangeCallback": Reg3), both!("XCrdRegisterWriteBudgetCallback": Reg3)];
    let unreg = [both!("XCrdUnregisterCorruptionCallback": Unreg), both!("XCrdUnregisterExternalStorageChangeCallback": Unreg), both!("XCrdUnregisterWriteBudgetCallback": Unreg)];
    let data = [0x34u8, 0x12, 0x78, 0x56];
    type Fire = unsafe extern "system" fn(u64, u32, *const c_void, *mut c_void, *const u8, u32) -> i32;
    for kind in 0..3usize {
        for (query, subscribe) in [(0, 0), (0xc000_0001_u32 as i32, 0), (0, 0xc000_0022_u32 as i32)] {
            same(&format!("kind {kind} {query:#x} {subscribe:#x}"), run(&s, |_| {}, |side| {
                {
                    let mut w = reference::WNF.lock().unwrap();
                    *w = reference::Wnf { names: Vec::new(), handler: 0, context: 0, query, subscribe, unsubscribed: Vec::new() };
                }
                FIRED.lock().unwrap().clear();
                let mut reg = std::ptr::null_mut();
                let hr = match kind {
                    0 => (if side { rc } else { lc })(fired as *const c_void, &mut reg),
                    k => { let (r, l) = three[k - 1]; (if side { r } else { l })(fired as *const c_void, 0x77 as *mut c_void, &mut reg) }
                };
                let (handler, context) = { let w = reference::WNF.lock().unwrap(); (w.handler, w.context) };
                if hr >= 0 {
                    // SAFETY: the handler the library subscribed.
                    let fire: Fire = unsafe { std::mem::transmute(handler) };
                    unsafe { fire(0, 0, std::ptr::null(), context as *mut c_void, data.as_ptr(), 4) };
                }
                let unhr = if hr >= 0 { let (r, l) = unreg[kind]; Some((if side { r } else { l })(reg)) } else { None };
                let w = reference::WNF.lock().unwrap();
                (hr, reg.is_null(), w.names.clone(), w.unsubscribed.clone(), FIRED.lock().unwrap().clone(), unhr)
            }));
        }
    }
}

#[test]
fn xds_volume_mappings() {
    let s = session();
    trace_all(&s);
    let (rg, lg) = both!("XCrdXdsGetVolumeMapping": extern "system" fn(*const Adapter, *mut u8, *mut u32, *mut u8, *mut u32) -> HRESULT);
    let (rs, ls) = both!("XCrdXdsSetVolumeMapping": extern "system" fn(*const Adapter, *const u16, *const u16) -> HRESULT);
    let a = adapter();
    // Answers: primary name of `p` bytes and an overlay record of `o` (0:
    // none), or a status.
    for (p, o, status) in [(8u32, 6u32, 0i32), (8, 0, 0), (0x40, 6, 0), (8, 0x40, 0), (0, 0, 0), (8, 6, 0x8000_0005_u32 as i32), (8, 6, 0xc000_0001_u32 as i32)] {
        for (plen, olen, with_overlay) in [(0x20u32, 0x20u32, true), (0x20, 0x20, false), (0x20, 2, true), (4, 0x20, true), (0, 0x20, true), (0x20, 0, true)] {
            same(&format!("get {p} {o} {status:#x} {plen} {olen} {with_overlay}"), run(&s, |s| s.reply(move |_, b| {
                b[0..4].copy_from_slice(&(if o != 0 { 0x18u32 } else if status != 0 { 0x99 } else { 0 }).to_le_bytes());
                b[8..12].copy_from_slice(&p.to_le_bytes());
                for (i, x) in b.iter_mut().enumerate().skip(0xc).take(0xc) { *x = i as u8; }
                if o != 0 && b.len() >= 0x18 + 0xc + (o as usize) {
                    b[0x18 + 8..0x18 + 12].copy_from_slice(&o.to_le_bytes());
                    for (i, x) in b.iter_mut().enumerate().skip(0x24).take(o as usize) { *x = (i as u8) ^ 0x80; }
                }
                status
            }), |side| {
                let (mut pb, mut ob) = ([0xccu8; 0x40], [0xccu8; 0x40]);
                let (mut pl, mut ol) = (plen, olen);
                let f = if side { rg } else { lg };
                let hr = f(&a, pb.as_mut_ptr(), &mut pl, if with_overlay { ob.as_mut_ptr() } else { std::ptr::null_mut() }, &mut ol);
                (hr, pl, ol, pb, ob)
            }));
        }
    }
    for (primary, overlay) in [("C:\\data", Some("D:\\overlay\\")), ("C:\\data\\", None), ("\\\\?\\Volume{1}", Some("x"))] {
        for status in [0, 0xc000_0001_u32 as i32] {
            same(&format!("set {primary} {overlay:?} {status:#x}"), run(&s, |s| s.reply(move |_, _| status), |side| {
                let p = wz(primary);
                let o = overlay.map(wz);
                let f = if side { rs } else { ls };
                f(&a, p.as_ptr(), o.as_ref().map_or(std::ptr::null(), |o| o.as_ptr()))
            }));
        }
    }
}

type Start = extern "system" fn(*mut u64, *const Adapter, *const u16, *const u16, *const u16, *const u16, *const u16, u32, *const [*const u16; 2], u32, *const [u8; 16], *mut u64, *mut u32) -> HRESULT;

#[test]
fn streaming_start() {
    let s = session();
    trace_all(&s);
    let (r, l) = both!("XCrdStreamingStart": Start);
    let a = adapter();
    let w = |t: &str| wz(t);
    let (src, dst, name, content, extra, empty) = (w("[XUC:]\\src"), w("[XTE:]\\dst"), w("name"), w("C:\\content"), w("extra"), w(""));
    let words: Vec<Vec<u16>> = ["", "lang", "chunk-7"].iter().map(|x| wz(x)).collect();
    let specs: Vec<[*const u16; 2]> = (0..3).map(|i| [words[i % 3].as_ptr(), words[(i + 1) % 3].as_ptr()]).collect();
    let guid = [5u8; 16];
    let mut flag_cases: Vec<u32> = (0..18).map(|b| 1u32 << b).collect();
    flag_cases.extend([0, 0x1000 | 0x800, 0x1000 | 0xe800 | 0x1f, 0x1_ffff, 0x2_0000, 0x800]);
    for flags in flag_cases {
        for (opt, count) in [(0u32, 0u32), (0b111, 2), (0b101, 3), (0b010, 1), (0b1000, 0), (0b1_0000, 1)] {
            for (what, driver) in drivers(&[]) {
                same(&format!("{flags:#x} {opt:b} {count} {what}"), run(&s, |s| driver(s), |side| {
                    let pick = |bit: u32, v: &Vec<u16>| if opt & bit != 0 { v.as_ptr() } else if opt & 0b1000 != 0 { empty.as_ptr() } else { std::ptr::null() };
                    let sp = if count != 0 && opt & 0b1_0000 == 0 { specs.as_ptr() } else if opt & 0b1_0000 != 0 && count == 0 { specs.as_ptr() } else { std::ptr::null() };
                    let (mut id, mut o64, mut o32) = (0xccccu64, 0xccccu64, 0xccccu32);
                    let f = if side { r } else { l };
                    let hr = f(&mut id, &a, src.as_ptr(), pick(1, &name), dst.as_ptr(), pick(2, &content), pick(4, &extra), count, sp, flags, if opt & 1 != 0 { &guid } else { std::ptr::null() }, &mut o64, &mut o32);
                    (hr, id, o64, o32)
                }));
            }
        }
    }
}

type Update = extern "system" fn(*const Adapter, *const u16, *const u16, *const u16, u32, *const [*const u16; 2], u32, *mut u64, *mut u64, *mut u8, *mut u32) -> HRESULT;
type UpdateEx = extern "system" fn(*const Adapter, *const u16, *const u16, *const u16, u32, *const [*const u16; 2], i32, u32, *mut u64, *mut u64, *mut u32) -> HRESULT;

#[test]
fn update_xvc() {
    let s = session();
    trace_all(&s);
    let (r, l) = both!("XCrdUpdateXvc": Update);
    let (rx, lx) = both!("XCrdUpdateXvcEx": UpdateEx);
    let a = adapter();
    let (base, upd, extra, empty) = (wz("[XUC:]\\\\base.xvc"), wz("[XTE:]\\upd.xvc"), wz("extra-name"), wz(""));
    let words: Vec<Vec<u16>> = ["", "en-US", "chunk"].iter().map(|x| wz(x)).collect();
    let specs: Vec<[*const u16; 2]> = (0..3).map(|i| [words[i % 3].as_ptr(), words[(i + 2) % 3].as_ptr()]).collect();
    let flag_cases: Vec<u32> = (0..8).map(|b| 1u32 << b).chain([0, 3, 0xa, 0x19, 0x1b, 0x3b, 0x8000_0000]).collect();
    // opt bits: 1 base, 2 updated, 4 extra, 8 extra empty, 16 size out, 32 free out, 64 small outs, 128 specs with count 0
    let opts = [0b111_0111u32, 0b111_0011, 0b111_1111, 0b000_0011, 0b001_0011, 0b010_0011, 0b000_0001, 0b000_0010, 0b1000_0011, 0b111_0110];
    let ptr = |on: bool, v: &Vec<u16>| if on { v.as_ptr() } else { std::ptr::null() };
    for opt in opts {
        for count in [0u32, 1, 3] {
            let sp = if count != 0 || opt & 128 != 0 { specs.as_ptr() } else { std::ptr::null() };
            let x = if opt & 8 != 0 { empty.as_ptr() } else { ptr(opt & 4 != 0, &extra) };
            let call = |f: &dyn Fn(*mut u64, *mut u64, *mut u8, *mut u32) -> HRESULT| {
                let (mut us, mut fs, mut o8, mut o32) = (0xccccu64, 0xccccu64, 0xccu8, 0xccccu32);
                let hr = f(if opt & 16 != 0 { &mut us } else { std::ptr::null_mut() }, if opt & 32 != 0 { &mut fs } else { std::ptr::null_mut() }, if opt & 64 != 0 { &mut o8 } else { std::ptr::null_mut() }, if opt & 64 != 0 { &mut o32 } else { std::ptr::null_mut() });
                (hr, us, fs, o8, o32)
            };
            for flags in &flag_cases {
                for (what, driver) in drivers(&[]) {
                    same(&format!("update {flags:#x} {opt:b} {count} {what}"), run(&s, |s| driver(s), |side| {
                        let f = if side { r } else { l };
                        call(&|us, fs, o8, o32| f(&a, ptr(opt & 1 != 0, &base), ptr(opt & 2 != 0, &upd), x, count, sp, *flags, us, fs, o8, o32))
                    }));
                }
            }
            for action in [0i32, 1, 2, 3, -1] {
                for flags in [0u32, 0x10, 1, 0x8000_0010] {
                    for (what, driver) in drivers(&[]) {
                        same(&format!("ex {action} {flags:#x} {opt:b} {count} {what}"), run(&s, |s| driver(s), |side| {
                            let f = if side { rx } else { lx };
                            call(&|us, fs, _, o32| f(&a, ptr(opt & 1 != 0, &base), ptr(opt & 2 != 0, &upd), x, count, sp, action, flags, us, fs, o32))
                        }));
                    }
                }
            }
        }
    }
}

#[test]
fn resize_xvd() {
    let s = session();
    trace_all(&s);
    let (r, l) = both!("XCrdResizeXVD": extern "system" fn(*const Adapter, *const u16, i64) -> HRESULT);
    let a = adapter();
    let p = wz("[XUC:]\\mounted.xvd");
    // fail: 0 none, 1 volume query, 2 the resize block, 3 update properties, 4 grow, 5 extend, 6 open (error 5), 7 open (no error)
    for fail in 0..8u32 {
        for size in [0x10_0000i64, 0, -1, 0x7fff_ffff_ffff_ffff, 0x1234_5678] {
            same(&format!("fail {fail} size {size:#x}"), run(&s, |s| {
                machine(s, 3, "");
                if fail >= 6 {
                    let error = if fail == 6 { 5 } else { 0 };
                    s.open(move |name| {
                        if let Some(n) = name.strip_prefix("\\\\.\\PhysicalDrive") {
                            let n: u32 = n.parse().unwrap();
                            return if n < 6 { Ok(FAKE + 0x100 + 2 * n as usize) } else { Err(2) };
                        }
                        if name.ends_with("Partition1") { Err(error) } else if name.contains("Harddisk") { Ok(FAKE + 0x400) } else { Err(2) }
                    });
                }
                let mut inner = machine_reply(3);
                s.reply(move |call, b| {
                    let failing = 0xc000_0022_u32 as i32;
                    match call.code {
                        c if c == reference::VOLUME_QUERY | 3 => {
                            if fail == 1 { return failing }
                            b[..8].copy_from_slice(&0x1000u64.to_le_bytes());
                            b[0x10..0x14].copy_from_slice(&8u32.to_le_bytes());
                            b[0x14..0x18].copy_from_slice(&0x200u32.to_le_bytes());
                            0
                        }
                        srb::IOCTL_SCSI_MINIPORT if u32::from_le_bytes(b[0x10..0x14].try_into().unwrap()) == 0x7800_00a0 => {
                            b[0x14..0x18].copy_from_slice(&(if fail == 2 { 0xc05e_0003u32 } else { 0 }).to_le_bytes());
                            0
                        }
                        0x7_0140 => if fail == 3 { failing } else { 0 },
                        0x7_c0d0 => if fail == 4 { failing } else { 0 },
                        0x9_00f0 => if fail == 5 { failing } else { 0 },
                        _ => inner(call, b),
                    }
                });
            }, |side| if side { r(&a, p.as_ptr(), size) } else { l(&a, p.as_ptr(), size) }));
        }
    }
    same("null adapter", run(&s, |_| {}, |side| if side { r(std::ptr::null(), p.as_ptr(), 1) } else { l(std::ptr::null(), p.as_ptr(), 1) }));
    same("null path", run(&s, |_| {}, |side| if side { r(&a, std::ptr::null(), 1) } else { l(&a, std::ptr::null(), 1) }));
}
