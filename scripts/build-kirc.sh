#!/bin/sh
# Build kirc for NodeOS: upstream source, unmodified, compiled against musl
# with liblx underneath and packed as build/lx/kirc.nxf.
#
#   scripts/build-kirc.sh [nxfpack]
#
# Needs the sysroot (scripts/build-musl.sh) and build/lx/liblx.a (`make lx`).
# The release tarball is cached in build/cache and checked against a pinned
# hash, so a rebuild with the tarball present needs no network.
#
# kirc is a small IRC client. Nothing about it is changed here: it is a
# terminal program that uses sockets, termios and ANSI output, and the point of
# the port is that liblx has to answer those calls the way Linux does.
set -eu

root=$(cd "$(dirname "$0")/.." && pwd)
nxfpack=${1:-$root/tools/target/release/nxfpack}
# Make the packer's path absolute before anything changes directory.
case $nxfpack in
    /*) ;;
    *) nxfpack=$(pwd)/$nxfpack ;;
esac
out=$root/build/lx
cache=$root/build/cache
cc=$root/scripts/lx-cc

version=1.2.2
sha256=a54bc4d1a644f6fcb610f702454affc241b26da7fbde51951e0ec59133fa7b1d
tarball=$cache/kirc-$version.tar.gz
src=$root/build/kirc-$version

[ -f "$root/build/sysroot/lib/libc.a" ] || {
    echo "build-kirc: no sysroot at build/sysroot; run scripts/build-musl.sh" >&2
    exit 1
}
[ -f "$out/liblx.a" ] || { echo "build-kirc: no liblx; run 'make lx'" >&2; exit 1; }

mkdir -p "$cache" "$out"
if [ ! -f "$tarball" ]; then
    curl -sSfL "https://github.com/mcpcpc/kirc/archive/refs/tags/$version.tar.gz" \
        -o "$tarball.part"
    mv "$tarball.part" "$tarball"
fi
echo "$sha256  $tarball" | sha256sum -c --quiet

if [ ! -f "$src/Makefile" ]; then
    rm -rf "$src"
    tar -xzf "$tarball" -C "$root/build"
fi

# kirc's Makefile takes $(CC) from config.mk, and a command-line variable beats
# a file assignment: the source tree stays exactly as upstream shipped it.
# `clean` first, because the objects are only valid for the compiler that made
# them -- same host gcc, but a rebuild after a liblx change must relink.
make -C "$src" clean >/dev/null 2>&1 || true
if ! make -C "$src" CC="$cc" >"$root/build/kirc-make.log" 2>&1; then
    echo "build-kirc: build failed; see build/kirc-make.log" >&2
    exit 1
fi

# Every `syscall` instruction left in the program must belong to libnode or
# liblx -- Rust code speaking the NodeOS ABI. One inside a C function would be
# kirc (or musl) handing the NodeOS kernel a Linux syscall number: Linux 1 is
# `write`, NodeOS 1 is `process_create`.
cp "$src/kirc" "$out/kirc"
offenders=$(objdump -d "$out/kirc" |
    awk '/^[0-9a-f]+ <.*>:$/ { fn = $2 } /\tsyscall *$/ { print fn }' |
    sort -u | grep -vE '^<(_R|__libnode_start>|__libnode_main>|__lx_syscall>)' || true)
if [ -n "$offenders" ]; then
    echo "build-kirc: $out/kirc issues raw syscalls outside libnode/liblx:" >&2
    echo "$offenders" >&2
    exit 1
fi

# The capabilities the program actually needs: a console and files for its
# own I/O, a clock for its poll timeouts, and the network channel for IRC.
"$nxfpack" "$out/kirc" -o "$out/kirc.nxf" --name kirc \
    --cap CHANNEL --cap MEMORY --cap CLOCK --cap FILE_READ --cap FILE_WRITE \
    --cap FILE_CREATE --cap NETWORK \
    --stack 1M --heap 4M --max-memory 256M
echo "build-kirc: $out/kirc.nxf ready (kirc $version)"
