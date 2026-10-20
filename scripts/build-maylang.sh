#!/bin/sh
# Maylang on NodeOS: build the compiler and test programs for x86_64-nodeos,
# and put them on a disk image.
#
#   scripts/build-maylang.sh build <nxfpack> <maylang-checkout>
#   scripts/build-maylang.sh install <disk.img>
#
# `build` stages the disk layout in build/maylang:
#
#   /bin/mayc.nxf          the Maylang compiler, built for NodeOS
#   /bin/runtime.may ...   the compiler's runtime sidecars, beside it as mayc
#   /bin/runtime/          expects (runtime.may, native.may, prelude.may and
#                          runtime/*.may, including nodeos.may)
#   /stdlib/               the standard library, found by walking up from mayc
#   /src/mayc/             the compiler's own source, so it can rebuild itself
#   /bin/maysmoke.nxf      user/maylang/tests/smoke.may, cross-compiled
#   /tests/*.may           programs for mayc to compile on NodeOS
#
# mayc emits NXF directly (--target x86_64-nodeos); nxfpack only checks each
# image with the loader's own parser. The first build of the maylang checkout
# bootstraps its compiler from source, which needs Rust and a C compiler.
set -eu

root=$(cd "$(dirname "$0")/.." && pwd)
out=$root/build/maylang

build() {
    nxfpack=$1
    maylang=$(cd "$2" && pwd)
    mayc=$maylang/compiler/mayc/mayc_new
    [ -x "$mayc" ] || { echo "build-maylang: no Maylang checkout at $maylang (set MAYLANG=...)" >&2; exit 1; }
    rm -rf "$out"
    mkdir -p "$out/bin/runtime" "$out/stdlib" "$out/src/mayc" "$out/tests"

    "$mayc" --target x86_64-nodeos "$maylang/compiler/mayc/main.may" -o "$out/bin/mayc.nxf"
    # The same compiler under the name the self-hosting test gives the copy it
    # builds on NodeOS, so the two can be compared byte for byte.
    "$mayc" --target x86_64-nodeos "$maylang/compiler/mayc/main.may" -o "$out/mayc2.nxf"
    "$mayc" --target x86_64-nodeos "$root/user/maylang/tests/smoke.may" -o "$out/bin/maysmoke.nxf"
    for image in "$out/bin/mayc.nxf" "$out/mayc2.nxf" "$out/bin/maysmoke.nxf"; do
        "$nxfpack" --verify "$image" | head -1
    done

    cp "$maylang/compiler/mayc/runtime.may" "$maylang/compiler/mayc/native.may" "$out/bin/"
    cp "$maylang/stdlib/prelude.may" "$out/bin/"
    cp "$maylang/compiler/mayc/runtime/"*.may "$out/bin/runtime/"
    cp "$maylang/stdlib/"*.may "$out/stdlib/"
    cp "$maylang/compiler/mayc/main.may" "$out/src/mayc/"
    (cd "$maylang/compiler/mayc" && find src -name '*.may' | while read -r file; do
        mkdir -p "$out/src/mayc/$(dirname "$file")"
        cp "$file" "$out/src/mayc/$file"
    done)
    cp "$maylang/examples/hello.may" "$out/tests/hello.may"
    cp "$root/user/maylang/tests/"*.may "$out/tests/"
    echo "build-maylang: staged $(find "$out" -type f | wc -l) files in $out"
}

# Copy the staged tree onto the disk, creating directories as needed. Names
# keep their case; the filesystem reads long names, and these are only read.
# mtools asks on the terminal what to do about a name that already exists, so
# only missing directories are created, and stdin is never the terminal: a
# question nobody sees would stop `make` for good.
install() {
    disk=$1
    [ -d "$out/bin" ] || { echo "build-maylang: nothing staged; run 'make maylang'" >&2; exit 1; }
    (cd "$out" && find bin stdlib src tests -type d) | while read -r dir; do
        mdir -b -i "$disk" "::/$dir" >/dev/null 2>&1 </dev/null || mmd -i "$disk" "::/$dir" </dev/null
    done
    (cd "$out" && find bin stdlib src tests -type f) | while read -r file; do
        mcopy -o -i "$disk" "$out/$file" "::/$file" </dev/null
    done
    echo "disk: packaged Maylang: /bin/mayc.nxf, its runtime, /stdlib and /src/mayc"
}

case "${1:-}" in
    build) build "${2:?nxfpack}" "${3:?maylang checkout}" ;;
    install) install "${2:?disk image}" ;;
    *) echo "usage: build-maylang.sh build <nxfpack> <maylang> | install <disk.img>" >&2; exit 2 ;;
esac
