#!/usr/bin/env bash
# Hostile-userspace stress suite: boot NodeOS once per case, run
# `/bin/stress.nxf <case>` from the shell, and decide from the serial log
# whether the kernel held up.
#
#   tests/stress/run.sh                 every case
#   tests/stress/run.sh case1 case2     a subset
#   tests/stress/run.sh --list          names and descriptions
#
# Verdicts:
#   PASS   the kernel refused the operation or survived it, and the system
#          still answers a liveness probe afterwards
#   FAIL   the weakness reproduced: a kernel panic, the guest dying, a hang,
#          or the case itself detecting a bypass
#   ERROR  the case could not set itself up, or the harness could not drive it
#
# Safety: everything runs inside QEMU. Each boot uses a scratch copy of
# build/disk.img (build/stress/disk.img), so the real disk image is never
# written to, and the guest's only network is QEMU user-mode NAT.
#
# Environment: ACCEL (default: KVM when /dev/kvm is usable), MEMORY (512M; the
# quota and leak cases assume 512M), CASE_TIMEOUT, BOOTWAIT, PORT (first of a
# range).
set -uo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
NAME=stress
OUT="${ROOT}/build/stress"
STRESS_NXF="${OUT}/stress.nxf"
BASE_DISK="${ROOT}/build/disk.img"
BASE_PORT="${PORT:-45600}"
CASE_TIMEOUT="${CASE_TIMEOUT:-150}"
LIVE_TIMEOUT="${LIVE_TIMEOUT:-40}"
if [[ -z "${ACCEL+x}" ]]; then
    if [[ -r /dev/kvm && -w /dev/kvm ]]; then ACCEL="-accel kvm -accel tcg"; else ACCEL=""; fi
fi
export ACCEL

ALL_CASES=(
    overflow-transfer overflow-map-len overflow-map-hint overflow-fault-offset
    chan-nest-drop chan-self-cycle
    thread-noncanon-rip thread-noncanon-rsp
    mem-commit-quota vmo-metadata-quota heap-exhaust-crash cpu-interactive-starve cpu-rt-unbounded
    caps-undeclared rights-map-noread-protect rights-map-noread-direct
)

# Cases that run longer than the default.
case_timeout() {
    case "$1" in
        chan-self-cycle|heap-exhaust-crash) echo $(( CASE_TIMEOUT * 3 )) ;;
        *) echo "${CASE_TIMEOUT}" ;;
    esac
}

if [[ "${1:-}" == "--list" ]]; then
    sed -n '/^const CASES/,/^];/p' "${ROOT}/tests/stress/guest/src/main.rs" \
        | grep -o '("[a-z0-9-]*", "[^"]*", "[^"]*"' \
        | sed -E 's/\("([^"]*)", "([^"]*)", "([^"]*)"/  \1  [\2]  \3/' | grep -v '  alive  '
    exit 0
fi

CASES=("$@")
(( ${#CASES[@]} == 0 )) && CASES=("${ALL_CASES[@]}")

for required in "${ROOT}/build/nodeos.iso" "${BASE_DISK}" "${STRESS_NXF}"; do
    [[ -f "${required}" ]] || { echo "stress: ${required} not found; run 'make test-stress' (or 'make && make stress')" >&2; exit 2; }
done
command -v mcopy >/dev/null || { echo "stress: mcopy (mtools) is required" >&2; exit 2; }

mkdir -p "${OUT}"
export DISK="${OUT}/disk.img"
export ISO="${ROOT}/build/nodeos.iso"
# guest.sh reads LOG and PORT when its functions run, so they are set per case.
source "${ROOT}/scripts/lib/guest.sh"

guest_alive() { kill -0 "${QEMU_PID}" 2>/dev/null; }

# First line number in the plain log matching a fixed string, or empty.
line_of() { plain_log | grep -nF -m1 -- "$1" | cut -d: -f1; }

panic_summary() {
    plain_log | grep -A3 "NodeOS kernel panic" | grep -E "^  " | head -2 | sed 's/^ *//' | paste -sd' ' -
}

# Sets VERDICT and DETAIL for the case running in the current guest.
# Returns 0 if the guest is still worth probing for liveness.
await_case() {
    local case="$1" limit result begin expect fault waited=0
    limit=$(( $(case_timeout "${case}") * 4 ))
    while (( waited < limit )); do
        if plain_log | grep -q "NodeOS kernel panic"; then
            VERDICT=FAIL; DETAIL="kernel panic: $(panic_summary)"; return 1
        fi
        result="$(plain_log | grep -o "STRESS-RESULT ${case} .*" | head -1)"
        if [[ -n "${result}" ]]; then
            VERDICT="$(awk '{print $3}' <<<"${result}")"
            DETAIL="$(cut -d' ' -f4- <<<"${result}")"
            return 0
        fi
        expect="$(line_of "STRESS-EXPECT-FAULT ${case}")"
        fault="$(line_of "fatal exception in user mode")"
        if [[ -n "${fault}" ]]; then
            begin="$(line_of "STRESS-BEGIN ${case}")"
            if [[ -n "${expect}" ]] && (( fault > expect )); then
                VERDICT=PASS
                DETAIL="kernel killed the process with a user-mode fault, the correct outcome"
            elif [[ -n "${begin}" ]] && (( fault > begin )); then
                VERDICT=ERROR; DETAIL="the case faulted during setup"
            else
                VERDICT=ERROR; DETAIL="a user-mode fault before the case started"
            fi
            return 0
        fi
        if ! guest_alive; then
            VERDICT=FAIL; DETAIL="QEMU exited mid-case (triple fault or reset)"; return 1
        fi
        sleep 0.25
        waited=$(( waited + 1 ))
    done
    if [[ -z "$(line_of "STRESS-BEGIN ${case}")" ]]; then
        VERDICT=ERROR; DETAIL="the case never started (did the shell get the command?)"
    else
        VERDICT=FAIL; DETAIL="no verdict within $(case_timeout "${case}")s: the kernel hung or the system froze"
    fi
    return 1
}

run_case() {
    local case="$1" index="$2"
    export LOG="${OUT}/${case}.log"
    export PORT=$(( BASE_PORT + index ))
    cp "${BASE_DISK}" "${DISK}"
    mcopy -o -i "${DISK}" "${STRESS_NXF}" ::/bin/stress.nxf

    local started=${SECONDS}
    echo "stress: [${case}] booting QEMU (log: build/stress/${case}.log)"
    boot_guest
    sleep "${BOOTWAIT}"
    echo "stress: [${case}]   waiting for the disk to mount..."
    if ! wait_for_log "fs: FAT16 mounted\|fs: FAT32 mounted" 40; then
        VERDICT=ERROR; DETAIL="the guest never mounted its disk"
        shutdown_guest
        return
    fi
    sleep 1
    echo "stress: [${case}]   typing 'run /bin/stress.nxf ${case}' at the shell"
    type_line "run /bin/stress.nxf ${case}"
    echo "stress: [${case}]   waiting for a verdict (up to $(case_timeout "${case}")s)"

    if await_case "${case}"; then
        # The case returned; make sure it did not leave the machine wedged.
        sleep 1
        echo "stress: [${case}]   case returned ${VERDICT}; checking the system is still alive"
        type_line "run /bin/stress.nxf alive"
        if ! wait_for_log "STRESS-RESULT alive PASS" "${LIVE_TIMEOUT}"; then
            if plain_log | grep -q "NodeOS kernel panic"; then
                VERDICT=FAIL; DETAIL="kernel panicked after the case returned: $(panic_summary)"
            else
                VERDICT=FAIL; DETAIL="system unresponsive after the case (${VERDICT}: ${DETAIL})"
            fi
        fi
    fi
    shutdown_guest
    echo "stress: [${case}]   done in $(( SECONDS - started ))s"
}

declare -A VERDICTS DETAILS
echo "stress: ${#CASES[@]} case(s), one QEMU boot each (~30-60 s per case; chan-self-cycle and heap-exhaust-crash take longer)"
index=0
for case in "${CASES[@]}"; do
    if [[ ! " ${ALL_CASES[*]} " == *" ${case} "* ]]; then
        echo "stress: unknown case '${case}' (see --list)" >&2
        VERDICTS[${case}]=ERROR; DETAILS[${case}]="unknown case"
        continue
    fi
    VERDICT=ERROR; DETAIL="not run"
    run_case "${case}" "${index}"
    VERDICTS[${case}]="${VERDICT}"; DETAILS[${case}]="${DETAIL}"
    printf 'stress: [%s] %s -- %s\n' "${case}" "${VERDICT}" "${DETAIL}"
    index=$(( index + 1 ))
done

echo
echo "================ stress summary ================"
pass=0; failed=0; errors=0
for case in "${CASES[@]}"; do
    v="${VERDICTS[${case}]}"
    printf '  %-5s  %-26s %s\n' "${v}" "${case}" "${DETAILS[${case}]}"
    case "${v}" in PASS) pass=$(( pass + 1 )) ;; FAIL) failed=$(( failed + 1 )) ;; *) errors=$(( errors + 1 )) ;; esac
done
echo "  ${pass} passed, ${failed} failed (vulnerable), ${errors} errors -- logs in build/stress/<case>.log"

(( failed == 0 && errors == 0 ))
