//! Per-call cost of the core API sets the title calls most, as the title
//! reaches them (our overrides, or the host's own function where we do not
//! override it), and of our import binding. `#[ignore]`d: run with
//! `~/victory/test-core.sh core_bench --ignored --nocapture --test-threads=1`.
#![allow(non_snake_case)]

use std::ffi::c_void;
use std::hint::black_box;
use std::time::Instant;

use crate::modules::files::host;

/// Median nanoseconds per call over 15 rounds of `n` calls.
fn per_call(n: u32, mut f: impl FnMut()) -> f64 {
    for _ in 0..n / 10 {
        f();
    }
    let mut rounds: Vec<f64> = (0..15)
        .map(|_| {
            let start = Instant::now();
            for _ in 0..n {
                f();
            }
            start.elapsed().as_nanos() as f64 / f64::from(n)
        })
        .collect();
    rounds.sort_by(f64::total_cmp);
    rounds[rounds.len() / 2]
}

/// Runs and reports one benchmark, if `VICTORY_BENCH` (a part of its name)
/// does not leave it out; a profile run repeats it `VICTORY_BENCH_REPEAT` times.
fn bench(name: &str, n: u32, mut f: impl FnMut()) {
    if std::env::var("VICTORY_BENCH").is_ok_and(|only| !name.contains(&only)) {
        return;
    }
    let repeat = std::env::var("VICTORY_BENCH_REPEAT")
        .ok()
        .and_then(|r| r.parse().ok())
        .unwrap_or(1);
    let sampling = sampler::start();
    for _ in 0..repeat {
        let ns = per_call(n, &mut f);
        println!("BENCH {name:<48} {ns:>9.1} ns");
    }
    sampling.finish(name);
}

/// With the `diagnostics` feature and `VICTORY_BENCH_SAMPLES=<file>`: the
/// benchmark thread's stacks, sampled every 100 µs by another thread and
/// appended to the file folded (root first, `module+rva` frames), for a
/// flamegraph (`~/victory/prof-core.sh`; perf cannot sample on this box).
mod sampler {
    #[cfg(feature = "diagnostics")]
    pub(super) struct Sampling(
        Option<(
            std::sync::Arc<std::sync::atomic::AtomicBool>,
            std::thread::JoinHandle<Vec<Vec<u64>>>,
        )>,
    );
    #[cfg(not(feature = "diagnostics"))]
    pub(super) struct Sampling;

    #[cfg(not(feature = "diagnostics"))]
    pub(super) fn start() -> Sampling {
        Sampling
    }

    #[cfg(not(feature = "diagnostics"))]
    impl Sampling {
        pub(super) fn finish(self, _: &str) {}
    }

    #[cfg(feature = "diagnostics")]
    pub(super) fn start() -> Sampling {
        use std::sync::atomic::{AtomicBool, Ordering};
        use windows_sys::Win32::Foundation::{DUPLICATE_SAME_ACCESS, DuplicateHandle};
        use windows_sys::Win32::System::Threading::{GetCurrentProcess, GetCurrentThread};
        if std::env::var_os("VICTORY_BENCH_SAMPLES").is_none() {
            return Sampling(None);
        }
        let mut me = std::ptr::null_mut();
        // SAFETY: a real handle to this thread, for the sampler.
        unsafe {
            DuplicateHandle(
                GetCurrentProcess(),
                GetCurrentThread(),
                GetCurrentProcess(),
                &mut me,
                0,
                0,
                DUPLICATE_SAME_ACCESS,
            );
        }
        let me = me as usize;
        let stop = std::sync::Arc::new(AtomicBool::new(false));
        let until = stop.clone();
        let thread = std::thread::spawn(move || {
            let mut samples = Vec::new();
            while !until.load(Ordering::Relaxed) {
                samples.push(victory_loader::sample_thread(me as _));
                std::thread::sleep(std::time::Duration::from_micros(100));
            }
            // SAFETY: duplicated above.
            unsafe { windows_sys::Win32::Foundation::CloseHandle(me as _) };
            samples
        });
        Sampling(Some((stop, thread)))
    }

    #[cfg(feature = "diagnostics")]
    impl Sampling {
        pub(super) fn finish(self, name: &str) {
            use std::io::Write;
            let Some((stop, thread)) = self.0 else {
                return;
            };
            stop.store(true, std::sync::atomic::Ordering::Relaxed);
            let samples = thread.join().unwrap_or_default();
            let mut folded = std::collections::HashMap::<String, u32>::new();
            for frames in samples.iter().filter(|f| !f.is_empty()) {
                let stack: Vec<String> = frames
                    .iter()
                    .rev()
                    .map(|&pc| victory_loader::locate(pc as usize))
                    .collect();
                *folded
                    .entry(format!("{name};{}", stack.join(";")))
                    .or_default() += 1;
            }
            let path = std::env::var_os("VICTORY_BENCH_SAMPLES").unwrap_or_default();
            if let Ok(mut file) = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(path)
            {
                for (stack, n) in folded {
                    let _ = writeln!(file, "{stack} {n}");
                }
            }
        }
    }
}

/// The host's function, as an unoverridden title import is bound to it.
fn bound(dll: &str, name: &str) -> usize {
    crate::registry::lookup(dll, name)
        .or_else(|| crate::host::function(dll, name))
        .unwrap_or_else(|| panic!("{dll}!{name}"))
}

#[test]
#[ignore = "benchmark"]
fn core_bench() {
    type Qpc = extern "system" fn(*mut i64) -> i32;
    type NoArgs64 = extern "system" fn() -> u64;
    type Ptr = extern "system" fn(*mut c_void);
    type TlsGet = extern "system" fn(u32) -> *mut c_void;
    type Wait = extern "system" fn(*const c_void, *const c_void, usize, u32) -> i32;
    type Handle = extern "system" fn(*mut c_void) -> i32;
    type WaitOne = extern "system" fn(*mut c_void, u32) -> u32;
    // SAFETY (every transmute): the host's functions with their signatures.
    unsafe {
        let qpc: Qpc = std::mem::transmute(bound(
            "api-ms-win-core-profile-l1-1-0.dll",
            "QueryPerformanceCounter",
        ));
        let mut t = 0i64;
        bench("QueryPerformanceCounter", 1_000_000, || {
            qpc(&mut t);
            black_box(t);
        });
        let tick: NoArgs64 = std::mem::transmute(bound(
            "api-ms-win-core-sysinfo-l1-1-0.dll",
            "GetTickCount64",
        ));
        bench("GetTickCount64", 1_000_000, || {
            black_box(tick());
        });
        let precise: Ptr = std::mem::transmute(bound(
            "api-ms-win-core-sysinfo-l1-2-0.dll",
            "GetSystemTimePreciseAsFileTime",
        ));
        let mut ft = 0u64;
        bench("GetSystemTimePreciseAsFileTime", 1_000_000, || {
            precise((&raw mut ft).cast());
            black_box(ft);
        });
        let tid: extern "system" fn() -> u32 = std::mem::transmute(bound(
            "api-ms-win-core-processthreads-l1-1-0.dll",
            "GetCurrentThreadId",
        ));
        bench("GetCurrentThreadId", 1_000_000, || {
            black_box(tid());
        });
        let tls_alloc: extern "system" fn() -> u32 = std::mem::transmute(bound(
            "api-ms-win-core-processthreads-l1-1-0.dll",
            "TlsAlloc",
        ));
        let tls_get: TlsGet = std::mem::transmute(bound(
            "api-ms-win-core-processthreads-l1-1-0.dll",
            "TlsGetValue",
        ));
        let slot = tls_alloc();
        bench("TlsGetValue", 1_000_000, || {
            black_box(tls_get(slot));
        });
        let fls_alloc: extern "system" fn(usize) -> u32 =
            std::mem::transmute(bound("api-ms-win-core-fibers-l1-1-0.dll", "FlsAlloc"));
        let fls_get: TlsGet =
            std::mem::transmute(bound("api-ms-win-core-fibers-l1-1-0.dll", "FlsGetValue"));
        let slot = fls_alloc(0);
        bench("FlsGetValue", 1_000_000, || {
            black_box(fls_get(slot));
        });
        let acquire: Ptr = std::mem::transmute(bound(
            "api-ms-win-core-synch-l1-1-0.dll",
            "AcquireSRWLockExclusive",
        ));
        let release: Ptr = std::mem::transmute(bound(
            "api-ms-win-core-synch-l1-1-0.dll",
            "ReleaseSRWLockExclusive",
        ));
        let mut lock = 0usize;
        bench("AcquireSRWLockExclusive+Release", 1_000_000, || {
            acquire((&raw mut lock).cast());
            release((&raw mut lock).cast());
        });
        let init_cs: Ptr = std::mem::transmute(bound(
            "api-ms-win-core-synch-l1-1-0.dll",
            "InitializeCriticalSection",
        ));
        let enter: Ptr = std::mem::transmute(bound(
            "api-ms-win-core-synch-l1-1-0.dll",
            "EnterCriticalSection",
        ));
        let leave: Ptr = std::mem::transmute(bound(
            "api-ms-win-core-synch-l1-1-0.dll",
            "LeaveCriticalSection",
        ));
        let mut cs = [0u64; 5];
        init_cs(cs.as_mut_ptr().cast());
        bench("EnterCriticalSection+Leave", 1_000_000, || {
            enter(cs.as_mut_ptr().cast());
            leave(cs.as_mut_ptr().cast());
        });
        let wait: Wait =
            std::mem::transmute(bound("api-ms-win-core-synch-l1-2-0.dll", "WaitOnAddress"));
        let wake: Ptr = std::mem::transmute(bound(
            "api-ms-win-core-synch-l1-2-0.dll",
            "WakeByAddressSingle",
        ));
        let (value, other) = (1u32, 2u32);
        bench("WaitOnAddress (value differs)", 200_000, || {
            black_box(wait(
                (&raw const value).cast(),
                (&raw const other).cast(),
                4,
                0,
            ));
        });
        bench("WakeByAddressSingle (no waiter)", 1_000_000, || {
            wake((&raw const value).cast_mut().cast());
        });
        type CreateEvent = extern "system" fn(*mut c_void, i32, i32, *const u16) -> *mut c_void;
        let create: CreateEvent =
            std::mem::transmute(bound("api-ms-win-core-synch-l1-1-0.dll", "CreateEventW"));
        let set: Handle =
            std::mem::transmute(bound("api-ms-win-core-synch-l1-1-0.dll", "SetEvent"));
        let wait_one: WaitOne = std::mem::transmute(bound(
            "api-ms-win-core-synch-l1-1-0.dll",
            "WaitForSingleObject",
        ));
        let event = create(std::ptr::null_mut(), 0, 0, std::ptr::null());
        bench("SetEvent+WaitForSingleObject (auto-reset)", 200_000, || {
            set(event);
            black_box(wait_one(event, 0));
        });
        let switch: extern "system" fn() -> i32 = std::mem::transmute(bound(
            "api-ms-win-core-processthreads-l1-1-0.dll",
            "SwitchToThread",
        ));
        bench("SwitchToThread", 100_000, || {
            black_box(switch());
        });
        type Submit = extern "system" fn(*mut c_void);
        type CreateWork = extern "system" fn(
            extern "system" fn(*mut c_void, *mut c_void, *mut c_void),
            *mut c_void,
            *mut c_void,
        ) -> *mut c_void;
        type WaitWork = extern "system" fn(*mut c_void, i32);
        extern "system" fn work(_: *mut c_void, _: *mut c_void, _: *mut c_void) {}
        let create_work: CreateWork = std::mem::transmute(bound(
            "api-ms-win-core-threadpool-l1-2-0.dll",
            "CreateThreadpoolWork",
        ));
        let submit: Submit = std::mem::transmute(bound(
            "api-ms-win-core-threadpool-l1-2-0.dll",
            "SubmitThreadpoolWork",
        ));
        let wait_work: WaitWork = std::mem::transmute(bound(
            "api-ms-win-core-threadpool-l1-2-0.dll",
            "WaitForThreadpoolWorkCallbacks",
        ));
        let item = create_work(work, std::ptr::null_mut(), std::ptr::null_mut());
        bench("SubmitThreadpoolWork+WaitForCallbacks", 20_000, || {
            submit(item);
            wait_work(item, 0);
        });
    }

    // Our own paths.
    let null = std::ptr::null_mut();
    bench("registry::lookup (an override)", 200_000, || {
        black_box(crate::registry::lookup(
            "api-ms-win-core-file-l1-1-0.dll",
            "CreateFileW",
        ));
    });
    bench("registry::lookup (left to the host)", 200_000, || {
        black_box(crate::registry::lookup(
            "api-ms-win-core-synch-l1-2-0.dll",
            "WaitOnAddress",
        ));
    });
    let runtime = super::kernel::LoadLibraryA(c"xgameruntime.dll".as_ptr().cast());
    bench("GetProcAddress (pseudo module)", 200_000, || {
        black_box(super::kernel::GetProcAddress(
            runtime,
            c"XGameRuntimeInitialize".as_ptr().cast(),
        ));
    });
    let kernel32 = super::kernel::GetModuleHandleA(c"kernel32.dll".as_ptr().cast());
    bench("GetProcAddress (host DLL)", 100_000, || {
        black_box(super::kernel::GetProcAddress(
            kernel32,
            c"GetTickCount".as_ptr().cast(),
        ));
    });
    bench(
        "GetProcAddress (host DLL, overridden name)",
        100_000,
        || {
            black_box(super::kernel::GetProcAddress(
                kernel32,
                c"CreateFileW".as_ptr().cast(),
            ));
        },
    );
    bench("GetModuleHandleA (pseudo module)", 100_000, || {
        black_box(super::kernel::GetModuleHandleA(
            c"xgameruntime.dll".as_ptr().cast(),
        ));
    });
    bench("GetModuleHandleA (host DLL)", 100_000, || {
        black_box(super::kernel::GetModuleHandleA(
            c"kernel32.dll".as_ptr().cast(),
        ));
    });
    let bad = 0x8u32;
    bench("CoInitializeEx (refused flags)", 1_000_000, || {
        black_box(super::com::CoInitializeEx(null, black_box(bad)));
    });
    // SAFETY: a pseudo handle.
    let me = unsafe { windows_sys::Win32::System::Threading::GetCurrentThread() };
    let mut before = 0u32;
    bench("SetThreadIdealProcessorEx", 200_000, || {
        black_box(super::threads::SetThreadIdealProcessorEx(
            me,
            &(1 << 16),
            &mut before,
        ));
    });
    // Overrides that only add checks in front of the host: the cost of the checks.
    let (mut variable, mut lock) = (0usize, 0usize);
    let host_sleep = host!("kernel32.dll", "SleepConditionVariableSRW": extern "system" fn(*mut c_void, *mut c_void, u32, u32) -> i32);
    bench("SleepConditionVariableSRW 0 ms (host)", 200_000, || {
        black_box(host_sleep((&raw mut variable).cast(), (&raw mut lock).cast(), 0, 0));
    });
    bench("SleepConditionVariableSRW 0 ms (ours)", 200_000, || {
        black_box(super::synch::SleepConditionVariableSRW((&raw mut variable).cast(), (&raw mut lock).cast(), 0, 0));
    });
    let host_local = host!("kernel32.dll", "LocalAlloc": extern "system" fn(u32, usize) -> *mut c_void);
    let free = |p: *mut c_void| {
        // SAFETY: ours.
        unsafe { windows_sys::Win32::Foundation::LocalFree(p) };
    };
    bench("LocalAlloc+LocalFree (host)", 200_000, || free(host_local(0, 64)));
    bench("LocalAlloc+LocalFree (ours)", 200_000, || free(super::synch::LocalAlloc(0, 64)));
    extern "system" fn nothing(_: *mut c_void) -> u32 {
        0
    }
    // SAFETY: a pseudo handle.
    let process = unsafe { windows_sys::Win32::System::Threading::GetCurrentProcess() };
    let host_create = host!("kernel32.dll", "CreateRemoteThreadEx": extern "system" fn(
        *mut c_void, *mut c_void, usize, *mut c_void, *mut c_void, u32, *mut u8, *mut u32,
    ) -> *mut c_void);
    let join = |t: *mut c_void| {
        // SAFETY: our thread.
        unsafe {
            windows_sys::Win32::System::Threading::WaitForSingleObject(t, u32::MAX);
            windows_sys::Win32::Foundation::CloseHandle(t);
        }
    };
    let start = nothing as *mut c_void;
    bench("CreateRemoteThreadEx+join (host)", 300, || {
        join(host_create(
            process,
            null,
            0,
            start,
            null,
            0,
            null.cast(),
            null.cast(),
        ));
    });
    bench("CreateRemoteThreadEx+join (ours)", 300, || {
        join(super::threads::CreateRemoteThreadEx(
            process,
            null,
            0,
            start,
            null,
            0,
            null.cast(),
            null.cast(),
        ));
    });
}
