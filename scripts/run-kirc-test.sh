#!/usr/bin/env bash
# Boot NodeOS and run `kirc`, built from upstream source against musl and liblx,
# as an ordinary Linux program: connect to an IRC server on the host, register,
# join a channel, send a message, read the server's own lines back, and quit.
#
# The server is scripts/lib/ircd.py, on the host. QEMU's user-mode network maps
# the host's loopback to 10.0.2.2, so the guest dials the server without any
# host configuration. The server logs what it receives, and the serial log
# carries what kirc drew, so the two ends together prove the round trip.
set -uo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
NAME=run-kirc-test
LOG="${ROOT}/build/kirc-test.log"
PORT="${PORT:-45593}"
IRC_PORT="${IRC_PORT:-6669}"
IRC_LOG="${ROOT}/build/kirc-ircd.log"
EXPECT_PREFIX="lx: | "
source "${ROOT}/scripts/lib/guest.sh"

require_files "${ISO}" "${DISK}"

python3 "${ROOT}/scripts/lib/ircd.py" "${IRC_PORT}" "${IRC_LOG}" >/dev/null 2>&1 &
IRC_PID=$!

boot_guest
trap 'kill "${IRC_PID}" "${QEMU_PID}" 2>/dev/null' EXIT
wait_for_mount 25
wait_for_log "net: DHCP\|net: no DHCP answer" 20

# A line in the server's log, waited for the same way as a serial line: the
# guest runs at its own speed and a fixed sleep is a race.
wait_for_server() {
    local pattern="$1" seconds="$2" waited=0
    while (( waited < seconds * 4 )); do
        grep -qF "${pattern}" "${IRC_LOG}" 2>/dev/null && return 0
        sleep 0.25
        waited=$(( waited + 1 ))
    done
    return 1
}

echo "${NAME}: kirc -s 10.0.2.2 -p ${IRC_PORT} probe"
type_line "run /bin/kirc.nxf -s 10.0.2.2 -p ${IRC_PORT} probe"
if ! wait_for_server "USER probe" 60; then
    echo "${NAME}: FAIL -- kirc did not register with the server" >&2
    plain_log | tail -30 >&2
    fail=1
fi

# kirc reads its keyboard before its socket, so what the server said is shown
# while it handles the keys of the next line. Typing /join is what makes the
# server's registration replies appear; the check for them follows the check
# that the join arrived. (001 itself is not drawn: kirc uses it to auto-join
# the channels it was started with, so 002 is the first line it shows.)
echo "${NAME}: /join #test"
type_line "/join #test"
if ! wait_for_server "JOIN #test" 30; then
    echo "${NAME}: FAIL -- kirc did not join the channel" >&2
    plain_log | tail -30 >&2
    fail=1
fi
if wait_for_log "Your host is irc.test, running a stub" 30; then
    echo "${NAME}: OK   the server's registration reply reached kirc"
else
    echo "${NAME}: FAIL -- kirc never showed the server's registration reply" >&2
    plain_log | tail -30 >&2
    fail=1
fi

# The client's message target is the channel it was told about. Say so, then
# send: the server echoes both back, and a client that never reads its socket
# would show neither.
echo "${NAME}: /set #test, then a channel message"
type_line "/set #test"
wait_for_server "set #test" 20
type_line "hello from nodeos"
if ! wait_for_server "PRIVMSG #test :hello from nodeos" 30; then
    echo "${NAME}: FAIL -- kirc did not send the channel message" >&2
    plain_log | tail -30 >&2
    fail=1
fi

echo "${NAME}: /quit"
type_line "/quit"
# The server closes the connection on QUIT, but kirc notices only when it next
# looks at the socket, and it reads its keyboard first. One more key is what
# lets it see the close and leave.
type_line "x"
if ! wait_for_log "lx: exited with" 60; then
    echo "${NAME}: FAIL -- kirc did not exit after the server closed" >&2
    plain_log | tail -30 >&2
    fail=1
fi

# Lines the server wrote are proof of the other direction: registration's
# welcome, then the channel message it echoed back. The channel line's shape
# (`nick [channel]: text`) is what tells it apart from kirc's own redrawing of
# the input line.
if wait_for_log "kirc-test-welcome" 20; then
    echo "${NAME}: OK   the server's NOTICE reached kirc"
else
    echo "${NAME}: FAIL -- kirc never showed the server's NOTICE" >&2
    plain_log | tail -30 >&2
    fail=1
fi
if wait_for_log "probe \[#test\]: hello from nodeos" 20; then
    echo "${NAME}: OK   the echoed channel message reached kirc"
else
    echo "${NAME}: FAIL -- kirc never showed the echoed message" >&2
    plain_log | tail -30 >&2
    fail=1
fi

if plain_log | grep -q "kernel panic\|SELFTEST FAILED\|fatal exception"; then
    echo "${NAME}: FAIL -- a panic or fatal exception appeared" >&2
    fail=1
fi

monitor "screendump ${ROOT}/build/kirc-test.ppm"
shutdown_guest
kill "${IRC_PID}" 2>/dev/null

if (( fail != 0 )); then
    echo "${NAME}: FAIL" >&2
    exit 1
fi
echo "${NAME}: PASS"
