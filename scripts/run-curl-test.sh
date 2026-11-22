#!/usr/bin/env bash
# Boot NodeOS and run `curl`, built from upstream source against musl and liblx,
# as an ordinary Linux program: fetch a file from a server on the host, write it
# to the disk, read it back, pull a quarter of a megabyte through the stack and
# check it byte for byte, and then fetch a page from the internet.
#
# The host server is reached as 10.0.2.2, which QEMU's user-mode network maps to
# the host's loopback. The internet fetch is a real one: it resolves example.com
# through the name server DHCP handed the stack and speaks HTTP over TCP to
# whatever address that name resolves to.
set -uo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
NAME=run-curl-test
LOG="${ROOT}/build/curl-test.log"
PORT="${PORT:-45491}"
HTTP_PORT="${HTTP_PORT:-8098}"
SERVE="${ROOT}/build/curl-serve"
EXPECT_PREFIX="lx: | "
source "${ROOT}/scripts/lib/guest.sh"

require_files "${ISO}" "${DISK}"

mkdir -p "${SERVE}"
printf 'hello from the host\n' > "${SERVE}/HELLO.TXT"
# 256 KiB of every byte value: enough to fill and refill smoltcp's rings, and
# checkable with a hash rather than by eye.
python3 - "${SERVE}/BIG.BIN" <<'PY'
import sys
with open(sys.argv[1], "wb") as f:
    f.write(bytes(range(256)) * 1024)
PY
python3 -m http.server --bind 127.0.0.1 --directory "${SERVE}" "${HTTP_PORT}" \
    >/dev/null 2>&1 &
HTTP_PID=$!
trap 'kill "${HTTP_PID}" "${QEMU_PID}" 2>/dev/null' EXIT
sleep 0.5

# Wait until `count` programs have reported their exit status: each curl run
# ends with a line liblx writes, so counting them is a stable way to know a
# program finished without depending on what it printed.
wait_for_exits() {
    local count="$1" seconds="$2" waited=0
    while (( waited < seconds * 4 )); do
        (( $(plain_log | grep -c "lx: exited with") >= count )) && return 0
        sleep 0.25
        waited=$(( waited + 1 ))
    done
    return 1
}

boot_guest
wait_for_mount 25

echo "${NAME}: curl http://10.0.2.2:${HTTP_PORT}/HELLO.TXT"
type_line "curl -s http://10.0.2.2:${HTTP_PORT}/HELLO.TXT"
if ! wait_for_exits 1 90; then
    echo "${NAME}: FAIL -- curl did not finish" >&2
    plain_log | grep -E "curl|lx:|shell:" | tail -30 >&2
    fail=1
fi
expect "hello from the host"

echo "${NAME}: curl --output /DOWNLOAD.TXT, then read it back"
type_line "curl -s -o /DOWNLOAD.TXT http://10.0.2.2:${HTTP_PORT}/HELLO.TXT"
wait_for_exits 2 90
type_line "cat /DOWNLOAD.TXT"
wait_for_log "hello from the host" 20

echo "${NAME}: 256 KiB over TCP, to /BIG.TXT"
type_line "curl -s -o /BIG.TXT http://10.0.2.2:${HTTP_PORT}/BIG.BIN"
if ! wait_for_exits 3 180; then
    echo "${NAME}: FAIL -- the large transfer did not finish" >&2
    plain_log | grep -E "curl|lx:|net:" | tail -30 >&2
    fail=1
fi

echo "${NAME}: curl http://example.com/ (the internet)"
type_line "curl -s http://example.com/"
if wait_for_log "Example Domain" 120; then
    echo "${NAME}: OK   example.com answered over the internet"
else
    echo "${NAME}: FAIL -- no page from example.com" >&2
    plain_log | grep -E "curl|lx:" | tail -30 >&2
    fail=1
fi

if plain_log | grep -q "kernel panic\|SELFTEST FAILED\|fatal exception"; then
    echo "${NAME}: FAIL -- a panic or fatal exception appeared" >&2
    fail=1
fi

monitor "screendump ${ROOT}/build/curl-test.ppm"
shutdown_guest
kill "${HTTP_PID}" 2>/dev/null

# Nothing is writing the disk any more: compare what curl saved with what the
# host served. A short page cannot make this check -- it covers the whole path,
# including the send and receive loops that handle partial transfers.
big_copy="${ROOT}/build/curl-big-copy.bin"
if mcopy -o -i "${DISK}" ::/BIG.TXT "${big_copy}" 2>/dev/null; then
    if cmp -s "${big_copy}" "${SERVE}/BIG.BIN"; then
        echo "${NAME}: OK   /BIG.TXT is identical to the 256 KiB the host served"
    else
        echo "${NAME}: FAIL -- /BIG.TXT differs from what the host served" >&2
        fail=1
    fi
    rm -f "${big_copy}"
else
    echo "${NAME}: FAIL -- curl left no /BIG.TXT on the disk" >&2
    fail=1
fi

if (( fail != 0 )); then
    echo "${NAME}: FAIL" >&2
    exit 1
fi
echo "${NAME}: PASS"
