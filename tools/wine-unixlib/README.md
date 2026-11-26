# victory_unix: kernel write tracking under Wine

`victory_unix.dll` is a Wine builtin whose only job is to carry
`victory_unix.so`, a Unix library that runs inside the game's own process.
It registers title memory with a userfaultfd in asynchronous write-protect
mode and uses `PAGEMAP_SCAN` to report the pages written since the last scan,
protecting them again in the same step. Linux added both for Windows'
`GetWriteWatch`. Unlike Wine's write watch, this works for section views, and
no fault reaches user mode. `crates/victory-gpu/src/uffd.rs` uses it.

The `.so` makes raw system calls and links no libc, so it loads in any runtime
container (Steam Linux Runtime included).

Build (Fedora, or the build container): `./build.sh` writes `lib/x86_64-windows/victory_unix.dll`
and `lib/x86_64-unix/victory_unix.so`. Run the game with
`WINEDLLPATH=<this folder>/lib` (Proton appends it to its own). Needs Linux 6.7
or later and userfaultfd allowed for user mode (the library asks for
`UFFD_USER_MODE_ONLY`, which `vm.unprivileged_userfaultfd=0` permits).
`VICTORY_UFFD=1` turns it on.

It also makes title memory a dma-buf for the GPU (`dmabuf`): the section's
memfd sealed against shrinking, a udmabuf of the range, and a handle wrapping
it that vkAllocateMemory imports with no copy. That needs Wine built with
`tools/wine-dmabuf` (sealable memfds, and win32u importing such handles) and
access to `/dev/udmabuf`. `crates/victory-gpu/src/dmabuf.rs` uses it.
