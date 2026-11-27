# xg-terms

Reads the address terms of the console GPU's pipe and bank XOR swizzle modes out of the console's own texture
library, `xg_xs.dll` (from the game OS, `Windows/System32`), and writes
`crates/victory-formats/src/texture_xor.rs` and the test points `texture_xor_points.txt`.

`xgterms.exe` loads the library under Wine (with `xmem.dll`, a two-export stand-in for the console's memory
library) and asks `XGCreateTextureComputer` / `GetTexelElementOffsetBytes` (computer slot 0x30) for the offset of
every single-bit coordinate, per mode, element size and dimension, then for pseudo-random points of a set of
textures with mips, arrays and 3D. `XGSetHardwareVersion(4)` gives the other pipe configuration (Series S).

```sh
x86_64-w64-mingw32-gcc -O1 -shared -o xmem.dll xmem.c xmem.def
x86_64-w64-mingw32-gcc -O1 -o xgterms.exe xgterms.c
WINEDLLOVERRIDES=xmem=n wine xgterms.exe 0 points | tr -d '\r' > anaconda.txt
WINEDLLOVERRIDES=xmem=n wine xgterms.exe 4 points | tr -d '\r' > lockhart.txt
python3 gen.py anaconda.txt lockhart.txt ../../crates/victory-formats/src
```

Findings while making it (2026-11-27): offsets inside a block are XORs of per-bit terms of x, y and the slice or
depth, with pipe bits taking x/y bits up to two above the block's size; block placement, mips and tails are the
non-XOR modes'. The library refuses `SW_4KB_Z_X`, `SW_4KB_R_X`, `SW_64KB_R` and the `_T` Z and R modes, 3D in
`D_X`/`D_T`, and 16-byte `Z_X`. A single-mip `Z_X` texture described with the depth-stencil flag answers
differently (texels 0..7 of a row share an offset); the terms are read without it, as the GPU's pattern for the
mode. `XGSuggestSwizzleMode` picks `SW_4KB_S_X` for an unordered-access texture (the layer's `XGSuggestSwizzleMode`
picks `SW_4KB_S`).
