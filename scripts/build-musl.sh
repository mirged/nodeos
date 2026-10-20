#!/bin/sh
# Build the NodeOS musl sysroot: upstream musl with liblx's seven-file overlay.
#
#   scripts/build-musl.sh [sysroot]        default: build/sysroot
#
# The tarball is fetched once into build/cache and checked against a pinned
# hash; the overlay in user/lx/musl/ replaces the only files in musl that issue
# a raw `syscall` instruction, so that every Linux syscall musl makes becomes a
# call to `__lx_syscall` in liblx instead (docs/MUSL.md, section 1).
set -eu

MUSL_VERSION=1.2.6
MUSL_SHA256=d585fd3b613c66151fc3249e8ed44f77020cb5e6c1e635a616d3f9f82460512a

root=$(cd "$(dirname "$0")/.." && pwd)
sysroot=${1:-$root/build/sysroot}
case $sysroot in
    /*) ;;
    *) sysroot=$(pwd)/$sysroot ;; # the build below runs from inside the musl tree
esac
cache=$root/build/cache
src=$root/build/musl-$MUSL_VERSION
tarball=$cache/musl-$MUSL_VERSION.tar.gz

mkdir -p "$cache"
if [ ! -f "$tarball" ]; then
    curl -sSfL "https://musl.libc.org/releases/musl-$MUSL_VERSION.tar.gz" -o "$tarball.part"
    mv "$tarball.part" "$tarball"
fi
echo "$MUSL_SHA256  $tarball" | sha256sum -c --quiet

rm -rf "$src"
tar -xzf "$tarball" -C "$root/build"
cp -R "$root/user/lx/musl/." "$src/"

cd "$src"
# `-fno-pie`: NodeOS programs are linked position-dependent at the program
# window (user/lx/lx.ld), so there is no reason to pay for PIC code.
CC=${CC:-gcc} CFLAGS="-O2 -fno-pie" ./configure --prefix="$sysroot" \
    --disable-shared --disable-wrapper >/dev/null
make -j"$(nproc)" >/dev/null
rm -rf "$sysroot"
make install >/dev/null

# The proof the overlay is complete: no object in libc.a may contain a raw
# `syscall`. One that did would hand the NodeOS kernel a Linux syscall number.
if objdump -d "$sysroot/lib/libc.a" | grep -qE '\ssyscall\s*$'; then
    echo "build-musl: libc.a still contains a raw syscall instruction:" >&2
    objdump -d "$sysroot/lib/libc.a" | grep -B20 -E '\ssyscall\s*$' | grep -E '^[0-9a-f]+ <|syscall' >&2
    exit 1
fi
echo "build-musl: $sysroot ready (musl $MUSL_VERSION, no raw syscalls)"
