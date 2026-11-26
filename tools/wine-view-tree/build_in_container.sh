#!/usr/bin/env bash
# Inside the Debian 13 container: configure Wine (server-only use), creating a
# placeholder for each generated file makedep reports missing, then build
# server/wineserver.
set -u
apt-get update -qq >/dev/null 2>&1 && apt-get install -y -qq --no-install-recommends gcc make flex bison libc6-dev pkg-config perl >/dev/null 2>&1
cd /wine/build-server
OPTS="--enable-win64 --without-x --without-freetype --without-mingw --without-wayland --without-vulkan --without-gstreamer --without-pulse --without-alsa --without-cups --without-dbus --without-gnutls --without-sane --without-usb --without-v4l2 --without-pcap --without-krb5 --without-netapi --without-opencl --without-osmesa --without-sdl --without-udev --without-unwind --without-xinerama --without-fontconfig --without-capi --without-gphoto --without-gssapi --without-inotify --without-ffmpeg --disable-tests"
for i in $(seq 1 25); do
  if ../configure $OPTS > /wine/configure.log 2>&1; then echo "configure ok after $i"; break; fi
  line=$(grep -E "error: .*No such file or directory|error: open .* No such file" /wine/configure.log | head -1)
  echo "missing: $line"
  # forms: "../dir/file.rc:29: error: name: No such file" or "error: open dir/name : No such file"
  if [[ $line =~ \.\./([^:]+):[0-9]+:\ error:\ ([^:]+):\ No ]]; then
    target="/wine/$(dirname "${BASH_REMATCH[1]}")/${BASH_REMATCH[2]}"
  elif [[ $line =~ error:\ open\ ([^ ]+)\ : ]]; then
    f=${BASH_REMATCH[1]}; target=$(find /wine/dlls /wine/include /wine/programs -path "*/$f" -print -quit 2>/dev/null); [ -n "$target" ] && { echo "exists?! $target"; break; }
    ref=$(grep -rl --include=Makefile.in "$(basename "$f")" /wine/dlls /wine/programs | head -1); target="$(dirname "$ref")/$(basename "$f")"
    [[ $f == */* ]] && target="/wine/include/$f"
  else echo "unparsed"; break; fi
  mkdir -p "$(dirname "$target")"; [ -e "$target" ] || { echo "/* placeholder for makedep (server-only build) */" > "$target"; echo "placeholder $target"; }
done
make -j"$(nproc)" server/wineserver > /wine/make-server.log 2>&1; echo "make=$?"
ls -la server/wineserver 2>&1
