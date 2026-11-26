/* victory_unix.so: the Unix side of victory_unix.dll.
 *
 * Write tracking of our own memory in the kernel: userfaultfd write-protect
 * in async mode (UFFD_FEATURE_WP_ASYNC) and PAGEMAP_SCAN, which reports the
 * pages written since the last scan and protects them again in one step (it
 * was added to Linux for GetWriteWatch).
 *
 * Title memory for the GPU with no copy: a section's memfd, sealed against
 * shrinking, made a dma-buf by udmabuf and handed back as a handle that
 * Wine's vkAllocateMemory imports (tools/wine-dmabuf).
 *
 * No libc: raw system calls only, so it loads in any runtime container. Two
 * calls are ntdll.so's (the handles' fds). Calls take a pointer to their
 * arguments. */
typedef unsigned long u64;
typedef long NTSTATUS;
#define STATUS_SUCCESS 0
#define STATUS_UNSUCCESSFUL ((NTSTATUS)0xC0000001L)

static long sys3(long n, long a, long b, long c) {
    long r;
    __asm__ volatile("syscall" : "=a"(r) : "a"(n), "D"(a), "S"(b), "d"(c) : "rcx", "r11", "memory");
    return r;
}
#define SYS_open 2
#define SYS_close 3
#define SYS_ioctl 16
#define SYS_fcntl 72
#define SYS_read 0
#define SYS_getdents64 217
#define SYS_userfaultfd 323

struct uffdio_api { u64 api, features, ioctls; };
struct uffdio_range { u64 start, len; };
struct uffdio_register { struct uffdio_range range; u64 mode, ioctls; };
struct pm_scan_arg { u64 size, flags, start, end, walk_end, vec, vec_len, max_pages,
                     category_inverted, category_mask, category_anyof_mask, return_mask; };
#define UFFDIO_API 0xc018aa3fUL
#define UFFDIO_REGISTER 0xc020aa00UL
#define UFFDIO_UNREGISTER 0x8010aa01UL
#define UFFD_FEATURE_WP_HUGETLBFS_SHMEM (1 << 12)
#define UFFD_FEATURE_WP_UNPOPULATED (1 << 13)
#define UFFD_FEATURE_WP_ASYNC (1 << 15)
#define UFFDIO_REGISTER_MODE_WP 2
#define PAGEMAP_SCAN 0xc0606610UL
#define PM_SCAN_WP_MATCHING 1
#define PM_SCAN_CHECK_WPASYNC 2
#define PAGE_IS_WRITTEN (1 << 1)

static long uffd = -1, pagemap = -1;

/* init: args = { u64 out_features } */
static NTSTATUS init(void *args) {
    u64 *a = args;
    if (uffd < 0) {
        uffd = sys3(SYS_userfaultfd, 1 /* UFFD_USER_MODE_ONLY */ | 02000000 /* O_CLOEXEC */, 0, 0);
        if (uffd < 0) { a[0] = (u64)uffd; return STATUS_UNSUCCESSFUL; }
        struct uffdio_api api = {0xAA, UFFD_FEATURE_WP_ASYNC | UFFD_FEATURE_WP_UNPOPULATED | UFFD_FEATURE_WP_HUGETLBFS_SHMEM, 0};
        long r = sys3(SYS_ioctl, uffd, UFFDIO_API, (long)&api);
        if (r < 0) { a[0] = (u64)r; return STATUS_UNSUCCESSFUL; }
        pagemap = sys3(SYS_open, (long)"/proc/self/pagemap", 0 /* O_RDONLY */, 0);
        if (pagemap < 0) { a[0] = (u64)pagemap; return STATUS_UNSUCCESSFUL; }
        a[0] = api.features;
    }
    return STATUS_SUCCESS;
}

/* track: args = { u64 start, u64 len, i64 out_result } — register for
 * write-protect and protect it (a first scan that resets). */
static NTSTATUS track(void *args) {
    u64 *a = args;
    struct uffdio_register reg = {{a[0], a[1]}, UFFDIO_REGISTER_MODE_WP, 0};
    long r = sys3(SYS_ioctl, uffd, UFFDIO_REGISTER, (long)&reg);
    if (r < 0) { a[2] = (u64)r; return STATUS_UNSUCCESSFUL; }
    struct pm_scan_arg s = {sizeof s, PM_SCAN_WP_MATCHING | PM_SCAN_CHECK_WPASYNC, a[0], a[0] + a[1], 0, 0, 0, 0,
                            0, PAGE_IS_WRITTEN, 0, PAGE_IS_WRITTEN};
    r = sys3(SYS_ioctl, pagemap, PAGEMAP_SCAN, (long)&s);
    a[2] = (u64)r;
    return r < 0 ? STATUS_UNSUCCESSFUL : STATUS_SUCCESS;
}

/* untrack: args = { u64 start, u64 len, i64 out_result } */
static NTSTATUS untrack(void *args) {
    u64 *a = args;
    struct uffdio_range range = {a[0], a[1]};
    long r = sys3(SYS_ioctl, uffd, UFFDIO_UNREGISTER, (long)&range);
    a[2] = (u64)r;
    return r < 0 ? STATUS_UNSUCCESSFUL : STATUS_SUCCESS;
}

/* scan: args = { u64 start, u64 end, u64 vec, u64 vec_len, u64 out_regions,
 * u64 out_walk_end } — the written runs [start, end) of pages since the last
 * scan into vec (struct { u64 start, end, categories; }), protecting them
 * again. walk_end < end: vec was full; scan again from walk_end. */
static NTSTATUS scan(void *args) {
    u64 *a = args;
    struct pm_scan_arg s = {sizeof s, PM_SCAN_WP_MATCHING | PM_SCAN_CHECK_WPASYNC, a[0], a[1], 0, a[2], a[3], 0,
                            0, PAGE_IS_WRITTEN, 0, PAGE_IS_WRITTEN};
    long r = sys3(SYS_ioctl, pagemap, PAGEMAP_SCAN, (long)&s);
    a[4] = (u64)r;
    a[5] = s.walk_end;
    return r < 0 ? STATUS_UNSUCCESSFUL : STATUS_SUCCESS;
}

/* ntdll.so's. */
typedef void *HANDLE;
extern NTSTATUS wine_server_handle_to_fd(HANDLE handle, unsigned int access, int *unix_fd, unsigned int *options);
extern NTSTATUS wine_server_fd_to_handle(int fd, unsigned int access, unsigned int attributes, HANDLE *handle);

struct udmabuf_create { unsigned int memfd, flags; u64 offset, size; };
#define UDMABUF_CREATE 0x40187542UL /* _IOW('u', 0x42, struct udmabuf_create) */
#define UDMABUF_FLAGS_CLOEXEC 1
#define F_ADD_SEALS 1033
#define F_SEAL_SHRINK 2
#define GENERIC_ALL 0x10000000

/* dmabuf: args = { u64 section, u64 offset, u64 len, u64 out_handle,
 * i64 out_error, u64 out_step } - a handle to a dma-buf of [offset,
 * offset + len) of the section's memory (page-aligned), for
 * VkImportMemoryWin32HandleInfoKHR. On failure out_step says which step
 * failed and out_error how: 1 the section's fd, 2 the seal (a memfd made
 * without MFD_ALLOW_SEALING: wineserver unpatched), 3 /dev/udmabuf, 4
 * UDMABUF_CREATE, 5 the handle. The seal stays on the memfd, which can
 * then grow and never shrink, as Wine's sections do. */
static NTSTATUS dmabuf(void *args) {
    u64 *a = args;
    int memfd = -1;
    long r, dev, buf;
    HANDLE handle = 0;
    a[3] = 0;
    if ((r = wine_server_handle_to_fd((HANDLE)a[0], 0, &memfd, 0))) { a[4] = (u64)r; a[5] = 1; return STATUS_UNSUCCESSFUL; }
    if ((r = sys3(SYS_fcntl, memfd, F_ADD_SEALS, F_SEAL_SHRINK)) < 0) { a[4] = (u64)r; a[5] = 2; goto failed; }
    if ((dev = sys3(SYS_open, (long)"/dev/udmabuf", 02 /* O_RDWR */ | 02000000 /* O_CLOEXEC */, 0)) < 0) { r = dev; a[4] = (u64)r; a[5] = 3; goto failed; }
    struct udmabuf_create create = {(unsigned int)memfd, UDMABUF_FLAGS_CLOEXEC, a[1], a[2]};
    buf = sys3(SYS_ioctl, dev, UDMABUF_CREATE, (long)&create);
    sys3(SYS_close, dev, 0, 0);
    if (buf < 0) { r = buf; a[4] = (u64)r; a[5] = 4; goto failed; }
    /* The server keeps its own copy of the fd. */
    r = wine_server_fd_to_handle((int)buf, GENERIC_ALL, 0, &handle);
    sys3(SYS_close, buf, 0, 0);
    if (r) { a[4] = (u64)r; a[5] = 5; goto failed; }
    sys3(SYS_close, memfd, 0, 0);
    a[3] = (u64)handle;
    a[4] = 0;
    a[5] = 0;
    return STATUS_SUCCESS;
failed:
    sys3(SYS_close, memfd, 0, 0);
    return STATUS_UNSUCCESSFUL;
}

/* The value after `key` on one of the lines of `text` (n bytes), or -1. */
static long field(const char *text, long n, const char *key) {
    long k = 0;
    while (key[k]) k++;
    for (long i = 0; i + k <= n; i++) {
        if (i && text[i - 1] != '\n') continue;
        long j = 0;
        while (j < k && text[i + j] == key[j]) j++;
        if (j < k) continue;
        long at = i + k, v = 0, digits = 0;
        while (at < n && (text[at] == ' ' || text[at] == '\t')) at++;
        while (at < n && text[at] >= '0' && text[at] <= '9') { v = v * 10 + (text[at++] - '0'); digits++; }
        return digits ? v : -1;
    }
    return -1;
}

static int has(const char *text, long n, const char *needle) {
    long k = 0;
    while (needle[k]) k++;
    for (long i = 0; i + k <= n; i++) {
        long j = 0;
        while (j < k && text[i + j] == needle[j]) j++;
        if (j == k) return 1;
    }
    return 0;
}

/* gtt: args = { u64 out_kib, u64 out_clients } - the PC's memory this
 * process's amdgpu clients hold (drm-total-gtt in /proc/self/fdinfo, each
 * client once), pinned dma-buf imports among it. */
static NTSTATUS gtt(void *args) {
    u64 *a = args;
    static char dents[8192], text[4096];
    long seen[64], nseen = 0, total = 0;
    long dir = sys3(SYS_open, (long)"/proc/self/fdinfo", 0200000 /* O_DIRECTORY */ | 02000000, 0);
    a[0] = a[1] = 0;
    if (dir < 0) return STATUS_UNSUCCESSFUL;
    for (;;) {
        long got = sys3(SYS_getdents64, dir, (long)dents, sizeof dents);
        if (got <= 0) break;
        for (long off = 0; off < got;) {
            unsigned short reclen = *(unsigned short *)(dents + off + 16);
            const char *name = dents + off + 19;
            off += reclen;
            if (name[0] < '0' || name[0] > '9') continue;
            char path[48] = "/proc/self/fdinfo/";
            long at = 18;
            for (long i = 0; name[i] && at < 46; i++) path[at++] = name[i];
            path[at] = 0;
            long fd = sys3(SYS_open, (long)path, 02000000, 0);
            if (fd < 0) continue;
            long n = sys3(SYS_read, fd, (long)text, sizeof text);
            sys3(SYS_close, fd, 0, 0);
            if (n <= 0 || !has(text, n, "drm-driver:\tamdgpu")) continue;
            long client = field(text, n, "drm-client-id:"), kib = field(text, n, "drm-total-gtt:");
            if (client < 0 || kib < 0) continue;
            int dup = 0;
            for (long i = 0; i < nseen; i++) dup |= seen[i] == client;
            if (dup || nseen == 64) continue;
            seen[nseen++] = client;
            total += kib;
        }
    }
    sys3(SYS_close, dir, 0, 0);
    a[0] = (u64)total;
    a[1] = (u64)nseen;
    return nseen ? STATUS_SUCCESS : STATUS_UNSUCCESSFUL;
}

typedef NTSTATUS (*unixlib_entry_t)(void *);
__attribute__((visibility("default"))) const unixlib_entry_t __wine_unix_call_funcs[] = {init, track, untrack, scan, dmabuf, gtt};
