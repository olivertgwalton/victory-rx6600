#!/usr/bin/env bash
# Inside a Debian 13 container (GE-Proton 11-14 is built with its GCC 14.2):
# generates Wine's Vulkan files, configures for the unix side only and builds
# server/wineserver and dlls/win32u/win32u.so in /wine/build-dmabuf.
set -u
apt-get update -qq >/dev/null 2>&1
apt-get install -y -qq --no-install-recommends gcc make flex bison libc6-dev pkg-config perl python3 ca-certificates \
  libvulkan-dev libfreetype-dev libfontconfig-dev libegl-dev libgl-dev libdbus-1-dev >/dev/null 2>&1
cd /wine
# The generated Vulkan files a server-only build left as placeholders.
for f in include/wine/vulkan.h dlls/winevulkan/loader_thunks.c dlls/winevulkan/vulkan_thunks.c \
         dlls/winevulkan/loader_thunks.h dlls/winevulkan/vulkan_thunks.h dlls/winevulkan/winevulkan.spec; do
  grep -qs "placeholder for makedep" "$f" && rm -f "$f"
done
if grep -qs "placeholder for makedep" include/wine/vulkan_driver.h; then echo "include/wine/vulkan_driver.h is a placeholder: prepare the tree again (build.sh)"; exit 1; fi
[ -e include/wine/vulkan.h ] || (cd dlls/winevulkan && python3 make_vulkan > /wine/make_vulkan.log 2>&1) || { echo "make_vulkan failed"; tail /wine/make_vulkan.log; exit 1; }
mkdir -p /wine/build-dmabuf && cd /wine/build-dmabuf
OPTS="--enable-win64 --without-mingw --without-x --without-wayland --without-gstreamer --without-pulse --without-alsa --without-cups \
 --without-gnutls --without-sane --without-usb --without-v4l2 --without-pcap --without-krb5 --without-netapi --without-opencl \
 --without-sdl --without-udev --without-unwind --without-xinerama --without-capi --without-gphoto --without-gssapi --without-inotify \
 --without-ffmpeg --disable-tests"
for i in $(seq 1 25); do
  if ../configure $OPTS > /wine/configure-dmabuf.log 2>&1; then echo "configure ok after $i"; break; fi
  line=$(grep -E "error: .*No such file or directory|error: open .* No such file" /wine/configure-dmabuf.log | head -1)
  echo "missing: $line"
  if [[ $line =~ \.\./([^:]+):[0-9]+:\ error:\ ([^:]+):\ No ]]; then
    target="/wine/$(dirname "${BASH_REMATCH[1]}")/${BASH_REMATCH[2]}"
  elif [[ $line =~ error:\ open\ ([^ ]+)\ : ]]; then
    f=${BASH_REMATCH[1]}
    ref=$(grep -rl --include=Makefile.in "$(basename "$f")" /wine/dlls /wine/programs | head -1); target="$(dirname "$ref")/$(basename "$f")"
    [[ $f == */* ]] && target="/wine/include/$f"
  else echo "unparsed"; tail -5 /wine/configure-dmabuf.log; exit 1; fi
  mkdir -p "$(dirname "$target")"; [ -e "$target" ] || { echo "/* placeholder for makedep (unix-side build) */" > "$target"; echo "placeholder $target"; }
done
grep -E "^(SONAME_LIBVULKAN|SONAME_LIBFREETYPE|SONAME_LIBFONTCONFIG|SONAME_LIBEGL)" include/config.h 2>/dev/null || grep -E "define SONAME_LIB(VULKAN|FREETYPE|FONTCONFIG|EGL) " include/config.h
make -j"$(nproc)" server/wineserver dlls/win32u/win32u.so > /wine/make-dmabuf.log 2>&1; echo "make=$?"
ls -la server/wineserver dlls/win32u/win32u.so 2>&1
