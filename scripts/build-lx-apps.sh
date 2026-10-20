#!/bin/sh
# Build the Linux programs that run on NodeOS through musl and liblx, and pack
# each as an NXF in build/lx/:
#
#   hello.nxf   user/lx/tests/hello.c: stdio, malloc, floats, errno
#   lua.nxf     Lua 5.4.7, cloned from github.com/lua/lua and built unmodified
#
#   scripts/build-lx-apps.sh <nxfpack>
#
# Needs the sysroot (scripts/build-musl.sh) and build/lx/liblx.a (`make lx`).
# Fetching Lua needs git and network access, once; the clone is cached.
set -eu

root=$(cd "$(dirname "$0")/.." && pwd)
nxfpack=${1:?usage: build-lx-apps.sh <nxfpack>}
out=$root/build/lx
cache=$root/build/cache
lua_tag=v5.4.7
lua_src=$cache/lua-${lua_tag#v}
cc=$root/scripts/lx-cc
mkdir -p "$out" "$cache"

# Every `syscall` instruction left in a program must belong to libnode or
# liblx -- Rust code speaking the NodeOS ABI. One inside a C function would be
# musl (or the program) handing the NodeOS kernel a Linux syscall number:
# Linux 1 is `write`, NodeOS 1 is `process_create`.
check_no_raw_syscalls() {
    offenders=$(objdump -d "$1" |
        awk '/^[0-9a-f]+ <.*>:$/ { fn = $2 } /\tsyscall *$/ { print fn }' |
        sort -u | grep -vE '^<(_R|__libnode_start>|__libnode_main>|__lx_syscall>)' || true)
    if [ -n "$offenders" ]; then
        echo "build-lx-apps: $1 issues raw syscalls outside libnode/liblx:" >&2
        echo "$offenders" >&2
        exit 1
    fi
}

pack() {
    check_no_raw_syscalls "$out/$1"
    "$nxfpack" "$out/$1" -o "$out/$1.nxf" --name "$1" \
        --cap CHANNEL --cap MEMORY --cap CLOCK --cap FILE_READ --cap FILE_WRITE --cap FILE_CREATE \
        --stack 1M --heap 2M --max-memory 256M
}

"$cc" -O2 -Wall -o "$out/hello" "$root/user/lx/tests/hello.c"
pack hello

if [ ! -f "$lua_src/onelua.c" ]; then
    rm -rf "$lua_src"
    git -c advice.detachedHead=false clone -q --depth 1 --branch "$lua_tag" \
        https://github.com/lua/lua "$lua_src"
fi
# onelua.c is Lua's own single-file build of the interpreter. LUA_USE_POSIX
# turns on the POSIX bits (isatty, mkstemp, popen) a Linux build would use.
"$cc" -O2 -std=gnu99 -DLUA_USE_POSIX -w -o "$out/lua" "$lua_src/onelua.c" -lm
pack lua
