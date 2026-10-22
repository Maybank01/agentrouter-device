#!/bin/sh
# Builds socat as one static program against musl, for Linux folder mode (#119): srt relays a sandbox's network
# through socat, and no maintained upstream static build exists. Runs inside the pinned Alpine image of
# .github/workflows/socat.yml; the source tarball was checked there against its pinned SHA-256 (the tarball Alpine's
# aports packages: same sha512 in main/socat/APKBUILD).
#
# Usage (in the container): sh scripts/build-socat-static.sh <socat-X.tar.gz> <out-dir>
set -eu
tarball=${1:?source tarball}
out=${2:?output folder}

apk add --no-cache build-base linux-headers >/dev/null

work=/tmp/socat-build
rm -rf "$work"
mkdir -p "$work" "$out"
tar -xzf "$tarball" -C "$work"
src=$(find "$work" -mindepth 1 -maxdepth 1 -type d | head -n 1)
cd "$src"

# musl: <netinet/if_ether.h> clashes with the kernel headers socat also includes (Alpine's use-linux-headers.patch).
sed -i 's|#include <netinet/if_ether.h>|#include <linux/if_ether.h>|' sysincludes.h
grep -q '#include <linux/if_ether.h>' sysincludes.h

# No OpenSSL, readline or libwrap: srt uses plain TCP, TCP6/TCP4-LISTEN, UNIX-LISTEN/CONNECT, PROXY and STDIO only.
# -Wno-int-conversion as Alpine builds it (newer GCC makes the warning an error).
CFLAGS="-O2 -Wno-int-conversion" LDFLAGS="-static" ./configure \
  --disable-openssl --disable-readline --disable-libwrap >/dev/null
make -j"$(nproc)" socat >/dev/null
strip socat
./socat -V | head -n 3

cp socat "$out/socat"
cp COPYING README "$out/"
