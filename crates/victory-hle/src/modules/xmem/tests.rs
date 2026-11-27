//! The layer's xmem against the console's `xmem.dll`.
//!
//! `vectors/` holds what the console's library did, called under a harness
//! (`~/victory/xmemharness` on the build box: `xmh.exe` maps `xmem.dll`,
//! binds its imports itself, and records every call it makes to the kernel;
//! `gen.py` writes the cases, `vectors.py` these files). The kernel side
//! is the harness's model: memory allocations succeed as asked, device
//! queries answer zeroes unless the file's `env` line says otherwise, and
//! physical pages are numbered from 0x100000 by 16. Each line is one call:
//! its arguments, then what the library returned, set as the thread's
//! error ("dead": left alone), wrote into the caller's buffers, and asked
//! the kernel for.
//!
//! The pure decisions (`spec`) are compared with what the library asked
//! its kernel for; the exports are replayed call by call, with the same
//! buffers, and compared where the outcome is the library's and not the
//! harness kernel's (each exception says why).

use std::collections::HashMap;
use std::sync::Mutex;

use super::spec::{self, Env, Param, Route};
use super::*;

/// One test at a time touches the module's state (its environment, heaps,
/// hooks, options).
static SERIAL: Mutex<()> = Mutex::new(());

fn serial() -> std::sync::MutexGuard<'static, ()> {
    SERIAL.lock().unwrap_or_else(|e| e.into_inner())
}

fn h(s: &str) -> u64 {
    u64::from_str_radix(s.trim_start_matches("0x"), 16).unwrap_or_else(|_| panic!("not hex: {s}"))
}

/// The harness's settings of a file (`# env ...`), as `xmem.dll` saw them.
fn env_of(text: &str) -> Env {
    let mut env = Env { mode: 0x50, title_partition: 0, ..Env::RETAIL };
    if let Some(line) = text.lines().find(|l| l.starts_with("# env ")) {
        for kv in line[6..].split_whitespace() {
            let (k, v) = kv.split_once('=').unwrap();
            let v = h(v);
            match k {
                "mode" => env.mode = v as u16,
                "part2" => env.tools_partition = v != 0,
                "ww" => env.write_watch_query = v,
                "numa" => env.numa_nodes = v as u32,
                "tools" => env.tools_query = v,
                "system" => env.system_type = v as u16,
                _ => panic!("unknown setting {k}"),
            }
        }
    }
    env
}

fn params_text(params: &[Param]) -> String {
    params
        .iter()
        .map(|p| match *p {
            Param::Address(lo, hi, a) => format!("1:{lo:x}:{hi:x}:{a:x}"),
            Param::Numa(v) => format!("2:{v:x}"),
            Param::Partition => "3".into(),
            Param::Physical => "4".into(),
            Param::Attributes(v) => format!("5:{v:x}"),
        })
        .collect::<Vec<_>>()
        .join(" ")
}

fn request_text(r: &Result<spec::Request, u32>, base: u64) -> String {
    match r {
        Err(e) => format!("err {e:x}"),
        Ok(r) => format!(
            "ok {:x} {:x} {:x} {:x}{}{}",
            r.kind,
            r.protect,
            r.base.wrapping_sub(base),
            r.size,
            if r.params.is_empty() { "" } else { " " },
            params_text(&r.params)
        ),
    }
}

/// Lines of a vector file, without comments.
fn cases(text: &str) -> impl Iterator<Item = (&str, &str)> {
    text.lines().filter(|l| !l.starts_with('#') && !l.is_empty()).map(|l| l.split_once(" -> ").unwrap())
}

fn compare(name: &str, failures: Vec<String>, total: usize) {
    assert!(total > 0, "{name}: no cases");
    if !failures.is_empty() {
        panic!("{name}: {} of {total} differ from xmem.dll:\n{}", failures.len(), failures[..failures.len().min(40)].join("\n"));
    }
}

/// `XMemVirtualAlloc` at an address of the system's choice: refusals, page
/// rounding, address requirements, kernel type and parameters, under each
/// recorded environment.
#[test]
fn virtual_alloc_asks_the_kernel_what_xmem_dll_asks() {
    for (name, text) in [
        ("va", include_str!("vectors/va.txt")),
        ("va_ww1", include_str!("vectors/va_ww1.txt")),
        ("va_numa", include_str!("vectors/va_numa.txt")),
        ("va_lockhart", include_str!("vectors/va_lockhart.txt")),
        ("va_tools", include_str!("vectors/va_tools.txt")),
        ("va_mode68", include_str!("vectors/va_mode68.txt")),
        ("va_sizes", include_str!("vectors/va_sizes.txt")),
    ] {
        let env = env_of(text);
        let mut failures = Vec::new();
        let mut total = 0;
        for (args, want) in cases(text) {
            let a: Vec<u64> = args.split_whitespace().map(h).collect();
            let got = request_text(&spec::virtual_alloc(&env, 0, a[3], a[0] as u32, a[1], a[2] as u32), 0);
            total += 1;
            if got != want {
                failures.push(format!("  kind {:x} flags {:x} protect {:x} size {:x}: xmem.dll `{want}`, ours `{got}`", a[0], a[1], a[2], a[3]));
            }
        }
        compare(name, failures, total);
    }
}

/// At a given address: the base rounded down to its page, the size to the
/// end of the last page.
#[test]
fn virtual_alloc_rounds_a_given_address_as_xmem_dll() {
    let text = include_str!("vectors/va_base.txt");
    let env = env_of(text);
    let mut failures = Vec::new();
    let mut total = 0;
    for (args, want) in cases(text) {
        let a: Vec<u64> = args.split_whitespace().map(h).collect();
        let (kind, offset, size, reservation) = (a[0] as u32, a[1], a[2], a[3]);
        let got = request_text(&spec::virtual_alloc(&env, reservation + offset, size, kind, 3, 0x2004), reservation);
        total += 1;
        if got != want {
            failures.push(format!("  kind {kind:x} offset {offset:x} size {size:x}: xmem.dll `{want}`, ours `{got}`"));
        }
    }
    compare("va_base", failures, total);
}

/// `XMemAllocDefault`: the route of every block (process heap, which heap
/// of its memory type, an allocation of its own), what an allocation of its
/// own asks for, what each heap reserves and commits, and the alignment
/// the block is given.
#[test]
fn alloc_default_routes_as_xmem_dll() {
    for (name, text, large) in [
        ("alloc", include_str!("vectors/alloc.txt"), false),
        ("alloc_many", include_str!("vectors/alloc_many.txt"), false),
        ("alloc_2m", include_str!("vectors/alloc_2m.txt"), true),
    ] {
        let env = Env { large_chunks: large as u32, ..env_of(text) };
        let mut failures = Vec::new();
        let mut total = 0;
        // The library's heaps (numbered as it made them) as our heap indexes.
        let mut heaps: HashMap<u64, usize> = HashMap::new();
        for (args, want) in cases(text) {
            let a: Vec<u64> = args.split_whitespace().map(h).collect();
            let (size, attributes) = (a[0], a[1]);
            let w: Vec<&str> = want.split_whitespace().collect();
            total += 1;
            let route = Route::of(&env, size, attributes);
            let mut fail = |what: String| failures.push(format!("  size {size:x} attributes {attributes:x}: {what}"));
            let align = |w: &[&str]| w.iter().position(|&x| x == "align").map(|i| h(w[i + 1]));
            match (w[0], route) {
                ("null", None) => {}
                ("null", Some(Route::Virtual)) => {
                    let (kind, flags, protect) = spec::virtual_request(&env, attributes);
                    let error = spec::virtual_alloc(&env, 0, size, kind, flags, protect).err();
                    if error != Some(h(w[1]) as u32) {
                        fail(format!("xmem.dll null with error {}, ours {error:?}", w[1]));
                    }
                }
                ("process", Some(Route::ProcessHeap)) => {}
                ("virtual", Some(Route::Virtual)) => {
                    let (kind, flags, protect) = spec::virtual_request(&env, attributes);
                    let got = request_text(&spec::virtual_alloc(&env, 0, size, kind, flags, protect), 0);
                    let end = w.iter().position(|&x| x == "align").unwrap();
                    let want = format!("ok {} {} 0 {}{}{}", w[1], w[2], w[3], if end > 4 { " " } else { "" }, w[4..end].join(" "));
                    if got != want {
                        fail(format!("xmem.dll asks `{want}`, ours `{got}`"));
                    }
                }
                ("heap", Some(Route::Heap(index))) => {
                    let id = h(w[1]);
                    let mapped = *heaps.entry(id).or_insert(index);
                    if mapped != index {
                        fail(format!("xmem.dll's heap {id} is our {mapped}, this block goes to our {index}"));
                    }
                    let shape = spec::heap_shape(&env, index);
                    let got = request_text(&spec::virtual_alloc(&env, 0, shape.reserve, shape.kind, shape.flags, shape.protect), 0);
                    let want = format!("ok {} {} 0 {:x} {}", w[2], w[3], 1u64 << 31, w[4].replace('/', " "));
                    if got != want {
                        fail(format!("heap reservation: xmem.dll `{want}`, ours `{got}`"));
                    }
                    for c in w.iter().filter(|x| x.starts_with("commit:")) {
                        let f: Vec<&str> = c.split(':').collect();
                        let protect = spec::KIND_PROTECTION[index & 0xf];
                        let piece = shape.piece_kind.filter(|&k| k | MEM_WRITE_WATCH == h(f[2]) as u32 | MEM_WRITE_WATCH);
                        let commit = spec::virtual_alloc(&env, 1 << 33, shape.chunk, piece.unwrap_or(shape.chunk_kind), shape.flags, protect);
                        let ok = commit.as_ref().is_ok_and(|r| r.size == h(f[1]) && r.kind == h(f[2]) as u32);
                        if !ok {
                            fail(format!("chunk commit: xmem.dll {c}, ours {commit:?}"));
                        }
                    }
                    // The block's alignment: what the attributes ask, at least
                    // a unit.
                    let needed = (attributes >> spec::ALIGNMENT_SHIFT & 0x1f).max(shape.unit.trailing_zeros() as u64);
                    if align(&w).is_some_and(|got| got < needed) {
                        fail(format!("xmem.dll aligned it to 2^{:x} only, below the 2^{needed:x} we promise", align(&w).unwrap()));
                    }
                }
                _ => fail(format!("xmem.dll `{want}`, our route {route:?}")),
            }
        }
        compare(name, failures, total);
    }
}

#[test]
fn tags_are_xmem_dll_tags() {
    let text = include_str!("vectors/tags.txt");
    let mut failures = Vec::new();
    let mut total = 0;
    for (args, want) in cases(text) {
        let tag = h(args.split_whitespace().nth(1).unwrap()) as u32;
        let want = h(want.split_whitespace().next().unwrap());
        total += 1;
        if spec::make_tag(tag) != want {
            failures.push(format!("  {tag:#x}: xmem.dll {want:#x}, ours {:#x}", spec::make_tag(tag)));
        }
        if XMemMakeTagExport(tag) != want {
            failures.push(format!("  export {tag:#x}"));
        }
    }
    compare("tags", failures, total);
}

#[allow(non_snake_case)]
fn XMemMakeTagExport(tag: u32) -> u64 {
    more::XMemMakeTag(tag)
}

// ---- replays of calls through the exports ----

/// The return width of each export: 'q' 64 bits, 'd' 32, 'b' 8, 'v' none.
fn width(name: &str) -> char {
    match name {
        "XMemVirtualAlloc" | "XMemAlloc" | "XMemAllocDefault" | "XMemMapPhysicalPages" | "XMemGetTitleHeap"
        | "XMemCreateWin32Heap" | "XMemMakeTag" | "XMemGetAllocationHysteresis" => 'q',
        "XMemFree" | "XMemFreeDefault" | "XMemSetAllocationHooks" | "XMemSetOption" | "XMemSetWin32HeapTrackingHooks"
        | "XMemRegisterGraphicsMemoryConfiguration" | "XMemDumpMemoryStats" | "XMemSetWinHttpHooks"
        | "XMemSetWin32HeapName" => 'v',
        "XMemGetPartitionHandleAndId" => 'b',
        _ => 'd',
    }
}

fn export(name: &str) -> usize {
    module()
        .exports
        .iter()
        .find(|e| e.name == name)
        .unwrap_or_else(|| panic!("no export {name}"))
        .address
}

type Six = extern "system" fn(u64, u64, u64, u64, u64, u64) -> u64;

/// What one replayed call did.
struct Done {
    ret: u64,
    err: u32,
    bufs: Vec<(usize, Vec<u8>)>,
}

/// Replays a file's calls through our exports (arguments as the harness
/// makes them: hex, `buf:N` a zeroed buffer, `in:HEX` a buffer with those
/// bytes, `r:K[+X]` case K's result plus X). `check` compares each call
/// with the library's; it returns the differences.
fn replay(text: &str, mut check: impl FnMut(usize, &str, &[&str], &Done, &str, &[u64]) -> Option<String>) -> (Vec<String>, usize) {
    let mut rets: Vec<u64> = Vec::new();
    let mut keep: Vec<Vec<Vec<u8>>> = Vec::new();
    let mut failures = Vec::new();
    let mut total = 0;
    for (k, (args, want)) in cases(text).enumerate() {
        let parts: Vec<&str> = args.split_whitespace().collect();
        let (name, args) = (parts[0], &parts[1..]);
        let mut bufs: Vec<Vec<u8>> = Vec::new();
        let mut a = [0u64; 6];
        let mut buf_of = vec![usize::MAX; 6];
        for (i, t) in args.iter().enumerate() {
            a[i] = if let Some(n) = t.strip_prefix("buf:") {
                bufs.push(vec![0; h(n) as usize + 16]);
                buf_of[i] = bufs.len() - 1;
                bufs.last_mut().unwrap().as_mut_ptr() as u64
            } else if let Some(x) = t.strip_prefix("in:") {
                let mut b: Vec<u8> = (0..x.len() / 2).map(|j| h(&x[2 * j..2 * j + 2]) as u8).collect();
                b.extend([0; 16]);
                bufs.push(b);
                buf_of[i] = bufs.len() - 1;
                bufs.last_mut().unwrap().as_mut_ptr() as u64
            } else if let Some(r) = t.strip_prefix("r:") {
                let (c, plus) = r.split_once('+').map_or((r, 0), |(c, p)| (c, h(p)));
                rets[c.parse::<usize>().unwrap()] + plus
            } else {
                h(t)
            };
        }
        set_last_error(0xdead);
        let ret = match name {
            "!free" => {
                if a[0] != 0 {
                    // SAFETY: a reservation the replay made.
                    unsafe { VirtualFree(a[0] as *mut c_void, 0, MEM_RELEASE) };
                }
                0
            }
            "!write" => {
                // SAFETY: committed memory the replay made.
                unsafe { ((a[0] + a[1]) as *mut u8).write_volatile(1) };
                0
            }
            _ => {
                // SAFETY: our own export, called with the harness's arguments.
                let f: Six = unsafe { std::mem::transmute(export(name)) };
                f(a[0], a[1], a[2], a[3], a[4], a[5])
            }
        };
        // SAFETY: plain thread state.
        let err = unsafe { windows_sys::Win32::Foundation::GetLastError() };
        let ret = match width(name) {
            'q' => ret,
            'd' => ret & 0xffff_ffff,
            'b' => ret & 0xff,
            _ => 0,
        };
        rets.push(ret);
        let done = Done {
            ret,
            err,
            bufs: (0..6).filter(|&i| buf_of[i] != usize::MAX).map(|i| (i, bufs[buf_of[i]].clone())).collect(),
        };
        if !name.starts_with('!') {
            total += 1;
            if let Some(f) = check(k, name, args, &done, want, &rets) {
                failures.push(format!("  case {k} {name} {}: {f}\n    xmem.dll: {want}", args.join(" ")));
            }
        }
        keep.push(bufs);
    }
    (failures, total)
}

/// The library's outcome of a line: return value, error (None: left
/// alone), buffers by argument.
fn outcome(want: &str) -> (u64, Option<u32>, HashMap<usize, Vec<u8>>) {
    let (head, _) = want.split_once(" | ").unwrap_or((want, ""));
    let f: Vec<&str> = head.split_whitespace().collect();
    let err = if f[1] == "dead" { None } else { Some(h(f[1]) as u32) };
    let bufs = f[2..]
        .iter()
        .filter_map(|b| b.strip_prefix("buf"))
        .map(|b| {
            let (i, x) = b.split_once('=').unwrap();
            (i.parse().unwrap(), (0..x.len() / 2).map(|j| h(&x[2 * j..2 * j + 2]) as u8).collect())
        })
        .collect();
    (h(f[0]), err, bufs)
}

/// Return value and error as the library's.
fn same_result(done: &Done, want: &str) -> Option<String> {
    let (ret, err, _) = outcome(want);
    let got_err = (done.err != 0xdead).then_some(done.err);
    if done.ret != ret || got_err != err {
        return Some(format!("ours {:x} error {:?}", done.ret, got_err));
    }
    None
}

/// Null or not, and the error, as the library's.
fn same_nullness(done: &Done, want: &str) -> Option<String> {
    let (ret, err, _) = outcome(want);
    let got_err = (done.err != 0xdead).then_some(done.err);
    if (done.ret == 0) != (ret == 0) || got_err != err {
        return Some(format!("ours {:x} error {:?}", done.ret, got_err));
    }
    None
}

/// Every buffer byte for byte as the library left it.
fn same_bufs(done: &Done, want: &str) -> Option<String> {
    let (_, _, bufs) = outcome(want);
    for (i, b) in &done.bufs {
        if let Some(w) = bufs.get(i)
            && &b[..w.len()] != w.as_slice()
        {
            return Some(format!("buffer {i}: ours {}", b[..w.len()].iter().map(|x| format!("{x:02x}")).collect::<String>()));
        }
    }
    None
}

/// The module as a fresh process would have it, in `env`.
fn fresh(env: Env) {
    *env_mut() = env;
    for heap in &BLOCKS {
        *heap.lock().unwrap_or_else(|e| e.into_inner()) = None;
    }
    *HOOKS.lock().unwrap_or_else(|e| e.into_inner()) = Some(defaults());
    more::reset();
}

fn run(name: &str, text: &str, check: impl FnMut(usize, &str, &[&str], &Done, &str, &[u64]) -> Option<String>) {
    let _serial = serial();
    fresh(env_of(text));
    let (failures, total) = replay(text, check);
    compare(name, failures, total);
}

#[test]
fn transfers_as_xmem_dll() {
    for (name, text, env) in [
        ("transfer", include_str!("vectors/transfer.txt"), None),
        ("transfer_tools", include_str!("vectors/transfer_tools.txt"), Some((0x50, true, 0x60))),
        ("transfer_lockhart", include_str!("vectors/transfer_lockhart.txt"), Some((0x60, true, 0x60))),
        ("transfer_fail", include_str!("vectors/transfer_fail.txt"), None),
    ] {
        let _serial = serial();
        let mut e = env_of(text);
        if let Some((mode, part2, tools)) = env {
            (e.mode, e.tools_partition, e.tools_query) = (mode, part2, tools);
        }
        fresh(e);
        TRANSFERRED.store(0, Ordering::Relaxed);
        let (failures, total) = replay(text, |_, _, args, done, want, _| {
            // The harness's kernel refuses every transfer in transfer_fail
            // (STATUS_NO_MEMORY); ours, past the title's budget only.
            if name == "transfer_fail" && args[0] == "0" {
                return None;
            }
            same_result(done, want)
        });
        compare(name, failures, total);
    }
}

#[test]
fn tools_memory_as_xmem_dll() {
    for (name, text, mode, part2, tools, system) in [
        ("tools", include_str!("vectors/tools.txt"), 0x50, false, 0, 0),
        ("tools_on", include_str!("vectors/tools_on.txt"), 0x50, true, 0x60, 0),
        ("tools_part2", include_str!("vectors/tools_part2.txt"), 0x50, true, 0, 0),
        ("tools_sys60", include_str!("vectors/tools_sys60.txt"), 0x50, true, 0, 0x60),
        ("tools_bit", include_str!("vectors/tools_bit.txt"), 0x50, true, 1, 0),
    ] {
        let _serial = serial();
        fresh(Env { mode, tools_partition: part2, tools_query: tools, system_type: system, ..env_of("") });
        let (failures, total) = replay(text, |_, _, _, done, want, _| same_result(done, want));
        compare(name, failures, total);
    }
}

#[test]
fn mapping_checks_as_xmem_dll() {
    run("map", include_str!("vectors/map.txt"), |_, name, args, done, want, _| {
        // Page numbers 0, which the harness's kernel took as pages though
        // none were allocated, and addresses 0 for a scatter: the outcome is
        // the kernel's (ours unmaps on page 0 and refuses address 0).
        if name == "XMemMapPhysicalPages" && args[0] != "0" && args[1] != "0" && args[2] != "0" {
            return None;
        }
        if name == "XMemMapPhysicalPagesScatter" && args[0] != "0" && args[1] != "0" {
            return None;
        }
        if name == "XMemMapPhysicalPages" && args[0] != "0" {
            // Success returns the address the caller gave.
            return same_nullness(done, want);
        }
        same_result(done, want)
    });
}

#[test]
fn physical_pages_as_xmem_dll() {
    for (name, text) in [("phys", include_str!("vectors/phys.txt")), ("phys_cap", include_str!("vectors/phys_cap.txt"))] {
        run(name, text, |_, _, args, done, want, _| {
            same_result(done, want)?;
            // The count as the library left it, and as many page numbers (ours
            // are our own).
            let (_, _, bufs) = outcome(want);
            let n = u64::from_le_bytes(done.bufs[0].1[..8].try_into().unwrap());
            let want_n = u64::from_le_bytes(bufs[&1][..8].try_into().unwrap());
            if n != want_n {
                return Some(format!("count {n:x}"));
            }
            let _ = args;
            None
        });
    }
    run("physfree", include_str!("vectors/physfree.txt"), |_, _, args, done, want, _| {
        // A null page array: the harness's kernel took it; ours refuses.
        if args[1] == "0" {
            return (done.ret != 0).then(|| "null array taken".into());
        }
        same_result(done, want)
    });
}

#[test]
fn hooks_as_xmem_dll() {
    run("hooks", include_str!("vectors/hooks.txt"), |_, _, _, done, want, _| same_nullness(done, want));
    run("hooks_set", include_str!("vectors/hooks_set.txt"), |_, _, _, done, want, _| same_nullness(done, want));
    // With the exe's routines: they are called (the library's kernel log
    // shows hook_alloc and hook_free), and SetAllocationHooks(0, 0) brings
    // the defaults back.
    static CALLS: AtomicU64 = AtomicU64::new(0);
    extern "system" fn alloc(n: usize, _: u64) -> *mut c_void {
        CALLS.fetch_add(1, Ordering::Relaxed);
        // SAFETY: plain allocation.
        unsafe { windows_sys::Win32::System::Memory::HeapAlloc(windows_sys::Win32::System::Memory::GetProcessHeap(), 0, n.max(1)) }
    }
    extern "system" fn free(p: *mut c_void, _: u64) {
        CALLS.fetch_add(1 << 32, Ordering::Relaxed);
        // SAFETY: what `alloc` gave, or null.
        unsafe { windows_sys::Win32::System::Memory::HeapFree(windows_sys::Win32::System::Memory::GetProcessHeap(), 0, p) };
    }
    for (name, text) in [("hooks_exe", include_str!("vectors/hooks_exe.txt")), ("hooks_set_exe", include_str!("vectors/hooks_set_exe.txt"))] {
        let _serial = serial();
        fresh(env_of(text));
        *HOOKS.lock().unwrap_or_else(|e| e.into_inner()) = Some([alloc as *const () as usize, free as *const () as usize]);
        let (failures, total) = replay(text, |_, _, _, done, want, _| {
            let before = CALLS.swap(0, Ordering::Relaxed);
            let hooked = want.contains("hook_alloc") as u64 | (want.contains("hook_free") as u64) << 32;
            same_nullness(done, want).or_else(|| (before != hooked).then(|| format!("hook calls {before:x}, xmem.dll {hooked:x}")))
        });
        compare(name, failures, total);
    }
}

#[test]
fn options_and_write_watch_as_xmem_dll() {
    run("options", include_str!("vectors/options.txt"), |_, _, _, done, want, _| same_result(done, want));
    for (name, text) in [("option_ww", include_str!("vectors/option_ww.txt")), ("writewatch", include_str!("vectors/writewatch.txt"))] {
        run(name, text, |_, call, _, done, want, rets| {
            if call == "XMemVirtualAlloc" {
                return same_nullness(done, want);
            }
            same_result(done, want)?;
            if call == "XMemGetWriteWatch" {
                // Addresses relative to the allocation, then count and
                // granularity.
                let (_, _, bufs) = outcome(want);
                let base_want = 0x80_0000u64;
                let rel = |b: &[u8], base: u64| -> Vec<u64> {
                    b.chunks(8).map(|c| u64::from_le_bytes(c.try_into().unwrap())).take_while(|&x| x != 0).map(|x| x - base).collect()
                };
                let ours = rel(&done.bufs[0].1[..0x80], rets[0]);
                let theirs = rel(&bufs[&3], base_want);
                if ours != theirs {
                    return Some(format!("pages written {ours:x?}, xmem.dll {theirs:x?}"));
                }
            }
            None
        });
    }
}

#[test]
fn heap_hysteresis_and_statistics_as_xmem_dll() {
    for (name, text) in [("hysteresis", include_str!("vectors/hysteresis.txt")), ("hysteresis_2m", include_str!("vectors/hysteresis_2m.txt"))] {
        run(name, text, |_, call, _, done, want, _| {
            if call == "XMemAllocDefault" {
                return same_nullness(done, want);
            }
            same_result(done, want).or_else(|| same_bufs(done, want))
        });
    }
}

#[test]
fn win32_heaps_as_xmem_dll() {
    run("heaps", include_str!("vectors/heaps.txt"), |_, call, _, done, want, _| {
        match call {
            "XMemGetTitleHeap" => {
                // SAFETY: plain query.
                let process = unsafe { windows_sys::Win32::System::Memory::GetProcessHeap() } as u64;
                (done.ret != process).then(|| "not the process heap".into())
            }
            "XMemCreateWin32Heap" | "XMemVirtualAlloc" => {
                // After a refusal of an uncommitted base the library leaves the
                // thread's error (the harness's, 0x7e); after success too.
                let (ret, err, _) = outcome(want);
                if call == "XMemCreateWin32Heap" && (ret != 0 || err == Some(0x7e)) {
                    return ((done.ret == 0) != (ret == 0)).then(|| format!("ours {:x}", done.ret));
                }
                same_nullness(done, want)
            }
            "XMemGetWin32HeapStatistics" => {
                // The figures are the heap manager's.
                (done.ret == 0).then(|| "failed".into())
            }
            _ => same_result(done, want).or_else(|| same_bufs(done, want)),
        }
    });
}

#[test]
fn statistics_layouts_as_xmem_dll() {
    // Which bytes each layout writes (the harness's kernel figures are
    // zeroes, ours the host's): a byte still 0xcc is one left alone.
    for (name, text) in [("wss", include_str!("vectors/wss.txt")), ("wss_graphics", include_str!("vectors/wss_graphics.txt"))] {
        run(name, text, |_, call, _, done, want, _| {
            same_result(done, want)?;
            if call != "XMemGetWorkingSetStatistics" {
                return None;
            }
            let (_, _, bufs) = outcome(want);
            let theirs: Vec<bool> = bufs[&1].iter().map(|&b| b != 0xcc).collect();
            let ours: Vec<bool> = done.bufs[0].1[..theirs.len()].iter().map(|&b| b != 0xcc).collect();
            let last = |v: &[bool]| v.iter().rposition(|&x| x);
            (last(&ours) != last(&theirs)).then(|| format!("writes to {:x?}, xmem.dll to {:x?}", last(&ours), last(&theirs)))
        });
    }
    run("freepages", include_str!("vectors/freepages.txt"), |_, _, _, done, want, _| {
        same_result(done, want)?;
        let (_, _, bufs) = outcome(want);
        let theirs = bufs[&0].iter().rposition(|&b| b != 0xcc);
        let ours = done.bufs[0].1[..bufs[&0].len()].iter().rposition(|&b| b != 0xcc);
        (bufs[&0][0x38..] != done.bufs[0].1[0x38..0x40] || theirs.is_none() || ours.is_none())
            .then(|| "a byte past the 0x38 written".into())
    });
    run("layout", include_str!("vectors/layout.txt"), |_, _, _, done, want, _| same_result(done, want).or_else(|| same_bufs(done, want)));
    run("dump", include_str!("vectors/dump.txt"), |_, _, _, done, want, _| same_result(done, want));
    run("tracking", include_str!("vectors/tracking.txt"), |_, _, _, done, want, _| same_result(done, want));
}

/// `XMemVirtualQuery` on what `XMemVirtualAlloc` made: the console's
/// protection, state bits, XMem flags and tag. Addresses are compared
/// relative to the allocation; region sizes and host types are the host's.
#[test]
fn virtual_query_as_xmem_dll() {
    let text = include_str!("vectors/query_part.txt");
    run("query_part", text, |_, call, args, done, want, rets| {
        if call == "XMemVirtualAlloc" {
            return same_nullness(done, want);
        }
        same_result(done, want)?;
        let (_, _, bufs) = outcome(want);
        let (w, o) = (&bufs[&2], &done.bufs[0].1);
        let q = |b: &[u8], at: usize| u64::from_le_bytes(b[at..at + 8].try_into().unwrap());
        let d = |b: &[u8], at: usize| u32::from_le_bytes(b[at..at + 4].try_into().unwrap());
        let alloc = args[1].strip_prefix("r:").map(|r| r.split('+').next().unwrap().parse::<usize>().unwrap());
        let Some(alloc) = alloc else {
            // Free memory: CPU flags as the title partition's or none.
            return (q(w, 0x30) != q(o, 0x30)).then(|| format!("flags {:x}", q(o, 0x30)));
        };
        let theirs_base = q(w, 8);
        let mut diffs = Vec::new();
        for (field, at, wide) in [
            ("base", 0, true),
            ("allocation base", 8, true),
            ("allocation protection", 0x10, false),
            ("state", 0x20, false),
            ("protection", 0x24, false),
            ("flags", 0x30, true),
            ("tag code", 0x38, false),
            ("tag", 0x3c, false),
        ] {
            let (t, ours) = if wide { (q(w, at), q(o, at)) } else { (d(w, at) as u64, d(o, at) as u64) };
            let (t, ours) = if at < 0x10 { (t - theirs_base, ours - rets[alloc]) } else { (t, ours) };
            if t != ours {
                diffs.push(format!("{field} {ours:x} (xmem.dll {t:x})"));
            }
        }
        (!diffs.is_empty()).then(|| diffs.join(", "))
    });
}
