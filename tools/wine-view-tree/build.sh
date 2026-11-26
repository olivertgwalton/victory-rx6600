#!/usr/bin/env bash
# Builds GE-Proton 11-14's wineserver with mapped views kept in a red-black tree
# (see README.md) and installs it into a copy of that GE-Proton.
#   build.sh [GE source dir, default ~/src/ge/proton-ge-custom]
set -euo pipefail
here=$(cd "$(dirname "$0")" && pwd)
GE=${1:-$HOME/src/ge/proton-ge-custom}
C=$HOME/.steam/root/compatibilitytools.d
[ -d "$GE" ] || git clone -q --depth 1 --branch GE-Proton11-14 https://github.com/GloriousEggroll/proton-ge-custom "$GE"
cd "$GE"
git submodule update --init --depth 1 wine wine-staging
S=patches/protonprep-valve-staging.sh
start=$(grep -n "^### (2) WINE PATCHING ###" $S | cut -d: -f1)
end=$(awk -v s="$start" 'NR>s && /^\s*popd/{print NR; exit}' $S)
{ sed -n 1,14p $S; sed -n "${start},${end}p" $S; } > /tmp/wineprep.sh
podman run --rm --userns=keep-id:uid=0,gid=0 -v "$GE":/ge:Z -v /tmp/wineprep.sh:/wineprep.sh:Z -w /ge docker.io/library/debian:trixie \
  bash -c 'apt-get update -qq >/dev/null && apt-get install -y -qq --no-install-recommends patch python3 git autoconf automake perl ca-certificates >/dev/null; git config --global --add safe.directory "*"; bash /wineprep.sh > /ge/wineprep.log 2>&1'
(cd wine && python3 "$here/apply_view_tree.py" && python3 "$here/apply_owner_init.py")
podman run --rm --userns=keep-id:uid=0,gid=0 -v "$GE/wine":/wine:Z -v "$here/build_in_container.sh":/ws_build.sh:Z docker.io/library/debian:trixie \
  bash -c 'perl /wine/tools/make_specfiles >/dev/null; mkdir -p /wine/build-server; for h in include/wine/vulkan.h include/wine/vulkan_driver.h dlls/winevulkan/loader_thunks.c dlls/winevulkan/vulkan_thunks.c dlls/winevulkan/loader_thunks.h dlls/winevulkan/vulkan_thunks.h dlls/winevulkan/winevulkan.spec; do [ -e /wine/$h ] || echo "/* placeholder for makedep (server-only build) */" > /wine/$h; done; bash /ws_build.sh'
[ -d "$C/GE-Proton11-14-viewtree" ] || cp -a "$C/GE-Proton11-14-x86_64" "$C/GE-Proton11-14-viewtree"
install -m755 wine/build-server/server/wineserver "$C/GE-Proton11-14-viewtree/files/bin/wineserver"
strip "$C/GE-Proton11-14-viewtree/files/bin/wineserver" || true
echo "installed: $C/GE-Proton11-14-viewtree (use it with PROTON_DIR=...)"
