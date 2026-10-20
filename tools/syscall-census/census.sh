#!/bin/sh
# Which Linux syscalls does a musl program actually make?
#
# Builds musl and a static busybox on the *host*, runs a shell workload and a
# pthread test under a small ptrace tracer, and prints a histogram of syscall
# names. That list is the work order for liblx (see docs/MUSL.md): every name
# in it is something the NodeOS translation layer must answer.
#
#   tools/syscall-census/census.sh [workdir]
#
# Needs gcc, make, curl, python3 and network access. Nothing is installed; the
# whole build lives under the work directory.
set -eu

here=$(cd "$(dirname "$0")" && pwd)
work=${1:-${TMPDIR:-/tmp}/nodeos-census}
musl=1.2.6
busybox=1.37.0
mkdir -p "$work"
cd "$work"

prefix=$work/prefix
if [ ! -x "$prefix/bin/musl-gcc" ]; then
    curl -sSfL "https://musl.libc.org/releases/musl-$musl.tar.gz" | tar -xz
    (cd "musl-$musl" && ./configure --prefix="$prefix" --disable-shared >/dev/null &&
        make -j"$(nproc)" >/dev/null && make install >/dev/null)
    # busybox includes kernel UAPI headers, which musl does not ship.
    cp -r /usr/include/linux /usr/include/asm-generic /usr/include/mtd "$prefix/include/"
    cp -rL "/usr/include/$(gcc -print-multiarch)/asm" "$prefix/include/asm"
fi

bb=$work/busybox-$busybox/busybox
if [ ! -x "$bb" ]; then
    curl -sSfL "https://busybox.net/downloads/busybox-$busybox.tar.bz2" | tar -xj
    (cd "busybox-$busybox" && make defconfig >/dev/null &&
        sed -i 's/^# CONFIG_STATIC is not set/CONFIG_STATIC=y/; s/^CONFIG_TC=y/# CONFIG_TC is not set/' .config &&
        make -j"$(nproc)" CC="$prefix/bin/musl-gcc" busybox >build.log 2>&1)
fi

gcc -O2 -o tracer "$here/tracer.c"
"$prefix/bin/musl-gcc" -static -O2 -pthread -o threads "$here/threads.c"

# Only busybox applets on PATH: otherwise the shell runs the host's glibc
# coreutils, and their syscalls (rseq, llistxattr, ...) pollute the count.
mkdir -p bin run
for applet in $("$bb" --list); do ln -sf "$bb" "bin/$applet"; done

(cd run && env -i PATH="$work/bin" ../tracer "$bb" sh "$here/workload.sh" >/dev/null 2>../busybox.counts)
./tracer ./threads >/dev/null 2>threads.counts

python3 -I - "$(gcc -print-multiarch)" busybox.counts threads.counts <<'EOF'
import re, sys
names = {}
for line in open(f"/usr/include/{sys.argv[1]}/asm/unistd_64.h"):
    m = re.match(r"#define __NR_(\w+) (\d+)", line)
    if m:
        names[int(m.group(2))] = m.group(1)
for path in sys.argv[2:]:
    rows = sorted((l.split() for l in open(path)), key=lambda r: -int(r[1]))
    print(f"{path}: {len(rows)} distinct")
    print("  " + " ".join(f"{names.get(int(n), n)}({c})" for n, c in rows))
EOF
