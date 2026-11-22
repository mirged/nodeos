#!/bin/sh
# Build curl for NodeOS: upstream source, unmodified, compiled against musl with
# liblx underneath and packed as build/lx/curl.nxf.
#
#   scripts/build-curl.sh [nxfpack]
#
# Needs the sysroot (scripts/build-musl.sh) and build/lx/liblx.a (`make lx`),
# and network access once to fetch the release tarball, which is cached and
# checked against a pinned hash.
#
# curl is configured as a cross build -- `--host` makes configure stop trying to
# *run* the programs it compiles -- so it never executes a binary on the host and
# assumes the conservative answers for anything it cannot check. Everything it
# does not need is switched off: no TLS, no LDAP, no zlib, no IPv6, and no
# threaded resolver, because NodeOS has no threads yet.
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

version=8.11.1
sha256=a889ac9dbba3644271bd9d1302b5c22a088893719b72be3487bc3d401e5c4e80
tarball=$cache/curl-$version.tar.gz
src=$root/build/curl-$version

[ -f "$root/build/sysroot/lib/libc.a" ] || {
    echo "build-curl: no sysroot at build/sysroot; run scripts/build-musl.sh" >&2
    exit 1
}
[ -f "$out/liblx.a" ] || { echo "build-curl: no liblx; run 'make lx'" >&2; exit 1; }

mkdir -p "$cache" "$out"
if [ ! -f "$tarball" ]; then
    curl -sSfL "https://curl.se/download/curl-$version.tar.gz" -o "$tarball.part"
    mv "$tarball.part" "$tarball"
fi
echo "$sha256  $tarball" | sha256sum -c --quiet

if [ ! -f "$src/configure" ]; then
    rm -rf "$src"
    tar -xzf "$tarball" -C "$root/build"
    cd "$src"
    # `--disable-docs` and `--disable-manual` keep the build to C compiler work:
    # no generated PDFs, no embeddable help text.
    if ! CC=$cc ./configure \
        --host=x86_64-nodeos-linux-gnu --build=x86_64-pc-linux-gnu \
        --disable-shared --enable-static \
        --disable-docs --disable-manual --disable-tests --disable-ldap --disable-ldaps \
        --without-ssl --without-libpsl --without-zlib --without-brotli --without-zstd \
        --without-libidn2 --without-librtmp --without-nghttp2 --without-ngtcp2 \
        --without-nghttp3 --without-quiche --without-msh3 \
        --disable-threaded-resolver --disable-ipv6 --disable-unix-sockets \
        --disable-ftp --disable-file --disable-tftp --disable-telnet --disable-dict \
        --disable-gopher --disable-rtsp --disable-mqtt --disable-smtp --disable-imap \
        --disable-pop3 --disable-tls-srp --disable-gssapi --disable-sspi \
        --prefix="$root/build/curl-install" >"$root/build/curl-configure.log" 2>&1; then
        echo "build-curl: configure failed; see build/curl-configure.log" >&2
        exit 1
    fi
    echo "build-curl: configured curl $version (host x86_64-nodeos-linux-gnu)"
fi

cd "$src"
if ! make -j"$(nproc)" >"$root/build/curl-make.log" 2>&1; then
    echo "build-curl: build failed; see build/curl-make.log" >&2
    exit 1
fi
# Relink unconditionally: the objects do not change when liblx does, but the
# program is linked against it, so a stale link is how a `socket()` that liblx
# has just learned to answer becomes a call nobody handles.
rm -f src/curl
if ! make -C src curl >>"$root/build/curl-make.log" 2>&1; then
    echo "build-curl: link failed; see build/curl-make.log" >&2
    exit 1
fi

# Every `syscall` instruction left in the program must belong to libnode or
# liblx -- Rust code speaking the NodeOS ABI. One inside a C function would be
# curl (or musl) handing the NodeOS kernel a Linux syscall number: Linux 1 is
# `write`, NodeOS 1 is `process_create`.
cp src/curl "$out/curl"
offenders=$(objdump -d "$out/curl" |
    awk '/^[0-9a-f]+ <.*>:$/ { fn = $2 } /\tsyscall *$/ { print fn }' |
    sort -u | grep -vE '^<(_R|__libnode_start>|__libnode_main>|__lx_syscall>)' || true)
if [ -n "$offenders" ]; then
    echo "build-curl: $out/curl issues raw syscalls outside libnode/liblx:" >&2
    echo "$offenders" >&2
    exit 1
fi

"$nxfpack" "$out/curl" -o "$out/curl.nxf" --name curl \
    --cap CHANNEL --cap MEMORY --cap CLOCK --cap FILE_READ --cap FILE_WRITE \
    --cap FILE_CREATE --cap NETWORK \
    --stack 1M --heap 4M --max-memory 256M
echo "build-curl: $out/curl.nxf ready (curl $version)"
