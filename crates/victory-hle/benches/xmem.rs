//! The xmem memory paths, timed (criterion). Run under Wine:
//! `cargo bench -p victory-hle --bench xmem --no-run`, then the bench exe
//! with `--bench`.
#![allow(clippy::not_unsafe_ptr_arg_deref)]

use std::ffi::c_void;
use std::hint::black_box;

use criterion::{Criterion, criterion_group, criterion_main};
use victory_hle::modules::xmem::{self as x, spec};

const TYPE1_SMALL: u64 = 1 << 29 | 1 << 14; // graphics, 64 KiB pages
const TYPE3_MEDIUM: u64 = 3 << 29 | 12 << 24 | 1 << 14;

fn decisions(c: &mut Criterion) {
    // For resolving profiles: where the exe is mapped.
    // SAFETY: plain query.
    let base = unsafe { windows_sys::Win32::System::LibraryLoader::GetModuleHandleA(std::ptr::null()) };
    eprintln!("image base {base:p}");
    let env = spec::Env::RETAIL;
    c.bench_function("spec::virtual_alloc 64K graphics reserve", |b| {
        b.iter(|| spec::virtual_alloc(&env, 0, black_box(0x10_0000), 0x2040_2000, 0x283, 0x2004))
    });
    c.bench_function("spec::Route::of", |b| b.iter(|| spec::Route::of(&env, black_box(0x100), black_box(TYPE1_SMALL))));
    c.bench_function("XMemMakeTag", |b| b.iter(|| x::more::XMemMakeTag(black_box(0x586d_5641))));
}

fn heaps(c: &mut Criterion) {
    c.bench_function("XMemAllocDefault+Free type 1 256 B", |b| {
        b.iter(|| {
            let p = x::XMemAllocDefault(black_box(0x100), TYPE1_SMALL);
            x::XMemFreeDefault(p, TYPE1_SMALL);
        })
    });
    c.bench_function("XMemAllocDefault+Free type 3 4 KiB", |b| {
        b.iter(|| {
            let p = x::XMemAllocDefault(black_box(0x1000), TYPE3_MEDIUM);
            x::XMemFreeDefault(p, TYPE3_MEDIUM);
        })
    });
    c.bench_function("XMemAllocDefault+Free process heap 64 B", |b| {
        b.iter(|| {
            let p = x::XMemAllocDefault(black_box(0x40), 0);
            x::XMemFreeDefault(p, 0);
        })
    });
    // A heap with many live blocks of mixed sizes: one free and one
    // allocation per iteration, oldest first.
    c.bench_function("heap churn 4096 live blocks", |b| {
        let sizes = [0x40usize, 0x100, 0x300, 0xf00];
        let mut live: std::collections::VecDeque<*mut c_void> =
            (0..4096).map(|i| x::XMemAllocDefault(sizes[i % 4], TYPE1_SMALL)).collect();
        let mut i = 0;
        b.iter(|| {
            x::XMemFreeDefault(live.pop_front().unwrap(), TYPE1_SMALL);
            live.push_back(x::XMemAllocDefault(sizes[i % 4], TYPE1_SMALL));
            i += 1;
        });
        for p in live {
            x::XMemFreeDefault(p, TYPE1_SMALL);
        }
    });
}

fn virtual_memory(c: &mut Criterion) {
    c.bench_function("XMemVirtualAlloc+release 64 KiB CPU commit", |b| {
        b.iter(|| {
            let p = x::XMemVirtualAlloc(std::ptr::null_mut(), 0x1_0000, 0x3000, 1, 4);
            x::title_virtual_free_for_bench(p);
        })
    });
    let p = x::XMemVirtualAlloc(std::ptr::null_mut(), 0x10_0000, 0x2040_3000, 3, 0x2004);
    c.bench_function("XMemVirtualQuery", |b| {
        let mut info = [0u8; 0x40];
        b.iter(|| x::XMemVirtualQuery(-1isize as _, black_box(p), info.as_mut_ptr().cast()))
    });
}

/// The floor: the host's own calls for 64 one-page views of a section into
/// placeholders, mapped and unmapped, with nothing of ours around them.
fn host_floor(c: &mut Criterion) {
    use windows_sys::Win32::System::Memory::*;
    const PAGE: usize = 64 << 10;
    // SAFETY: plain host calls on memory of our own.
    unsafe {
        let section = CreateFileMappingW(-1isize as _, std::ptr::null(), PAGE_READWRITE | SEC_COMMIT, 0, (64 * PAGE) as u32, std::ptr::null());
        let process = windows_sys::Win32::System::Threading::GetCurrentProcess();
        let base = VirtualAlloc2(process, std::ptr::null(), 64 * PAGE, MEM_RESERVE | MEM_RESERVE_PLACEHOLDER, PAGE_NOACCESS, std::ptr::null_mut(), 0) as usize;
        for i in 0..63 {
            VirtualFree((base + i * PAGE) as *mut c_void, PAGE, MEM_RELEASE | MEM_PRESERVE_PLACEHOLDER);
        }
        c.bench_function("host floor: 64 views into placeholders, map+unmap", |b| {
            b.iter(|| {
                for i in 0..64 {
                    let at = (base + i * PAGE) as *mut c_void;
                    let v = MapViewOfFile3(section, process, at, (i * PAGE) as u64, PAGE, MEM_REPLACE_PLACEHOLDER, PAGE_READWRITE, std::ptr::null_mut(), 0);
                    assert_eq!(v.Value, at);
                }
                for i in 0..64 {
                    let at = (base + i * PAGE) as *mut c_void;
                    UnmapViewOfFile2(process, MEMORY_MAPPED_VIEW_ADDRESS { Value: at }, MEM_PRESERVE_PLACEHOLDER);
                }
            })
        });
    }
}

fn physical(c: &mut Criterion) {
    let mut n = 64usize;
    let mut pages = vec![0usize; 64];
    assert_eq!(x::XMemAllocatePhysicalPages(0x2040_0000, &mut n, pages.as_mut_ptr()), 1);
    let base = x::XMemVirtualAlloc(std::ptr::null_mut(), 64 << 16, 0x2040_2000, 0x21, 4) as usize;
    let at: Vec<*mut c_void> = (0..64).map(|i| (base + (i << 16)) as *mut c_void).collect();
    c.bench_function("XMemMapPhysicalPagesScatter 64 pages map+unmap", |b| {
        b.iter(|| {
            assert_eq!(x::XMemMapPhysicalPagesScatter(at.as_ptr(), 64, pages.as_ptr()), 1);
            assert_eq!(x::XMemMapPhysicalPagesScatter(at.as_ptr(), 64, std::ptr::null()), 1);
        })
    });
    c.bench_function("XMemMapPhysicalPages 64 pages map+unmap", |b| {
        b.iter(|| {
            assert!(!x::XMemMapPhysicalPages(base as *mut c_void, 64, pages.as_ptr()).is_null());
            assert!(!x::XMemMapPhysicalPages(base as *mut c_void, 64, std::ptr::null()).is_null());
        })
    });
}

criterion_group! {
    name = benches;
    config = Criterion::default().sample_size(30).measurement_time(std::time::Duration::from_secs(3)).warm_up_time(std::time::Duration::from_secs(1));
    targets = decisions, heaps, virtual_memory, host_floor, physical
}
criterion_main!(benches);
