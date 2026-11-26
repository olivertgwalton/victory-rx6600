# vkd3d-proton patches

Applied by `~/victory/build-vkd3d.sh` on top of GE-Proton 11-14's
vkd3d-proton (upstream `cae8353` plus GE's patches), each once, by subject.

- `0001-vkd3d-Open-shared-heaps-from-dma-buf-handles.patch`:
  `OpenSharedHandle(IID_ID3D12Heap)` of a handle whose file size is known
  imports it as `OPAQUE_WIN32` memory, a buffers-only, CPU-visible
  write-back heap. With `../wine-dmabuf`, a handle from
  `crate::dmabuf::export` is the process's own section pages, with no copy.
  Failure is returned, not covered with an allocation of its own (which
  `OpenExistingHeapFromFileMapping` does under Wine: the probe's "section in
  place" reads false). Known gap: the heap's global buffer is created
  without `VkExternalMemoryBufferCreateInfo`, which RADV does not check but
  the validation layer would flag.
