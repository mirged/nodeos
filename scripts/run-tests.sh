#!/usr/bin/env bash
# Boot NodeOS in QEMU and assert that everything reported success.
#
# The kernel runs its self-tests on every boot, so "did the build work?" and
# "did the system work?" are the same question, answered by reading the serial
# log.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
ISO="${ROOT}/build/nodeos-test.iso"
LOG="${ROOT}/build/serial.log"
TIMEOUT="${TIMEOUT:-90}"

if [[ ! -f "${ISO}" ]]; then
    echo "run-tests: ${ISO} not found; run 'make iso' first" >&2
    exit 1
fi

mkdir -p "$(dirname "${LOG}")"
rm -f "${LOG}"

echo "run-tests: booting ${ISO} (timeout ${TIMEOUT}s)"
# `isa-debug-exit` lets the kernel report a status instead of leaving us to
# infer one: it exits QEMU with (value << 1) | 1, so 0x10 becomes 33.
QEMU_STATUS=0
timeout "${TIMEOUT}" qemu-system-x86_64 \
    -machine q35 -cpu qemu64 -m 512M \
    -cdrom "${ISO}" \
    -serial "file:${LOG}" \
    -device isa-debug-exit,iobase=0xf4,iosize=0x04 \
    -display none -no-reboot \
    >/dev/null 2>&1 || QEMU_STATUS=$?

if [[ ! -s "${LOG}" ]]; then
    echo "run-tests: FAIL -- no serial output at all" >&2
    exit 1
fi

strip_ansi() { sed -e 's/\x1b\[[0-9;]*m//g' "$1"; }
PLAIN="$(strip_ansi "${LOG}")"

fail() {
    echo "run-tests: FAIL -- $1" >&2
    echo "--- last 30 lines of serial output ---" >&2
    echo "${PLAIN}" | tail -30 >&2
    exit 1
}

grep -q "kernel panic" <<<"${PLAIN}" && fail "the kernel panicked"
grep -q "SELFTEST FAILED" <<<"${PLAIN}" && fail "a self-test failed"

grep -q "mm self-test passed" <<<"${PLAIN}" || fail "memory manager self-test did not run"

SELFTEST_LINE="$(grep -o 'self-tests: [0-9]* passed, [0-9]* failed' <<<"${PLAIN}" | tail -1 || true)"
[[ -n "${SELFTEST_LINE}" ]] || fail "kernel self-tests did not report"
grep -q ', 0 failed' <<<"${SELFTEST_LINE}" || fail "${SELFTEST_LINE}"

grep -q "syscall ABI online" <<<"${PLAIN}" || fail "the syscall ABI did not come up"
grep -q "launched \"init\"" <<<"${PLAIN}" || fail "init was not launched"
grep -q "init: userspace is alive" <<<"${PLAIN}" || fail "init did not reach ring 3"
grep -q "readback ok" <<<"${PLAIN}" || fail "userspace memory mapping was corrupt"
grep -q "init: done" <<<"${PLAIN}" || fail "init did not finish"
grep -q "autotest complete: PASS" <<<"${PLAIN}" || fail "the autotest supervisor did not report PASS"

# 33 == the kernel's SUCCESS code through isa-debug-exit; 124 == timeout(1).
case "${QEMU_STATUS}" in
    33) ;;
    35) fail "the kernel powered off reporting FAILURE" ;;
    124) fail "the boot did not finish within ${TIMEOUT}s" ;;
    *)  fail "unexpected QEMU exit status ${QEMU_STATUS}" ;;
esac

PASSED="$(grep -o '[0-9]* passed' <<<"${SELFTEST_LINE}" | grep -o '[0-9]*')"
echo "run-tests: PASS -- ${PASSED} kernel self-tests, userspace reached ring 3"
