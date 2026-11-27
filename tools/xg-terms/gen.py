# Writes texture_xor.rs (per-bit address terms of the XOR modes) and texture_xor_points.txt (reference points)
# from xgterms output: gen.py anaconda.txt lockhart.txt outdir
import sys, collections
SMALL = {21, 22, 23}
def parse(path):
    terms = collections.defaultdict(dict); bad = set(); cases = []
    for line in open(path):
        f = line.split()
        if f[0] == 'term':
            mode, dim, bpe, axis, bit, off = map(int, f[1:])
            if off >= 1 << 63: bad.add((mode, dim, bpe)); continue
            blk = 4096 if mode in SMALL else 65536
            terms[(mode, dim, bpe)][(axis, bit)] = off & (blk - 1)
        elif f[0] == 'fail':
            bad.add(tuple(map(int, f[1:4])))
        elif f[0] == 'case':
            cases.append([tuple(map(int, f[1:])), []])
        elif f[0] == 'nocase':
            pass
        else:
            cases[-1][1].append(line.strip())
    return terms, bad, cases
out = sys.argv[3]
rows = []
fixture = []
for cfg, path, step in ((0, sys.argv[1], 4), (1, sys.argv[2], 8)):
    terms, bad, cases = parse(path)
    for (mode, dim, bpe), t in sorted(terms.items()):
        if (mode, dim, bpe) in bad: continue
        axes = []
        for axis, n in ((0, 14), (1, 14), (2, 11)):
            v = [t.get((axis, b), 0) for b in range(n)]
            axes.append(v)
        rows.append((cfg, mode, dim == 4, bpe.bit_length() - 1, axes))
    for key, pts in cases:
        fixture.append('case %d %s' % (cfg, ' '.join(map(str, key))))
        fixture.extend(pts[::step])
with open(out + '/texture_xor.rs', 'w') as f:
    f.write('''//! Address terms of the console GPU's pipe and bank XOR modes, read out of the
//! console's own texture library (`xg_xs.dll`, `GetTexelElementOffsetBytes`) by
//! `tools/xg-terms`: for each mode, element size and dimension, the byte offset
//! inside a block that each coordinate bit (x 0..13, y 0..13, z 0..10: the array
//! slice of a 2D texture, the depth of a 3D one) XORs in. Bits below the block
//! size give the block's own pattern; the few above it are the pipe and bank
//! bits. Generated; do not edit.

/// (Series S, mode, 3D, log2 element bytes, x, y, z terms).
pub(crate) type XorTerms = (bool, u8, bool, u8, [u16; 14], [u16; 14], [u16; 11]);

pub(crate) static XOR_TERMS: &[XorTerms] = &[
''')
    for cfg, mode, thick, e, (x, y, z) in rows:
        f.write('    (%s, %d, %s, %d, %s, %s, %s),\n' % ('true' if cfg else 'false', mode, 'true' if thick else 'false', e,
                 '[%s]' % ', '.join('%#x' % v for v in x), '[%s]' % ', '.join('%#x' % v for v in y), '[%s]' % ', '.join('%#x' % v for v in z)))
    f.write('];\n')
with open(out + '/texture_xor_points.txt', 'w') as f:
    f.write('# Reference element offsets from the console texture library, Series X (0) and S (1):\n')
    f.write('# "case <series-s> <dim 3=2D 4=3D> <mode> <bytes> <w> <h> <depth or slices> <mips>", then "x y z mip offset".\n')
    f.write('\n'.join(fixture) + '\n')
print(len(rows), 'entries', len(fixture), 'fixture lines')
