#include <windows.h>
__declspec(dllexport) void* XMemAlloc(SIZE_T n, ULONGLONG a) { (void)a; return HeapAlloc(GetProcessHeap(), HEAP_ZERO_MEMORY, n); }
__declspec(dllexport) void XMemFree(void* p, ULONGLONG a) { (void)a; HeapFree(GetProcessHeap(), 0, p); }
