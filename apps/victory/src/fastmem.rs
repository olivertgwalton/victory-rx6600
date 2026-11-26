//! `memcmp` and `bcmp` for the MinGW build that runs under Wine (Proton).
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
