#!/usr/bin/env python3
"""Patches a Wine tree (GE-Proton 11-14) so title memory can reach the GPU
as a dma-buf, with no copy:

  server/mapping.c   section memfds are made with MFD_ALLOW_SEALING, so a
                     view's memfd can take F_SEAL_SHRINK, which udmabuf needs;
  dlls/win32u/vulkan.c
                     vkAllocateMemory with VkImportMemoryWin32HandleInfoKHR
                     (OPAQUE_WIN32) of a handle that is no D3DKMT resource but
                     wraps a dma-buf fd imports it as
                     VK_EXTERNAL_MEMORY_HANDLE_TYPE_DMA_BUF_BIT_EXT, after
                     vkGetMemoryFdPropertiesKHR accepts the memory type, and
                     fails with VK_ERROR_INVALID_EXTERNAL_HANDLE otherwise;
                     a buffer for Win32-handle memory also declares
                     DMA_BUF_BIT_EXT, so it may be bound to such an import,
                     and one for host allocations keeps that bit (it was
                     dropped, and the buffer declared no type at all).

Run in the Wine source directory. Applying twice changes nothing.
"""
import sys

def patch(path, old, new, marker, times=1):
    s = open(path).read()
    if marker in s:
        print(f"{path}: already patched")
        return
    if s.count(old) != times:
        sys.exit(f"{path}: expected text found {s.count(old)} times, not {times}")
    open(path, "w").write(s.replace(old, new))
    print(f"{path}: patched")

patch("server/mapping.c",
      'memfd_create( "wine-mapping", MFD_EXEC )',
      'memfd_create( "wine-mapping", MFD_EXEC | MFD_ALLOW_SEALING )',
      "MFD_ALLOW_SEALING")

patch("dlls/win32u/vulkan.c",
      "#include <linux/dma-buf.h>\n",
      "#include <linux/dma-buf.h>\n#include <sys/vfs.h>\n",
      "#include <sys/vfs.h>")

patch("dlls/win32u/vulkan.c",
      "static VkExternalMemoryHandleTypeFlagBits get_host_external_memory_type(void)\n",
      """/* the fd of a handle that wraps a dma-buf (wine_server_fd_to_handle), or -1 */
static int dma_buf_from_handle( HANDLE handle )
{
    struct statfs fs;
    int fd;

    if (!handle || wine_server_handle_to_fd( handle, GENERIC_ALL, &fd, NULL )) return -1;
    if (fstatfs( fd, &fs ) || fs.f_type != 0x444d4142 /* DMA_BUF_MAGIC */)
    {
        close( fd );
        return -1;
    }
    return fd;
}

static VkExternalMemoryHandleTypeFlagBits get_host_external_memory_type(void)
""",
      "dma_buf_from_handle")

patch("dlls/win32u/vulkan.c",
      """        if ((fd_info.fd = d3dkmt_object_get_fd( memory->local )) < 0)
        {
            res = VK_ERROR_INVALID_EXTERNAL_HANDLE;
            goto failed;
        }

        fd_info.handleType = get_host_external_memory_type();
""",
      """        if (!memory->local && import_win32->handleType == VK_EXTERNAL_MEMORY_HANDLE_TYPE_OPAQUE_WIN32_BIT &&
            (fd_info.fd = dma_buf_from_handle( import_win32->handle )) >= 0)
        {
            /* Memory of the process's own, shared with the GPU as a dma-buf
             * (a udmabuf of a section's memfd): imported as one, or refused. */
            VkMemoryFdPropertiesKHR properties = {.sType = VK_STRUCTURE_TYPE_MEMORY_FD_PROPERTIES_KHR};

            if (!device->extensions.has_VK_EXT_external_memory_dma_buf ||
                device->p_vkGetMemoryFdPropertiesKHR( device->host.device, VK_EXTERNAL_MEMORY_HANDLE_TYPE_DMA_BUF_BIT_EXT,
                                                      fd_info.fd, &properties ) ||
                !(properties.memoryTypeBits & (1u << alloc_info->memoryTypeIndex)))
            {
                WARN( "dma-buf import refused, memory type %u, allowed %#x\\n", alloc_info->memoryTypeIndex,
                      properties.memoryTypeBits );
                close( fd_info.fd );
                free( memory );
                return VK_ERROR_INVALID_EXTERNAL_HANDLE;
            }
            fd_info.handleType = VK_EXTERNAL_MEMORY_HANDLE_TYPE_DMA_BUF_BIT_EXT;
        }
        else if ((fd_info.fd = d3dkmt_object_get_fd( memory->local )) < 0)
        {
            res = VK_ERROR_INVALID_EXTERNAL_HANDLE;
            goto failed;
        }
        else fd_info.handleType = get_host_external_memory_type();
""",
      "dma-buf import refused")

patch("dlls/win32u/vulkan.c",
      """        case VK_STRUCTURE_TYPE_DEDICATED_ALLOCATION_BUFFER_CREATE_INFO_NV: break;
        case VK_STRUCTURE_TYPE_EXTERNAL_MEMORY_BUFFER_CREATE_INFO:
            external_info = (VkExternalMemoryBufferCreateInfo *)*next;
            external_info->handleTypes = map_external_memory_handle_types( external_info->handleTypes );
            break;
""",
      """        case VK_STRUCTURE_TYPE_DEDICATED_ALLOCATION_BUFFER_CREATE_INFO_NV: break;
        case VK_STRUCTURE_TYPE_EXTERNAL_MEMORY_BUFFER_CREATE_INFO:
            external_info = (VkExternalMemoryBufferCreateInfo *)*next;
            {
                /* Win32 handles are imported as the host's type or, wrapping a
                 * dma-buf, as one (vkAllocateMemory): a buffer for such memory
                 * may be bound to either, and RADV takes both together. Host
                 * allocations (VK_EXT_external_memory_host) keep their bits. */
                const UINT host_bits = VK_EXTERNAL_MEMORY_HANDLE_TYPE_HOST_ALLOCATION_BIT_EXT |
                                       VK_EXTERNAL_MEMORY_HANDLE_TYPE_HOST_MAPPED_FOREIGN_MEMORY_BIT_EXT;
                UINT types = external_info->handleTypes;

                external_info->handleTypes = map_external_memory_handle_types( types & ~host_bits ) | (types & host_bits);
                if ((types & EXTERNAL_MEMORY_WIN32_BITS) && device->extensions.has_VK_EXT_external_memory_dma_buf)
                    external_info->handleTypes |= VK_EXTERNAL_MEMORY_HANDLE_TYPE_DMA_BUF_BIT_EXT;
            }
            break;
""",
      "may be bound to either",
      times=2)  # vkCreateBuffer and vkGetDeviceBufferMemoryRequirements
