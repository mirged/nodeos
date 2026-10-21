#!/usr/bin/env bash
# Boot NodeOS and run two Linux programs built against musl: a C test program
# and Lua 5.4 (github.com/lua/lua) running a script from the disk.
#
# Both are ordinary C, compiled with scripts/lx-cc against the NodeOS musl
# sysroot. liblx mirrors everything they write to stdout into the kernel log,
# prefixed `lx: |`, which is what this script greps. Getting the expected lines
# means the whole path worked: the shell launched an NXF packed from a static
# musl ELF, musl started up on liblx's Linux-style stack and auxv, TLS came up
# through the new per-thread FS base, and stdio, malloc, floating point and
# files all went through liblx to the console and filesystem services.
set -uo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
NAME=run-lua-test
LOG="${ROOT}/build/lua-test.log"
PORT="${PORT:-45485}"
EXPECT_PREFIX="lx: | "
source "${ROOT}/scripts/lib/guest.sh"

require_files "${ISO}" "${DISK}"
boot_guest
wait_for_mount 20

# Wait for the Nth "lx: exited with" line: each program reports its exit.
wait_for_exit() {
    local count="$1" seconds="$2" waited=0
    while (( waited < seconds * 4 )); do
        (( $(plain_log | grep -c "lx: exited with") >= count )) && return 0
        sleep 0.25
        waited=$(( waited + 1 ))
    done
    return 1
}

type_line "run /bin/hello.nxf"
if ! wait_for_exit 1 60; then
    echo "run-lua-test: FAIL -- the musl hello program did not finish" >&2
    plain_log | grep -E "lx:|run:|panic|fault|exception" | tail -20 >&2
    exit 1
fi
expect "hello from musl on NodeOS"
expect "argc=1 argv[0]=/bin/hello.nxf"
expect "float: 3.142 3e-05"
expect "malloc: ok"
expect "fopen missing: No such file or directory (errno 2)"

type_line "run /bin/lua.nxf /tests/hello.lua"
if ! wait_for_exit 2 120; then
    echo "run-lua-test: FAIL -- lua did not finish" >&2
    plain_log | grep -E "lx:|run:|panic|fault|exception" | tail -20 >&2
    exit 1
fi
expect "hello from Lua 5.4 on NodeOS"
expect "fib(20) = 6765"
expect "pi ~ 3.141593, sqrt(2) = 1.4142, 2^0.5 = 1.41421"
expect "int division: 7 // 2 = 3, float: 3.5"
expect "sorted: brown dog fox jumps lazy over quick the the"
expect "gsub: NODE0S"
expect "coroutine: 1 4 9"
expect "pcall: false caught this"
expect "file: 5 lines, sum 15"
expect "io.open missing: nil (/no/such/file: No such file or directory)"
expect "os.clock() > 0: true"
expect "done"

# Interactive: with no script, Lua sees a terminal on stdin (isatty -> TCGETS)
# and starts its REPL. Lines come from the console's line input, typed here.
type_line "run /bin/lua.nxf"
sleep 3
type_line "x = 6*7"
type_line "print(\"answer\", x)"
type_line "os.exit(0)"
if ! wait_for_exit 3 60; then
    echo "run-lua-test: FAIL -- the interactive lua session did not exit" >&2
    plain_log | grep -E "lx:|run:|panic|fault|exception" | tail -20 >&2
    exit 1
fi
if plain_log | grep -qE "lx: \| .*answer	42"; then
    echo "run-lua-test: OK   interactive: print(\"answer\", x) -> answer	42"
else
    echo "run-lua-test: FAIL -- the REPL did not evaluate typed input" >&2
    fail=1
fi

if (( $(plain_log | grep -c "lx: exited with 0") < 3 )); then
    echo "run-lua-test: FAIL -- a program exited with a non-zero status" >&2
    plain_log | grep "lx: exited" >&2
    fail=1
fi
# The same output on the screen, not only in the log: a screenshot for a
# human to look at. `screendump` writes PPM; nothing here parses it.
monitor "screendump ${ROOT}/build/lua-test.ppm"

if plain_log | grep -q "kernel panic\|SELFTEST FAILED\|fatal exception"; then
    echo "run-lua-test: FAIL -- a panic or fatal exception appeared" >&2
    fail=1
fi

shutdown_guest

if (( fail != 0 )); then
    echo "run-lua-test: FAIL" >&2
    plain_log | grep -E "lx:" | tail -40 >&2
    exit 1
fi
echo "run-lua-test: PASS"
