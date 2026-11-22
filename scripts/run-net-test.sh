#!/usr/bin/env bash
# Boot NodeOS and use `netprobe` -- the network service's smoke test -- to prove
# the stack works end to end: DHCP or the QEMU fallback address, a TCP
# connection to a real HTTP server, a full response, and a DNS lookup for a
# name on the internet.
#
# The HTTP server runs on the host and is reached as 10.0.2.2, which is what
# QEMU's user-mode network calls the host's loopback. The DNS query goes to the
# name server the stack learned (10.0.2.3 under QEMU), so it exercises UDP, the
# resolver-facing socket path, and whatever network the host itself has.
set -uo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
NAME=run-net-test
LOG="${ROOT}/build/net-test.log"
PORT="${PORT:-45490}"
HTTP_PORT="${HTTP_PORT:-8099}"
SERVE="${ROOT}/build/net-serve"
EXPECT_PREFIX="netprobe: | "
source "${ROOT}/scripts/lib/guest.sh"

require_files "${ISO}" "${DISK}"

mkdir -p "${SERVE}"
printf 'hello from the host\n' > "${SERVE}/HELLO.TXT"
python3 -m http.server --bind 127.0.0.1 --directory "${SERVE}" "${HTTP_PORT}" \
    >/dev/null 2>&1 &
HTTP_PID=$!
trap 'kill "${HTTP_PID}" "${QEMU_PID}" 2>/dev/null' EXIT
sleep 0.5

boot_guest
wait_for_mount 25

echo "${NAME}: fetching http://10.0.2.2:${HTTP_PORT}/HELLO.TXT"
type_line "netprobe 10.0.2.2 ${HTTP_PORT} /HELLO.TXT"
if ! wait_for_log "netprobe: response complete" 90; then
    echo "${NAME}: FAIL -- no complete HTTP response" >&2
    plain_log | grep -E "netprobe:|net:|lx:" | tail -30 >&2
    fail=1
fi
expect "hello from the host"

echo "${NAME}: resolving example.com"
type_line "netprobe dns example.com"
if wait_for_log "netprobe: example.com is" 90; then
    plain_log | grep "netprobe: example.com is" | tail -1
else
    echo "${NAME}: FAIL -- DNS lookup of example.com did not answer" >&2
    plain_log | grep -E "netprobe:" | tail -20 >&2
    fail=1
fi

if plain_log | grep -q "kernel panic\|SELFTEST FAILED\|fatal exception"; then
    echo "${NAME}: FAIL -- a panic or fatal exception appeared" >&2
    fail=1
fi

monitor "screendump ${ROOT}/build/net-test.ppm"
shutdown_guest
kill "${HTTP_PID}" 2>/dev/null

if (( fail != 0 )); then
    echo "${NAME}: FAIL" >&2
    exit 1
fi
echo "${NAME}: PASS"
