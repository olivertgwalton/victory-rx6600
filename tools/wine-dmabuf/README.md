# Wine: title memory to the GPU as a dma-buf

VIctory emulates the console's one memory with section views. Today the card
gets copies of the title's bytes. These patches let it read and write the
pages themselves, through a dma-buf of the section's memfd, with no copy (see
`crates/victory-gpu/src/dmabuf.rs`).

`apply.py` patches GE-Proton 11-14's Wine tree:

- `server/mapping.c`: section memfds are made with `MFD_ALLOW_SEALING`.
  udmabuf takes only a memfd sealed against shrinking, and only a memfd made
  sealable can be sealed. Nothing seals it until the layer exports a range.
- `dlls/win32u/vulkan.c`: `vkAllocateMemory` with
  `VkImportMemoryWin32HandleInfoKHR` (`OPAQUE_WIN32`) of a handle that is no
  D3DKMT resource but wraps a dma-buf fd (`wine_server_fd_to_handle`)
  imports it as `VK_EXTERNAL_MEMORY_HANDLE_TYPE_DMA_BUF_BIT_EXT`. The memory
  type is checked against `vkGetMemoryFdPropertiesKHR` first, and
  `VK_ERROR_INVALID_EXTERNAL_HANDLE` is returned where it does not fit. Before
  this, Proton imported every handle as the host's default type, opaque fd
  on RADV, which is not valid for a foreign dma-buf.

`build.sh` prepares the tree afresh (checked out clean, GE's patches applied as
protonprep does, then the view tree of `../wine-view-tree` and `apply.py`) and
builds `wineserver` and `win32u.so` in a Debian 13 container (GE builds with its
GCC 14.2). It installs both into `GE-Proton11-14-viewtree`, keeping what they
replace as `*.orig`. The new `win32u.so` exports and loads the same
libraries as the one it replaces. It supersedes `../wine-view-tree/build.sh`.

The other pieces: `../wine-unixlib` (the export: seal, `UDMABUF_CREATE`,
handle) and `../vkd3d-proton` (`OpenSharedHandle(IID_ID3D12Heap)`).

Needs `/dev/udmabuf` with access for the user (the `kvm` group, or the
`uaccess` ACL most desktops give it) and `CONFIG_UDMABUF`.
