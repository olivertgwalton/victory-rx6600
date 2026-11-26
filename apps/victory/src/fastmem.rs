//! `memcmp`, `bcmp`, `memcpy` and `memmove` for the MinGW build that runs
//! under Wine (Proton).
//!
//! A MinGW executable takes the C library's byte functions from
//! `msvcrt.dll`, and under Wine that is Wine's own: its `memcmp` compares
//! eight bytes at a time without vector instructions. The layer compares
//! hundreds of megabytes a frame (snapshots compared back, refreshes asked
//! whether anything is new, the relay's landings), and every comparison of
//! two slices in Rust ends in `memcmp`: in a profile of the world under
//! Proton it was 12-16 % of the refresh, submit and relay threads' samples.
//! Defined here, the linker takes these before the import library's.
//!
//! SSE2 only (every x86-64 has it), 64 bytes a round. The loops are written
//! with vector loads so that the compiler cannot turn them back into a call
//! to `memcmp` itself.

use core::arch::x86_64::{__m128i, _mm_and_si128, _mm_cmpeq_epi8, _mm_loadu_si128, _mm_movemask_epi8};

/// The first `n` bytes at `a` and `b` compared as unsigned bytes: below zero
/// if `a`'s first differing byte is the smaller, zero if they are equal.
///
/// # Safety
/// `a` and `b` are readable for `n` bytes (the C contract).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn memcmp(a: *const u8, b: *const u8, n: usize) -> i32 {
    // SAFETY: the caller's `n` bytes at each; unaligned loads.
    unsafe {
        let load = |p: *const u8, at: usize| _mm_loadu_si128(p.add(at).cast::<__m128i>());
        let mut i = 0;
        // Four lanes of 16 at once; a round with a difference is looked at
        // again 16 bytes at a time below.
        while i + 64 <= n {
            let eq = _mm_and_si128(
                _mm_and_si128(_mm_cmpeq_epi8(load(a, i), load(b, i)), _mm_cmpeq_epi8(load(a, i + 16), load(b, i + 16))),
                _mm_and_si128(_mm_cmpeq_epi8(load(a, i + 32), load(b, i + 32)), _mm_cmpeq_epi8(load(a, i + 48), load(b, i + 48))),
            );
            if _mm_movemask_epi8(eq) != 0xffff {
                break;
            }
            i += 64;
        }
        while i + 16 <= n {
            let same = _mm_movemask_epi8(_mm_cmpeq_epi8(load(a, i), load(b, i))) as u32;
            if same != 0xffff {
                let at = i + (!same).trailing_zeros() as usize;
                return i32::from(*a.add(at)) - i32::from(*b.add(at));
            }
            i += 16;
        }
        while i < n {
            let (x, y) = (*a.add(i), *b.add(i));
            if x != y {
                return i32::from(x) - i32::from(y);
            }
            i += 1;
        }
        0
    }
}

/// Whether the first `n` bytes at `a` and `b` differ (zero: equal).
///
/// # Safety
/// As [`memcmp`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn bcmp(a: *const u8, b: *const u8, n: usize) -> i32 {
    // SAFETY: the caller's contract.
    unsafe { memcmp(a, b, n) }
}

#[cfg(test)]
mod tests {
    #[test]
    fn agrees_with_byte_order() {
        let mut a = vec![7u8; 300];
        let b = a.clone();
        for at in [0usize, 5, 15, 16, 63, 64, 65, 200, 299] {
            for len in [at + 1, 300] {
                a[at] = 9;
                // SAFETY: both hold `len` bytes.
                let r = unsafe { super::memcmp(a.as_ptr(), b.as_ptr(), len) };
                assert!(r > 0, "at {at} len {len}");
                // SAFETY: as above.
                let r = unsafe { super::memcmp(b.as_ptr(), a.as_ptr(), len) };
                assert!(r < 0, "at {at} len {len}");
                a[at] = 7;
            }
        }
        // SAFETY: as above.
        assert_eq!(unsafe { super::memcmp(a.as_ptr(), b.as_ptr(), 300) }, 0);
        // SAFETY: zero bytes.
        assert_eq!(unsafe { super::memcmp(a.as_ptr(), b.as_ptr(), 0) }, 0);
    }
}

/// Copies `n` bytes from `src` to `dst` (they do not overlap) with
/// `rep movsb`, which processors with fast string moves (Intel since Ice
/// Lake, AMD since Zen 3) run as their fastest copy at every size. Wine's
/// msvcrt copies 16 bytes at a time with SSE2: in the world under Proton
/// that copy was 22-35 % of the submitting and refresh threads' samples.
///
/// # Safety
/// The C contract: `n` bytes readable at `src`, writable at `dst`, apart.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn memcpy(dst: *mut u8, src: *const u8, n: usize) -> *mut u8 {
    // SAFETY: the caller's contract; `rep movsb` copies forward, the
    // direction flag being clear by the calling convention.
    unsafe {
        core::arch::asm!(
            "rep movsb",
            inout("rcx") n => _,
            inout("rdi") dst => _,
            inout("rsi") src => _,
            options(nostack, preserves_flags)
        );
    }
    dst
}

/// As [`memcpy`], for buffers that may overlap: forward when the
/// destination lies below the source (or apart), else backward from the
/// last byte, with the direction flag set for the copy and cleared after.
///
/// # Safety
/// The C contract: `n` bytes readable at `src` and writable at `dst`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn memmove(dst: *mut u8, src: *const u8, n: usize) -> *mut u8 {
    if n == 0 {
        return dst;
    }
    let (d, s) = (dst as usize, src as usize);
    if d <= s || d >= s + n {
        // SAFETY: forward is right when the destination is below the
        // source or apart from it.
        return unsafe { memcpy(dst, src, n) };
    }
    // SAFETY: the caller's contract; from the last byte down, then the
    // direction flag back as the calling convention wants it.
    unsafe {
        core::arch::asm!(
            "std",
            "rep movsb",
            "cld",
            inout("rcx") n => _,
            inout("rdi") dst.add(n - 1) => _,
            inout("rsi") src.add(n - 1) => _,
            options(nostack)
        );
    }
    dst
}

#[cfg(test)]
mod copy_tests {
    #[test]
    fn copies_and_moves_both_ways() {
        let src: Vec<u8> = (0..=255u8).cycle().take(5000).collect();
        for n in [0usize, 1, 7, 16, 63, 64, 65, 4096, 4999] {
            let mut dst = vec![0u8; 5000];
            // SAFETY: both hold 5000 bytes.
            unsafe { super::memcpy(dst.as_mut_ptr(), src.as_ptr(), n) };
            assert_eq!(&dst[..n], &src[..n], "memcpy {n}");
            assert!(dst[n..].iter().all(|&b| b == 0), "memcpy past {n}");
        }
        for (from, to, n) in [(0usize, 10usize, 100usize), (10, 0, 100), (0, 1, 4000), (1, 0, 4000), (5, 5, 50)] {
            let mut buf: Vec<u8> = (0..=255u8).cycle().take(5000).collect();
            let want = {
                let mut w = buf.clone();
                w.copy_within(from..from + n, to);
                w
            };
            // SAFETY: inside the buffer.
            unsafe { super::memmove(buf.as_mut_ptr().add(to), buf.as_ptr().add(from), n) };
            assert_eq!(buf, want, "memmove {from}->{to} {n}");
        }
    }
}
