#!/usr/bin/env bash
# Boot NodeOS, edit a new file with the windowed editor, save it, and read the
# result back off the same disk with mtools.
#
# The editor is a compositor client now, so this also checks the windowing
# path: the shell launches it, the compositor gives the new window the
# keyboard, keystrokes reach the editor, and a save travels back through the
# filesystem. The host's read is what makes it a real check rather than the
# editor agreeing with itself.
set -uo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
NAME=run-edit-test
LOG="${ROOT}/build/edit-test.log"
PORT="${PORT:-45479}"
source "${ROOT}/scripts/lib/guest.sh"

require_files "${ISO}" "${DISK}"

# A file left over from an earlier run would make the final check pass without
# the editor having written anything.
mdel -i "${DISK}" ::/EDITTEST.TXT >/dev/null 2>&1 || true

boot_guest
wait_for_mount 20

# Lower case throughout: QEMU's `sendkey` names keys, not shifted characters.
# FAT upper-cases what it stores, which is why the host checks for the name in
# capital letters below.
type_line "run /bin/edit.nxf /edittest.txt"
if ! wait_for_log "edit: editing /edittest.txt" 30; then
    echo "run-edit-test: FAIL -- the editor did not start" >&2
    plain_log | grep -E "edit:|compositor:" | tail -20 >&2
    exit 1
fi
sleep 1

# Type two characters, save, and quit.
monitor "sendkey h"
monitor "sendkey i"
sleep 0.5
monitor "sendkey ctrl-s"
if ! wait_for_log "edit: saved" 20; then
    echo "run-edit-test: FAIL -- the editor never saved" >&2
    plain_log | grep -E "edit:" | tail -10 >&2
    exit 1
fi
monitor "sendkey ctrl-q"
sleep 1

if plain_log | grep -q "kernel panic"; then
    echo "run-edit-test: FAIL -- the kernel panicked" >&2
    exit 1
fi

shutdown_guest
sleep 0.5

CONTENT="$(mtype -i "${DISK}" ::/EDITTEST.TXT 2>/dev/null || true)"
case "${CONTENT}" in
    *hi*) echo "run-edit-test: mtools reads back what the editor saved ('${CONTENT}')" ;;
    "")   echo "run-edit-test: FAIL -- /EDITTEST.TXT is not on the disk" >&2; exit 1 ;;
    *)    echo "run-edit-test: FAIL -- /EDITTEST.TXT holds unexpected bytes: '${CONTENT}'" >&2; exit 1 ;;
esac

echo "run-edit-test: PASS"
