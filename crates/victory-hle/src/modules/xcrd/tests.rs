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
    setup(s);
    let r = f(true);
    let rr = s.take();
    setup(s);
    let l = f(false);
    let lr = s.take();
    ((r, rr), (l, lr))
}

fn same<T: std::fmt::Debug + PartialEq>(what: &str, ((r, rr), (l, lr)): ((T, Record), (T, Record))) {
    assert_eq!(r, l, "{what}: result");
    assert_eq!(rr, lr, "{what}: what the driver saw");
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
