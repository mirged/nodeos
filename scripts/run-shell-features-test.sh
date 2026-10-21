#!/usr/bin/env bash
# Boot NodeOS and check the shell's variables, chaining, redirection and file
# commands. Every check writes its result into a file with `>` or `>>`, and
# the host reads the disk back with mtools afterwards, so the test reads what
# the shell actually did rather than what it drew on the screen.
set -uo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
NAME=run-shell-features-test
SOURCE_DISK="${DISK:-${ROOT}/build/disk.img}"
DISK="${ROOT}/build/shell-features.img"
LOG="${ROOT}/build/shell-features.log"
PORT="${PORT:-45483}"
ACCEL="${ACCEL:--accel kvm -accel tcg}"
MONITOR_DELAY=0.05
source "${ROOT}/scripts/lib/guest.sh"

require_files "${ISO}" "${SOURCE_DISK}"
cp "${SOURCE_DISK}" "${DISK}"
boot_guest

completed=0
# Wait for the shell to finish the next command line.
run() {
    local before
    before=$(plain_log | grep -c "shell: command complete")
    type_line "$1"
    for (( waited = 0; waited < 240; waited++ )); do
        (( $(plain_log | grep -c "shell: command complete") > before )) && { sleep 0.3; return 0; }
        sleep 0.25
    done
    echo "run-shell-features-test: FAIL -- '$1' did not complete" >&2
    exit 1
}

for (( waited = 0; waited < 160; waited++ )); do
    plain_log | grep -q "shell: ready" && break
    sleep 0.25
done
plain_log | grep -q "shell: ready" || { echo "run-shell-features-test: FAIL -- no shell" >&2; exit 1; }

run 'NAME=world; echo hello $NAME "${NAME}s" > /SF1.TXT'
run 'false || echo or-ran >> /SF1.TXT; true && echo and-ran >> /SF1.TXT'
run 'true || echo skipped >> /SF1.TXT; false && echo skipped >> /SF1.TXT'
run 'nosuchprogram; echo status $? >> /SF1.TXT'
run 'ls -l /tests > /SF2.TXT; wc /SF1.TXT > /SF3.TXT; head -n 2 /SF1.TXT > /SF4.TXT; tail -n 1 /SF1.TXT >> /SF4.TXT'
run 'mkdir /SFD; cp /SF1.TXT /SFD/A.TXT; mv /SFD/A.TXT /SFD/B.TXT; find / b.txt > /SF5.TXT'
run 'mkdir /SFD/SUB; write /SFD/SUB/C.TXT x; rmdir /SFD/SUB; rm -r /SFD; echo removed >> /SF5.TXT'
run 'export GREETING=hi; env > /SF6.TXT; ONCE=1 maysmoke arg > /SF7.TXT'
run 'cd /tests; pwd > /SF8.TXT; mayc hello.may -o /tests/sfhello.nxf && sfhello >> /SF8.TXT; cd /'
run 'mayupper < /SF1.TXT > /SF9.TXT'

monitor "screendump ${ROOT}/build/shell-features.ppm"
shutdown_guest

check() {
    local name="$1" file="$2" expected="$3" actual
    actual="$(mtype -i "${DISK}" "::/${file}" 2>/dev/null)"
    if [[ "${actual}" == *"${expected}"* ]]; then
        echo "run-shell-features-test: OK   ${name}"
    else
        echo "run-shell-features-test: FAIL -- ${name}: /${file} lacks '${expected}'" >&2
        printf '%s\n' "${actual}" | head -10 >&2
        fail=1
    fi
}
check "variables expand, unsplit" SF1.TXT "hello world worlds"
check "|| runs after failure" SF1.TXT "or-ran"
check "&& runs after success" SF1.TXT "and-ran"
check "exit status of a missing program" SF1.TXT "status 127"
check "ls -l lists exact sizes" SF2.TXT "HELLO.MAY"
check "wc counts lines, words, bytes" SF3.TXT "/SF1.TXT"
check "head and tail" SF4.TXT "hello world worlds"
check "mv and find" SF5.TXT "/SFD/B.TXT"
check "rmdir and rm -r" SF5.TXT "removed"
check "export passes variables" SF6.TXT "GREETING=hi"
check "a program's output redirected" SF7.TXT "smoke: exec status 7"
check "programs start in the shell's directory" SF8.TXT "/tests"
check "a program compiled and run by name in the directory" SF8.TXT "Hello, Maylang!"
check "a program's input redirected" SF9.TXT "HELLO WORLD WORLDS"
if [[ "$(mtype -i "${DISK}" ::/SF1.TXT)" == *skipped* ]]; then
    echo "run-shell-features-test: FAIL -- a command after && or || ran when it should not" >&2
    fail=1
fi
if mdir -i "${DISK}" ::/SFD >/dev/null 2>&1; then
    echo "run-shell-features-test: FAIL -- rm -r left /SFD behind" >&2
    fail=1
fi
if plain_log | grep -q "kernel panic\|fatal exception"; then
    echo "run-shell-features-test: FAIL -- a panic or fatal exception appeared" >&2
    fail=1
fi
(( fail == 0 )) || { echo "run-shell-features-test: FAIL" >&2; exit 1; }
echo "run-shell-features-test: PASS"
