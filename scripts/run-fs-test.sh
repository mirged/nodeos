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
NAME=run-fs-test
LOG="${ROOT}/build/fs-test.log"
PORT="${PORT:-45471}"
source "${ROOT}/scripts/lib/guest.sh"

require_files "${ISO}" "${DISK}"

# A file left behind by an earlier run would make the "the host can read it"
# check pass without NodeOS having written anything at all.
mdel -i "${DISK}" ::/SHTEST.TXT >/dev/null 2>&1 || true

boot_guest
wait_for_mount 15

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

# Either answer, waited for: the shell runs its own filesystem tests and reports
# the verdict in the log. Waiting for the verdict rather than for the shell to
# have had time to reach one is the difference between a test that measures the
# filesystem and one that measures the host's load average.
if wait_for_log "fstest: PASS" 25 || wait_for_log "fstest: FAIL" 1; then
    if plain_log | grep -q "fstest: FAIL"; then
        echo "run-fs-test: FAIL -- the shell's self-test reported failures" >&2
        plain_log | grep "fstest:" >&2
        exit 1
    fi
    echo "run-fs-test: the shell's self-test passed"
else
    echo "run-fs-test: FAIL -- the self-test never reported" >&2
    plain_log | grep -i "fstest\|fs:" | tail -20 >&2
    exit 1
fi

# What a `run` leaves behind is not the shell saying "process started" -- the
# shell writes that to the console, which draws it on a screen this test does
# not have. The serial log carries what clients *log*, so the evidence is the
# child's own output under the path it was launched from, and the kernel
# reporting that the process it created exited cleanly.
if wait_for_log "\[/bin/init.nxf\] init: userspace is alive" 20 &&
    wait_for_log "process [0-9]* (/bin/init.nxf) terminated with code 0" 20; then
    echo "run-fs-test: shell launched /bin/init.nxf as a child process"
else
    echo "run-fs-test: FAIL -- run did not start /bin/init.nxf" >&2
    plain_log | grep -E "init.nxf|process started" | tail -10 >&2
    exit 1
fi

shutdown_guest
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
