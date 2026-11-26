#!/usr/bin/env bash
# Builds lib/x86_64-windows/victory_unix.dll (with Wine's builtin marker) and
# lib/x86_64-unix/victory_unix.so. Needs gcc and x86_64-w64-mingw32-gcc.
set -euo pipefail
cd "$(dirname "$0")"
mkdir -p lib/x86_64-windows lib/x86_64-unix
# The two wine_server_* calls are ntdll.so's: linked against a stub of that
# name, so the library needs the ntdll.so the process has loaded already.
mkdir -p build
printf 'int wine_server_handle_to_fd(void){return 0;}\nint wine_server_fd_to_handle(void){return 0;}\n' > build/ntdll_stub.c
gcc -O2 -fPIC -shared -nostdlib -Wl,-soname,ntdll.so -o build/ntdll.so build/ntdll_stub.c
gcc -O2 -fPIC -shared -nostdlib -fno-stack-protector -o lib/x86_64-unix/victory_unix.so victory_unix.c build/ntdll.so
x86_64-w64-mingw32-gcc -O2 -shared -o lib/x86_64-windows/victory_unix.dll victory_unix_pe.c
python3 - <<'PY'
p = "lib/x86_64-windows/victory_unix.dll"
d = bytearray(open(p, "rb").read())
d[0x40:0x60] = b"Wine builtin DLL\0".ljust(32, b"\0")
open(p, "wb").write(d)
PY
echo "built lib/x86_64-windows/victory_unix.dll lib/x86_64-unix/victory_unix.so"
