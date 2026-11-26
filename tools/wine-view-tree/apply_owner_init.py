import re
p = "server/mapping.c"
s = open(p).read()
pat = re.compile(r"(    if \(\(view = mem_alloc\( (?:sizeof\(\*view\)|sizeof\(struct memory_view\) \+ namelen) \)\)\)\n    \{\n)")
s, n = pat.subn(r"\1        view->owner     = NULL;\n", s)
assert n == 3, n
open(p, "w").write(s)
print("owner initialised at", n, "allocations")
