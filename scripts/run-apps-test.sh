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
NAME=run-apps-test
LOG="${ROOT}/build/apps-test.log"
PORT="${PORT:-45473}"
source "${ROOT}/scripts/lib/guest.sh"

require_files "${ISO}" "${DISK}"
boot_guest
wait_for_mount 20

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

shutdown_guest

if (( fail != 0 )); then
    echo "run-apps-test: FAIL" >&2
    plain_log | grep -E "sysinfo|tree:|bench:|panic|CORRUPT" | tail -20 >&2
    exit 1
fi
echo "run-apps-test: PASS"
