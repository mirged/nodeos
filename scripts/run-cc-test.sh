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
ISO="${ISO:-${ROOT}/build/nodeos.iso}"
DISK="${DISK:-${ROOT}/build/disk.img}"
LOG="${ROOT}/build/cc-test.log"
PORT="${PORT:-45477}"
BOOTWAIT="${BOOTWAIT:-14}"

for required in "${ISO}" "${DISK}"; do
    [[ -f "${required}" ]] || { echo "run-cc-test: ${required} not found; run 'make'" >&2; exit 1; }
done

rm -f "${LOG}"
qemu-system-x86_64 \
    -machine q35 -cpu qemu64 -smp 1 -m 512M -boot d -no-reboot \
    -cdrom "${ISO}" -serial "file:${LOG}" -display none \
    -audiodev none,id=snd0 -device AC97,audiodev=snd0 \
    -drive file="${DISK}",if=virtio,format=raw \
    -monitor tcp:127.0.0.1:${PORT},server,nowait >/dev/null 2>&1 &
QEMU_PID=$!
cleanup() { kill "${QEMU_PID}" 2>/dev/null; wait "${QEMU_PID}" 2>/dev/null; }
trap cleanup EXIT

monitor() {
    exec 3<>"/dev/tcp/127.0.0.1/${PORT}" || return 1
    printf '%s\n' "$@" >&3
    sleep 0.15
    exec 3<&- 3>&-
}

type_line() {
    local text="$1" char
    for (( i = 0; i < ${#text}; i++ )); do
        char="${text:i:1}"
        case "${char}" in
            " ") monitor "sendkey spc" ;;
            "/") monitor "sendkey slash" ;;
            ".") monitor "sendkey dot" ;;
            *)   monitor "sendkey ${char}" ;;
        esac
    done
    monitor "sendkey ret"
}

plain_log() { sed -e 's/\x1b\[[0-9;]*m//g' "${LOG}"; }

wait_for_log() {
    local pattern="$1" seconds="${2:-30}" waited=0
    while (( waited < seconds * 4 )); do
        plain_log | grep -q "${pattern}" && return 0
        sleep 0.25
        waited=$(( waited + 1 ))
    done
    return 1
}

echo "run-cc-test: booting ${ISO}"
sleep "${BOOTWAIT}"
wait_for_log "fs: FAT16 mounted\|fs: FAT32 mounted" 20 || {
    echo "run-cc-test: FAIL -- the filesystem service did not mount the disk" >&2
    tail -20 "${LOG}" >&2
    exit 1
}

type_line "run /bin/cc.nxf /tests/hello.c"
if ! wait_for_log "cc: /tests/hello.c exited with 0" 60; then
    echo "run-cc-test: FAIL -- cc did not finish" >&2
    plain_log | grep -E "cc:|run:|panic" | tail -20 >&2
    exit 1
fi

fail=0
expect() {
    if plain_log | grep -qF "cc: | $1"; then
        echo "run-cc-test: OK   $1"
    else
        echo "run-cc-test: FAIL -- missing output line: $1" >&2
        fail=1
    fi
}

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

cleanup
trap - EXIT

if (( fail != 0 )); then
    echo "run-cc-test: FAIL" >&2
    plain_log | grep -E "cc:" | tail -25 >&2
    exit 1
fi
echo "run-cc-test: PASS"
