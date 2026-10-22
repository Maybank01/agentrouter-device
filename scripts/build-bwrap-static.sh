#!/bin/sh
# Builds bubblewrap (bwrap) as one static program against musl, for Linux folder mode (#122): srt confines every local
# server with bubblewrap, and the device brings its own (as it does socat and ripgrep) so the person installs nothing,
# and so Ubuntu's AppArmor profile for it can name one root-owned copy (`agentrouter sandbox userns install`). No
# upstream binary build exists. Runs inside the pinned Alpine image of .github/workflows/bwrap.yml; the source tarball
# was checked there against its pinned SHA-256 (the one in the upstream release notes).
#
# Usage (in the container): sh scripts/build-bwrap-static.sh <bubblewrap-X.tar.xz> <out-dir>
set -eu
tarball=${1:?source tarball}
out=${2:?output folder}

apk add --no-cache build-base linux-headers meson samurai pkgconf libcap-dev libcap-static >/dev/null

mkdir -p "$out"
out=$(cd "$out" && pwd)
work=/tmp/bwrap-build
rm -rf "$work"
mkdir -p "$work"
tar -xJf "$tarball" -C "$work"
src=$(find "$work" -mindepth 1 -maxdepth 1 -type d | head -n 1)
cd "$src"

# libcap only (no SELinux), no man page, completions or tests; one static executable.
meson setup _build --buildtype=release --prefer-static -Ddefault_library=static -Dc_link_args=-static \
  -Dselinux=disabled -Dman=disabled -Dtests=false -Dbash_completion=disabled -Dzsh_completion=disabled >/dev/null
meson compile -C _build bwrap >/dev/null
strip _build/bwrap
_build/bwrap --version

cp _build/bwrap "$out/bwrap"
cp COPYING "$out/"
