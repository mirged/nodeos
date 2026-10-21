#!/usr/bin/env bash
# Boot NodeOS, compile and run a C program with `cc`, and check its output.
#
# The compiler runs on the guest, reads the source from the disk, and its
# program's output is echoed into the kernel log (prefixed `cc: |`), which is
# what this script greps. Getting the expected lines means the whole path
# worked: the shell launched cc, cc read the file through the filesystem, the
# tokenizer and parser and code generator ran, the virtual machine executed the
# program, and the standard library builtin it called returned the right answer.
set -uo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
NAME=run-cc-test
LOG="${ROOT}/build/cc-test.log"
PORT="${PORT:-45477}"
EXPECT_PREFIX="cc: | "
source "${ROOT}/scripts/lib/guest.sh"

require_files "${ISO}" "${DISK}"
boot_guest
wait_for_mount 20

type_line "run /bin/cc.nxf /tests/hello.c"
if ! wait_for_log "cc: /tests/hello.c exited with 0" 60; then
    echo "run-cc-test: FAIL -- cc did not finish" >&2
    plain_log | grep -E "cc:|run:|panic" | tail -20 >&2
    exit 1
fi

expect "hello, world"
expect "sum 1..10 = 55"
expect "fib(10)   = 55"
expect "5!        = 120"
expect "squares   = 0 1 4 9 16 25 36 49"
expect "strlen    = 6"
expect "heap      = 42"

if plain_log | grep -q "kernel panic\|SELFTEST FAILED\|cc: .*error"; then
    echo "run-cc-test: FAIL -- a panic or compiler error appeared" >&2
    fail=1
fi

shutdown_guest

if (( fail != 0 )); then
    echo "run-cc-test: FAIL" >&2
    plain_log | grep -E "cc:" | tail -25 >&2
    exit 1
fi
echo "run-cc-test: PASS"
