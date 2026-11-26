#!/usr/bin/env bash
# Builds GE-Proton 11-14's wineserver and win32u.so for VIctory and installs
# both into a copy of that GE-Proton, keeping the files replaced as *.orig:
#   1. GE's Wine tree is prepared afresh (checked out clean, GE's patches
#      applied as protonprep does);
#   2. the view tree (../wine-view-tree) and apply.py's dma-buf changes;
#   3. a Debian 13 container builds both (build_in_container.sh).
#   build.sh [GE source dir, default ~/src/ge/proton-ge-custom] [Proton dir]
set -euo pipefail
here=$(cd "$(dirname "$0")" && pwd)
GE=${1:-$HOME/src/ge/proton-ge-custom}
P=${2:-$HOME/.steam/root/compatibilitytools.d/GE-Proton11-14-viewtree}
C=$HOME/.steam/root/compatibilitytools.d
[ -d "$GE" ] || git clone -q --depth 1 --branch GE-Proton11-14 https://github.com/GloriousEggroll/proton-ge-custom "$GE"
cd "$GE"
git submodule update --init --depth 1 wine wine-staging
git -C wine checkout -qf -- . && git -C wine clean -qfdx -e build-server -e build-dmabuf
git -C wine-staging checkout -qf -- . && git -C wine-staging clean -qfdx
S=patches/protonprep-valve-staging.sh
start=$(grep -n "^### (2) WINE PATCHING ###" $S | cut -d: -f1)
end=$(awk -v s="$start" 'NR>s && /^\s*popd/{print NR; exit}' $S)
{ sed -n 1,14p $S; sed -n "${start},${end}p" $S; } > /tmp/wineprep.sh
podman run --rm --userns=keep-id:uid=0,gid=0 -v "$GE":/ge:Z -v /tmp/wineprep.sh:/wineprep.sh:Z -w /ge docker.io/library/debian:trixie \
  bash -c 'apt-get update -qq >/dev/null && apt-get install -y -qq --no-install-recommends patch python3 git autoconf automake perl ca-certificates >/dev/null; git config --global --add safe.directory "*"; bash /wineprep.sh > /ge/wineprep.log 2>&1'
(cd wine && python3 "$here/../wine-view-tree/apply_view_tree.py" && python3 "$here/../wine-view-tree/apply_owner_init.py" && python3 "$here/apply.py")
podman run --rm --userns=keep-id:uid=0,gid=0 -v "$GE/wine":/wine:Z -v "$here/build_in_container.sh":/build.sh:Z \
  docker.io/library/debian:trixie bash -c 'cd /wine && perl tools/make_specfiles && bash /build.sh'
B=$GE/wine/build-dmabuf
[ -d "$P" ] || cp -a "$C/GE-Proton11-14-x86_64" "$P"
for f in bin/wineserver:server/wineserver lib/wine/x86_64-unix/win32u.so:dlls/win32u/win32u.so; do
  dst=$P/files/${f%%:*}; src=$B/${f##*:}
  [ -e "$dst.orig" ] || cp -a "$dst" "$dst.orig"
  install -m755 "$src" "$dst"; strip "$dst" || true
done
echo "installed into $P"
