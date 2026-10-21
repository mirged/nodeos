#!/usr/bin/env bash
# Boot NodeOS and run Maylang on it (docs/MAYLANG.md):
#
#   1. maysmoke, cross-compiled on the host, exercising the runtime's files,
#      directories, collections, faults, fibers, the clock and exec/wait;
#   2. mayc on NodeOS compiling /tests/hello.may, then the result run;
#   3. mayc on NodeOS compiling the smoke test, then the result run, which
#      must print what the cross-compiled copy printed;
#   then ten compiles in a row, which fail if exited programs keep memory;
#   4. with SELFHOST=1, mayc rebuilding itself from /src/mayc on NodeOS, the
#      new compiler compiling hello, and the compiler it built compared byte
#      for byte with the one cross-built on the host.
#
# The Maylang runtime mirrors every line a program writes to its console into
# the kernel log as `may: | <line>`, and reports `may: exited with N`, which is
# what this script reads from the serial log.
set -uo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
NAME=run-maylang-test
LOG="${ROOT}/build/maylang-test.log"
PORT="${PORT:-45481}"
EXPECT_PREFIX="may: | "
SELFHOST="${SELFHOST:-0}"
# Compiling the compiler needs about 400 MiB. KVM, where the host allows it,
# turns a self-build of minutes into seconds; QEMU falls back to TCG.
MEMORY="${MEMORY:-1G}"
ACCEL="${ACCEL:--accel kvm -accel tcg}"
STAGED="${ROOT}/build/maylang"
source "${ROOT}/scripts/lib/guest.sh"

require_files "${ISO}" "${DISK}"
boot_guest

# Wait for the Nth program exit; every Maylang program reports one.
exits=0
wait_for_exit() {
    local what="$1" seconds="$2" waited=0
    exits=$(( exits + 1 ))
    while (( waited < seconds * 4 )); do
        if (( $(plain_log | grep -c "may: exited with") >= exits )); then
            return 0
        fi
        sleep 0.25
        waited=$(( waited + 1 ))
    done
    echo "run-maylang-test: FAIL -- ${what} did not finish within ${seconds}s" >&2
    plain_log | grep -E "may:|shell:|panic|fault|exception" | tail -20 >&2
    exit 1
}

# Every line of the smoke test, as a program on NodeOS prints it. A run from
# the host (`mayc smoke.may && ./smoke arg`) prints the same apart from PATH.
SMOKE_LINES=(
    "smoke: hello from Maylang on NodeOS"
    "smoke: args 2 arg"
    "smoke: map fish=3 red=2 keys=3"
    "smoke: closure 3,6,9"
    "smoke: float 5.0 2.0 1.4142135623730951"
    "smoke: fault caught boom"
    "smoke: gc 200000 19999900000"
    "smoke: file 2 lines, exists=true"
    "smoke: dir A.TXT B.TXT"
    "smoke: missing"
    "smoke: fibers a1 b1 a2 b2"
    "smoke: clock ok=true"
    "smoke: env PATH=/bin"
    "smoke: child running"
    "smoke: exec status 7"
    "smoke: done"
)

wait_for_mount 30
wait_for_log "shell: ready" 30

# 1. A program cross-compiled on the host. It runs itself again as a child,
#    which is two exits.
type_line "maysmoke arg"
wait_for_exit "maysmoke (child)" 120
wait_for_exit "maysmoke" 120
for line in "${SMOKE_LINES[@]}"; do expect "${line}"; done
smoke_lines=$(plain_log | grep -c "may: | smoke:")

# 2. The compiler on NodeOS, then what it built.
type_line "mayc /tests/hello.may -o /hello.nxf"
wait_for_exit "mayc /tests/hello.may" 600
type_line "run /hello.nxf"
wait_for_exit "/hello.nxf" 60
expect "Hello, Maylang!"
expect "Hello, world!"

# 3. The smoke test compiled on NodeOS must behave like the cross-compiled one.
type_line "mayc /tests/smoke.may -o /smoke.nxf"
wait_for_exit "mayc /tests/smoke.may" 600
type_line "run /smoke.nxf arg"
wait_for_exit "/smoke.nxf (child)" 120
wait_for_exit "/smoke.nxf" 120
if (( $(plain_log | grep -c "may: | smoke:") == 2 * smoke_lines )); then
    echo "run-maylang-test: OK   the smoke test built on NodeOS printed the same ${smoke_lines} lines"
else
    echo "run-maylang-test: FAIL -- the smoke test built on NodeOS printed different output" >&2
    fail=1
fi

# Repeated compiles: each touches about 100 MiB, which the kernel must give
# back when the compiler exits (it once kept it, and the tenth run in 1 GiB
# faulted).
for (( round = 1; round <= 10; round++ )); do
    type_line "mayc /tests/hello.may -o /again.nxf"
    wait_for_exit "mayc, compile ${round} of 10" 600
done
if plain_log | grep -q "fatal exception"; then
    echo "run-maylang-test: FAIL -- a repeated compile faulted: memory was not released" >&2
    fail=1
else
    echo "run-maylang-test: OK   ten compiles in one boot: exited programs release their memory"
fi

# 4. The compiler rebuilding itself.
if [[ "${SELFHOST}" == 1 ]]; then
    type_line "mayc /src/mayc/main.may -o /bin/mayc2.nxf"
    wait_for_exit "mayc /src/mayc/main.may" 3600
    type_line "mayc2 /tests/hello.may -o /hello2.nxf"
    wait_for_exit "mayc2 /tests/hello.may" 600
    type_line "run /hello2.nxf"
    wait_for_exit "/hello2.nxf" 60
    if (( $(plain_log | grep -cF "may: | Hello, Maylang!") >= 2 )); then
        echo "run-maylang-test: OK   the compiler built on NodeOS compiles working programs"
    else
        echo "run-maylang-test: FAIL -- the compiler built on NodeOS did not produce a working hello" >&2
        fail=1
    fi
fi

if (( $(plain_log | grep -c "may: exited with") != $(plain_log | grep -c "may: exited with [07]$") )); then
    echo "run-maylang-test: FAIL -- a program exited with an unexpected status" >&2
    plain_log | grep "may: exited" >&2
    fail=1
fi
if plain_log | grep -q "kernel panic\|SELFTEST FAILED\|fatal exception"; then
    echo "run-maylang-test: FAIL -- a panic or fatal exception appeared" >&2
    fail=1
fi
monitor "screendump ${ROOT}/build/maylang-test.ppm"
shutdown_guest

# What the programs wrote is on the disk, where the host can read it back.
if [[ "$(mtype -i "${DISK}" ::/SMOKE.TXT 2>/dev/null)" == $'line one\nline two' ]]; then
    echo "run-maylang-test: OK   /SMOKE.TXT reads back on the host"
else
    echo "run-maylang-test: FAIL -- /SMOKE.TXT is missing or different on the disk" >&2
    fail=1
fi
if [[ "${SELFHOST}" == 1 ]]; then
    mcopy -o -i "${DISK}" ::/bin/mayc2.nxf "${ROOT}/build/mayc2-nodeos.nxf"
    if cmp -s "${ROOT}/build/mayc2-nodeos.nxf" "${STAGED}/mayc2.nxf"; then
        echo "run-maylang-test: OK   the compiler built on NodeOS is byte-identical to the host's"
    else
        echo "run-maylang-test: FAIL -- the compiler built on NodeOS differs from the host's" >&2
        fail=1
    fi
fi

if (( fail != 0 )); then
    echo "run-maylang-test: FAIL" >&2
    plain_log | grep -E "may:" | tail -40 >&2
    exit 1
fi
echo "run-maylang-test: PASS"
