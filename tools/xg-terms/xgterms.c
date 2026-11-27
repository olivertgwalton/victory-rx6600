// xgterms [4]: from the console's xg_xs.dll (after XGSetHardwareVersion(4) when given), prints
//   term <mode> <dim> <bpe> <axis> <bit> <offset>     offset of the element at coordinate 1<<bit on one axis
//   case <dim> <mode> <bpe> <w> <h> <d> <mips>        then "<x> <y> <z> <mip> <offset>" lines: pseudo-random points
// dim 3 = 2D (z is the array slice), 4 = 3D.
#include <windows.h>
#include <stdio.h>
#include <stdlib.h>
#include <stdint.h>
typedef struct { uint32_t dim; uint64_t align, width; uint32_t height; uint16_t depth, mips; uint32_t format, samples, quality, layout, flags; } Desc;
typedef HRESULT (WINAPI *Create)(const Desc*, void***);
typedef uint64_t (WINAPI *Offset)(void*, uint32_t, uint32_t, uint64_t, uint32_t, uint32_t, uint32_t, uint32_t*);
typedef uint32_t (WINAPI *Rel)(void*);
typedef HRESULT (WINAPI *Hw)(uint32_t);
static Create create;
static uint32_t rng = 12345; static uint32_t rnd(void){ rng = rng * 1103515245u + 12345u; return rng >> 4; }
static int fmt_of(int bpe){ switch(bpe){case 1:return 61;case 2:return 54;case 4:return 41;case 8:return 16;default:return 2;} }
static void **make(int dim, int mode, int bpe, int w, int h, int d, int mips) {
  Desc ds = {(uint32_t)dim, 0, (uint64_t)w, (uint32_t)h, (uint16_t)d, (uint16_t)mips, (uint32_t)fmt_of(bpe), 1, 0, 0x100u | (uint32_t)mode, 1u /* render target: a depth flag on a single-mip Z_X texture gives other answers */};
  void **c = 0; return create(&ds, &c) < 0 ? 0 : c;
}
static uint64_t at(void **c, int mip, uint32_t x, uint32_t y, uint32_t z) { uint32_t b; return ((Offset)(*(void***)c)[6])(c, 0, mip, x, y, z, 0, &b); }
static void done(void **c) { ((Rel)(*(void***)c)[1])(c); }
static void terms(int mode, int dim, int bpe) {
  int big = dim == 3 ? 16384 : 2048, zbits = 11;
  for (int axis = 0; axis < 3; axis++) {
    int w = axis == 0 ? big : 64, h = axis == 1 ? big : 64, d = axis == 2 ? 2048 : 1;
    void **c = make(dim, mode, bpe, w, h, d, 1);
    if (!c) { printf("fail %d %d %d %d\n", mode, dim, bpe, axis); continue; }
    int n = axis == 2 ? zbits : (dim == 3 ? 14 : 11);
    for (int bit = 0; bit < n; bit++)
      printf("term %d %d %d %d %d %llu\n", mode, dim, bpe, axis, bit,
             (unsigned long long)at(c, 0, axis == 0 ? 1u << bit : 0, axis == 1 ? 1u << bit : 0, axis == 2 ? 1u << bit : 0));
    done(c);
  }
}
static void points(int dim, int mode, int bpe, int w, int h, int d, int mips, int n) {
  void **c = make(dim, mode, bpe, w, h, d, mips);
  if (!c) { printf("nocase %d %d %d %d %d %d %d\n", dim, mode, bpe, w, h, d, mips); return; }
  printf("case %d %d %d %d %d %d %d\n", dim, mode, bpe, w, h, d, mips);
  for (int m = 0; m < mips; m++) {
    uint32_t mw = w >> m ? w >> m : 1, mh = h >> m ? h >> m : 1, md = dim == 4 ? (d >> m ? d >> m : 1) : d;
    int k = m == 0 ? n : n / 8 + 8;
    for (int i = 0; i < k; i++) {
      uint32_t x = rnd() % mw, y = rnd() % mh, z = rnd() % md;
      printf("%u %u %u %d %llu\n", x, y, z, m, (unsigned long long)at(c, m, x, y, z));
    }
  }
  done(c);
}
int main(int argc, char **argv) {
  HMODULE xg = LoadLibraryA("xg_xs.dll");
  if (!xg) { printf("load failed %lu\n", GetLastError()); return 1; }
  create = (Create)GetProcAddress(xg, "XGCreateTextureComputer");
  if (argc > 1) ((Hw)GetProcAddress(xg, "XGSetHardwareVersion"))((uint32_t)atoi(argv[1]));
  int modes[] = {17, 18, 21, 22, 24, 25, 26, 27};
  for (int i = 0; i < 8; i++) for (int dim = 3; dim <= 4; dim++) for (int b = 1; b <= 16; b <<= 1) terms(modes[i], dim, b);
  if (argc > 2) {
    points(3, 27, 4, 256, 256, 1, 9, 600);     // the ocean height map's shape, with mips
    points(3, 27, 4, 3000, 1100, 1, 1, 600);
    points(3, 27, 4, 1920, 1080, 1, 11, 600);
    points(3, 27, 8, 1024, 1024, 6, 11, 600);
    points(3, 27, 1, 777, 333, 3, 10, 600);
    points(3, 27, 16, 300, 200, 1, 9, 600);
    points(3, 27, 2, 513, 257, 1, 10, 600);
    points(3, 24, 4, 1920, 1080, 1, 11, 600);
    points(3, 24, 2, 1024, 768, 2, 11, 600);
    points(3, 25, 16, 512, 512, 4, 10, 600);
    points(3, 26, 4, 640, 360, 1, 10, 600);
    points(3, 21, 4, 100, 60, 1, 7, 600);
    points(3, 22, 8, 256, 256, 1, 9, 600);
    points(3, 21, 1, 128, 4, 3, 1, 300);
    points(4, 25, 4, 128, 128, 64, 8, 600);
    points(4, 21, 8, 64, 64, 64, 7, 600);
    points(4, 26, 1, 256, 128, 64, 9, 600);
    points(4, 27, 4, 256, 256, 32, 9, 600);
    points(3, 17, 4, 1000, 700, 3, 10, 600);
    points(3, 18, 16, 640, 480, 1, 10, 600);
    points(4, 17, 2, 128, 64, 32, 8, 600);
  }
  return 0;
}
