/* victory_unix.dll: a Wine builtin whose only work is to carry
 * victory_unix.so; callers find it with NtQueryVirtualMemory
 * (MemoryWineUnixFuncs) and call through __wine_unix_call_dispatcher. */
#include <windows.h>
BOOL WINAPI DllMain(HINSTANCE h, DWORD r, LPVOID p) { (void)h; (void)r; (void)p; return TRUE; }
__declspec(dllexport) int victory_unix_version(void) { return 1; }
