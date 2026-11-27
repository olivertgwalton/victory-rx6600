//! Renderer port #8 (`VICTORY_PORT_WAITS=1`, read in `victory-gpu`'s
//! `port_waits`): the title's wait for a value the GPU writes sleeps until
//! the layer writes it, instead of asking again and again.
//!
//! What the title does (game_scarlett_master.exe 1.0.2.1, the function at
//! image +0x4fb0ad0): it waits until a 64-bit word that a bottom-of-pipe
//! write of a list sets has reached a value (`rdi` the word, `rsi` the
//! value; reached when bit 55 of their difference is clear). The loop at
//! +0x4fb0bd0: `monitorx` on the word, a look, `mwaitx` with a short timer,
//! a look, then `Sleep(0)` through +0x4fb0e90 and a look at its own
//! timeout (+0x10a0c0), and again. On the console the GPU's store into the
//! one memory ends the `mwaitx`. Here the store is the layer's, made when
//! the list's flight has landed, and the title's render thread spins
//! meanwhile: run prof1, 78 % of one core, two thirds of it in the system
//! call of `Sleep(0)`, while the frame waits for exactly this thread
//! (timeline tl3: 21 of 45 ms).
//!
//! The call of `Sleep(0)` becomes a call of code of ours that hands the
//! word and the value to `victory-gpu`, which waits on the word's address
//! (`WaitOnAddress`) until a store of the layer's wakes it
//! (`Written::write` has always called `WakeByAddressAll`), with a short
//! timeout for stores that tell nobody. The loop's own looks, before and
//! after, stay as they are. Both registers are saved by a callee, so they
//! hold the word and the value at the call.
//!
//! The site's bytes are compared before anything is written; another build
//! of the title is left alone and said so.

/// The call of `Sleep(0)` in the wait loop, its offset in the image, and
/// the bytes from it to the loop's jump back.
pub const SITE: usize = 0x4fb0c22;
pub const BYTES: [u8; 15] = [0xe8, 0x69, 0x02, 0x00, 0x00, 0x4c, 0x89, 0xf9, 0xe8, 0x91, 0x94, 0x15, 0xfb, 0xeb, 0x9f];

/// The code the call goes to: `mov rcx, rdi ; mov rdx, rsi ; mov rax,
/// wait ; jmp rax` (`wait` returns to the loop).
fn stub(wait: usize) -> Vec<u8> {
    let mut out = vec![0x48, 0x89, 0xf9, 0x48, 0x89, 0xf2, 0x48, 0xb8];
    out.extend((wait as u64).to_le_bytes());
    out.extend([0xff, 0xe0]);
    out
}

/// What became of the site.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Hooked {
    Done,
    /// Its bytes are not the ones expected: left alone.
    Differs,
    /// Outside the image, no room for code near it, or Windows refused.
    Failed(&'static str),
}

/// Hooks the wait loop of the image of `size` bytes at `base`: its
/// `Sleep(0)` calls `wait(word, value)` (`extern "system"`).
///
/// # Safety
/// The image stays mapped and no thread runs its code yet; `wait` is a
/// function of that shape that lives as long as the image runs.
#[cfg(windows)]
pub unsafe fn apply(base: usize, size: usize, wait: usize) -> Hooked {
    use windows_sys::Win32::System::Diagnostics::Debug::FlushInstructionCache;
    use windows_sys::Win32::System::Memory::{PAGE_EXECUTE_READWRITE, VirtualProtect};
    use windows_sys::Win32::System::Threading::GetCurrentProcess;
    let done = (|| {
        if SITE + BYTES.len() > size {
            return Hooked::Failed("outside the image");
        }
        let at = base + SITE;
        // SAFETY: inside the mapped image (checked above).
        if unsafe { std::slice::from_raw_parts(at as *const u8, BYTES.len()) } != BYTES {
            return Hooked::Differs;
        }
        let Some(to) = crate::console_insn::near(at) else {
            return Hooked::Failed("no room for code near the title's");
        };
        let body = stub(wait);
        let Some(jump) = crate::console_insn::jump(at, to) else {
            return Hooked::Failed("out of reach");
        };
        // A call, not a jump: the same displacement after another opcode.
        let call = [0xe8, jump[1], jump[2], jump[3], jump[4]];
        let mut old = 0;
        // SAFETY: our own code memory, which nothing runs yet; then the
        // call, whose page is made writable for it and put back, and which
        // nothing runs either.
        unsafe {
            std::ptr::copy_nonoverlapping(body.as_ptr(), to as *mut u8, body.len());
            if VirtualProtect(at as *const _, call.len(), PAGE_EXECUTE_READWRITE, &mut old) == 0 {
                return Hooked::Failed("Windows would not make it writable");
            }
            std::ptr::copy_nonoverlapping(call.as_ptr(), at as *mut u8, call.len());
            VirtualProtect(at as *const _, call.len(), old, &mut old);
            FlushInstructionCache(GetCurrentProcess(), at as *const _, call.len());
        }
        Hooked::Done
    })();
    match done {
        Hooked::Done => tracing::info!(site = format_args!("{SITE:#x}"), "renderer port 8: the title's wait for a value the GPU writes sleeps until the layer writes it (VICTORY_PORT_WAITS)"),
        Hooked::Differs => tracing::warn!(site = format_args!("{SITE:#x}"), "renderer port 8: not patched, the title's bytes there are not the ones expected (another build?)"),
        Hooked::Failed(why) => tracing::warn!(site = format_args!("{SITE:#x}"), why, "renderer port 8: not patched"),
    }
    done
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_stub_hands_over_the_word_and_the_value() {
        let s = stub(0x1122334455667788);
        assert_eq!(&s[..8], &[0x48, 0x89, 0xf9, 0x48, 0x89, 0xf2, 0x48, 0xb8]);
        assert_eq!(&s[8..16], &0x1122334455667788u64.to_le_bytes());
        assert_eq!(&s[16..], &[0xff, 0xe0]);
    }

    #[test]
    fn the_site_is_the_call_before_the_loops_jump_back() {
        assert_eq!(BYTES[0], 0xe8);
        // The call's target is the `Sleep(0)` at +0x4fb0e90.
        let rel = i32::from_le_bytes([BYTES[1], BYTES[2], BYTES[3], BYTES[4]]) as i64;
        assert_eq!((SITE as i64 + 5 + rel) as usize, 0x4fb0e90);
        assert_eq!(&BYTES[13..], &[0xeb, 0x9f]);
    }
}
