# wineserver: mapped views in a tree

Built now with the dma-buf changes by `../wine-dmabuf/build.sh`, which applies these scripts first. This folder's `build.sh` builds the view tree alone and would put back a wineserver whose memfds cannot be sealed.

Under Proton, wineserver keeps each process's mapped section views in a linked
list. `find_mapped_view` (every unmap), `find_mapped_addr` and
`is_valid_view_addr` (every map) walk the whole list. VIctory emulates the
console's physical memory with section views, so the title holds a very large
number of them and remaps some 80 a frame. Profiled on GE-Proton 11-14 in the
first rooms (Series X, RX 6600 XT): wineserver at 91% of a core, all of it in
`req_unmap_view` and `is_valid_view_addr`, and the title's threads waiting on
it. The frame rate was 0.8 with the GPU 4% busy.

The patch (`apply_view_tree.py`, `apply_owner_init.py`, applied to GE's
patched Wine 11 tree) keeps the views in a red-black tree by base address as
well as the list (the list keeps its order: main executable first, DLLs in load
order). Lookups by base are exact, by address take the view with the highest
base at or below it, and the overlap check needs only the view with the highest
base below the range's end, as views never overlap. Nothing else changes, and
the protocol is the same, so the rebuilt wineserver drops into the same
GE-Proton.

Measured (skornuta/victory-fork, same scene and settings): 0.80 -> 6.90 fps
median (1,256 -> 145 ms a frame), wineserver 91% -> 13% of a core, the GPU 4% ->
75% busy.

`build.sh` reproduces it: GE's source at the tag, its Wine patch section, this
patch, a server-only build in Debian 13 (the Steam Linux Runtime 4's glibc),
installed into a copy named GE-Proton11-14-viewtree. The original GE-Proton is
left as it is.
