#!/usr/bin/env bash
# Boot the graphic image, run the console apps from the shell, and check that
# each one reached its final line without the memory test reporting corruption.
#
# The apps are ordinary console clients: the shell reads each NXF off the disk,
# starts it, and hands it the console and filesystem channels. Reaching
# "sysinfo: done" means the console, filesystem and memory capabilities all
# worked end to end.
set -uo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
ISO="${ISO:-${ROOT}/build/nodeos.iso}"
DISK="${DISK:-${ROOT}/build/disk.img}"
LOG="${ROOT}/build/apps-test.log"
PORT="${PORT:-45473}"
BOOTWAIT="${BOOTWAIT:-14}"

for required in "${ISO}" "${DISK}"; do
    [[ -f "${required}" ]] || { echo "run-apps-test: ${required} not found; run 'make'" >&2; exit 1; }
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
            "-") monitor "sendkey minus" ;;
            *)   monitor "sendkey ${char}" ;;
        esac
    done
    monitor "sendkey ret"
}

plain_log() {
    sed -e 's/\x1b\[[0-9;]*m//g' "${LOG}"
}

wait_for_log() {
    local pattern="$1" seconds="${2:-20}" waited=0
    while (( waited < seconds * 4 )); do
        plain_log | grep -q "${pattern}" && return 0
        sleep 0.25
        waited=$(( waited + 1 ))
    done
    return 1
}

echo "run-apps-test: booting ${ISO}"
sleep "${BOOTWAIT}"

if ! wait_for_log "fs: FAT16 mounted\|fs: FAT32 mounted"; then
    echo "run-apps-test: FAIL -- the filesystem service did not mount the disk" >&2
    tail -20 "${LOG}" >&2
    exit 1
fi

fail=0
run_app() {
    local command="$1" marker="$2" label="$3"
    type_line "${command}"
    if wait_for_log "${marker}" 25; then
        echo "run-apps-test: OK   ${label} (${marker})"
    else
        echo "run-apps-test: FAIL -- ${label} never reported '${marker}'" >&2
        fail=1
    fi
    sleep 1
}

run_app "run /bin/sysinfo.nxf" "sysinfo: done" "sysinfo completed"
run_app "run /bin/tree.nxf"    "tree: [0-9]* director" "tree walked the filesystem"
run_app "run /bin/bench.nxf"   "bench: done" "bench completed"

if plain_log | grep -q "CORRUPT"; then
    echo "run-apps-test: FAIL -- a memory object read back corrupt" >&2
    fail=1
fi
if plain_log | grep -q "kernel panic\|SELFTEST FAILED"; then
    echo "run-apps-test: FAIL -- the kernel panicked" >&2
    fail=1
fi

cleanup
trap - EXIT

if (( fail != 0 )); then
    echo "run-apps-test: FAIL" >&2
    plain_log | grep -E "sysinfo|tree:|bench:|panic|CORRUPT" | tail -20 >&2
    exit 1
fi
echo "run-apps-test: PASS"
