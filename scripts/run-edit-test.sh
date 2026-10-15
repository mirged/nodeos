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
ISO="${ISO:-${ROOT}/build/nodeos.iso}"
DISK="${DISK:-${ROOT}/build/disk.img}"
LOG="${ROOT}/build/edit-test.log"
PORT="${PORT:-45479}"
BOOTWAIT="${BOOTWAIT:-14}"

for required in "${ISO}" "${DISK}"; do
    [[ -f "${required}" ]] || { echo "run-edit-test: ${required} not found; run 'make'" >&2; exit 1; }
done

# A file left over from an earlier run would make the final check pass without
# the editor having written anything.
mdel -i "${DISK}" ::/EDITTEST.TXT >/dev/null 2>&1 || true

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

echo "run-edit-test: booting ${ISO}"
sleep "${BOOTWAIT}"
wait_for_log "fs: FAT16 mounted\|fs: FAT32 mounted" 20 || {
    echo "run-edit-test: FAIL -- the filesystem service did not mount the disk" >&2
    tail -20 "${LOG}" >&2
    exit 1
}

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

cleanup
trap - EXIT
sleep 0.5

CONTENT="$(mtype -i "${DISK}" ::/EDITTEST.TXT 2>/dev/null || true)"
case "${CONTENT}" in
    *hi*) echo "run-edit-test: mtools reads back what the editor saved ('${CONTENT}')" ;;
    "")   echo "run-edit-test: FAIL -- /EDITTEST.TXT is not on the disk" >&2; exit 1 ;;
    *)    echo "run-edit-test: FAIL -- /EDITTEST.TXT holds unexpected bytes: '${CONTENT}'" >&2; exit 1 ;;
esac

echo "run-edit-test: PASS"
