#!/usr/bin/env bash
# Boot NodeOS, type `fstest` at the shell, and check both halves of the answer:
# what the shell reported, and what the host can read back off the same disk.
#
# The second half is the point. A filesystem that agrees with itself proves
# nothing -- it could be storing files in a format only it will ever read. This
# script runs mtools against the image afterwards and asserts that the file the
# shell wrote is there, with the bytes it said it wrote.
set -uo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
ISO="${ISO:-${ROOT}/build/nodeos-graphic.iso}"
DISK="${DISK:-${ROOT}/build/disk.img}"
LOG="${ROOT}/build/fs-test.log"
PORT="${PORT:-45471}"
BOOTWAIT="${BOOTWAIT:-14}"

for required in "${ISO}" "${DISK}"; do
    [[ -f "${required}" ]] || { echo "run-fs-test: ${required} not found; run 'make iso-graphic disk'" >&2; exit 1; }
done

# A file left behind by an earlier run would make the "the host can read it"
# check pass without NodeOS having written anything at all.
mdel -i "${DISK}" ::/SHTEST.TXT >/dev/null 2>&1 || true

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

echo "run-fs-test: booting ${ISO}"
sleep "${BOOTWAIT}"

grep -q "fs: FAT16 mounted\|fs: FAT32 mounted" "${LOG}" || {
    echo "run-fs-test: FAIL -- the filesystem service did not mount the disk" >&2
    tail -20 "${LOG}" >&2
    exit 1
}

type_line "fstest"
sleep 6
# The shell writes one more file after the self-test so the host has something
# with known contents to find.
# Lower case throughout: QEMU's `sendkey` names keys, not characters, and
# there is no key called "H". FAT upper-cases what it stores anyway, which
# is why the host looks for HOSTCHK.TXT below.
type_line "write /hostchk.txt hello from nodeos"
sleep 3
type_line "run /bin/init.nxf"
sleep 3

if grep -q "fstest: PASS" "${LOG}"; then
    echo "run-fs-test: the shell's self-test passed"
elif grep -q "fstest: FAIL" "${LOG}"; then
    echo "run-fs-test: FAIL -- the shell's self-test reported failures" >&2
    grep "fstest:" "${LOG}" >&2
    exit 1
else
    echo "run-fs-test: FAIL -- the self-test never reported" >&2
    grep -i "fstest\|fs:" "${LOG}" | tail -20 >&2
    exit 1
fi

if grep -q "process started" "${LOG}" && grep -q "init: userspace is alive" "${LOG}"; then
    echo "run-fs-test: shell launched /bin/init.nxf as a child process"
else
    echo "run-fs-test: FAIL -- run did not start /bin/init.nxf" >&2
    grep -E "run:|process started|init: userspace" "${LOG}" >&2 || true
    exit 1
fi

cleanup
trap - EXIT
sleep 0.5

# --- The other half: read the same disk with somebody else's tools -----------
CONTENTS="$(mtype -i "${DISK}" ::/SHTEST.TXT 2>/dev/null || true)"
case "${CONTENTS}" in
    *"nodeos wrote this"*"and this"*)
        echo "run-fs-test: mtools reads back what NodeOS wrote" ;;
    "")
        echo "run-fs-test: FAIL -- /SHTEST.TXT is not on the disk the host can see" >&2
        exit 1 ;;
    *)
        echo "run-fs-test: FAIL -- /SHTEST.TXT holds unexpected bytes:" >&2
        echo "${CONTENTS}" >&2
        exit 1 ;;
esac

HOSTCHK="$(mtype -i "${DISK}" ::/HOSTCHK.TXT 2>/dev/null || true)"
case "${HOSTCHK}" in
    *"hello from nodeos"*) echo "run-fs-test: a file typed at the shell is readable on the host" ;;
    *) echo "run-fs-test: FAIL -- /HOSTCHK.TXT missing or wrong: '${HOSTCHK}'" >&2; exit 1 ;;
esac

# And the file the *host* put there before the boot is still intact, which is
# what says NodeOS's writes did not corrupt the allocation table.
if mdir -i "${DISK}" ::/ 2>/dev/null | grep -q "README"; then
    echo "run-fs-test: the host's own files survived"
else
    echo "run-fs-test: FAIL -- README.TXT vanished; the FAT is damaged" >&2
    exit 1
fi

echo "run-fs-test: PASS"
