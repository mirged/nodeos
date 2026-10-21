# Shared by the interactive QEMU tests: boot NodeOS with a monitor port, type
# at its shell, and read what it reports on the serial log.
#
# Source it after setting:
#   NAME      prefix for messages, e.g. run-cc-test
#   LOG       serial log path
#   PORT      monitor TCP port; give each test its own so they can run at once
# and optionally ISO, DISK, MEMORY, ACCEL (a list of options), AUDIODEV,
# BOOTWAIT and MONITOR_DELAY.

ISO="${ISO:-${ROOT}/build/nodeos.iso}"
DISK="${DISK:-${ROOT}/build/disk.img}"
MEMORY="${MEMORY:-512M}"
ACCEL="${ACCEL:-}"
AUDIODEV="${AUDIODEV:-none,id=snd0}"
BOOTWAIT="${BOOTWAIT:-14}"
MONITOR_DELAY="${MONITOR_DELAY:-0.15}"

require_files() {
    local required
    for required in "$@"; do
        [[ -f "${required}" ]] || { echo "${NAME}: ${required} not found; run 'make'" >&2; exit 1; }
    done
}

boot_guest() {
    rm -f "${LOG}"
    # shellcheck disable=SC2086 # ACCEL is a list of options
    qemu-system-x86_64 ${ACCEL} \
        -machine q35 -cpu qemu64 -smp 1 -m "${MEMORY}" -boot d -no-reboot \
        -cdrom "${ISO}" -serial "file:${LOG}" -display none \
        -audiodev "${AUDIODEV}" -device AC97,audiodev=snd0 \
        -drive file="${DISK}",if=virtio,format=raw \
        -monitor tcp:127.0.0.1:${PORT},server,nowait >/dev/null 2>&1 &
    QEMU_PID=$!
    trap cleanup EXIT
}

cleanup() { kill "${QEMU_PID}" 2>/dev/null; wait "${QEMU_PID}" 2>/dev/null; }

# Stop the guest before reading its disk from the host.
shutdown_guest() {
    cleanup
    trap - EXIT
}

monitor() {
    exec 3<>"/dev/tcp/127.0.0.1/${PORT}" || return 1
    printf '%s\n' "$@" >&3
    sleep "${MONITOR_DELAY}"
    exec 3<&- 3>&-
}

# QEMU's `sendkey` names keys, not characters, so punctuation and capitals are
# spelled out as the keys (and shift) that produce them.
type_line() {
    local text="$1" char key i
    for (( i = 0; i < ${#text}; i++ )); do
        char="${text:i:1}"
        case "${char}" in
            " ") key=spc ;; "/") key=slash ;; ".") key=dot ;; "-") key=minus ;; "=") key=equal ;;
            ",") key=comma ;; ";") key=semicolon ;; "_") key=shift-minus ;; "\$") key=shift-4 ;;
            "&") key=shift-7 ;; "*") key=shift-8 ;; "(") key=shift-9 ;; ")") key=shift-0 ;;
            "|") key=shift-backslash ;; ">") key=shift-dot ;; "<") key=shift-comma ;;
            "?") key=shift-slash ;; "\"") key=shift-apostrophe ;;
            "{") key=shift-bracket_left ;; "}") key=shift-bracket_right ;;
            [A-Z]) key="shift-${char,,}" ;;
            *) key="${char}" ;;
        esac
        monitor "sendkey ${key}"
    done
    monitor "sendkey ret"
}

# The log carries colour escapes, and they sit *inside* the module tags:
# `ESC[35m[/bin/init.nxf]ESC[0m init: ...`. A pattern that includes a tag -- the
# only way to tell the shell's own output apart from a child it launched -- has
# to be matched with the escapes taken out, or it silently never matches and
# the test blames the guest.
plain_log() { sed -e 's/\x1b\[[0-9;]*m//g' -e 's/\r$//' "${LOG}"; }

# A line in the serial log, waited for. Everything a test asks the guest to do
# happens asynchronously -- it types at a machine running at its own speed -- so
# a check that sleeps for a fixed time and looks once is a race that passes on
# a fast host and fails, misleadingly, on a slow one.
wait_for_log() {
    local pattern="$1" seconds="$2" waited=0
    while (( waited < seconds * 4 )); do
        plain_log | grep -q "${pattern}" && return 0
        sleep 0.25
        waited=$(( waited + 1 ))
    done
    return 1
}

# Give the guest its boot time, then wait for the filesystem service.
wait_for_mount() {
    echo "${NAME}: booting ${ISO}"
    sleep "${BOOTWAIT}"
    wait_for_log "fs: FAT16 mounted\|fs: FAT32 mounted" "$1" && return 0
    echo "${NAME}: FAIL -- the filesystem service did not mount the disk" >&2
    [[ -s "${LOG}" ]] || echo "${NAME}: the serial log is empty; QEMU produced nothing at all" >&2
    tail -20 "${LOG}" >&2
    exit 1
}

# A program's output line, as mirrored into the log under EXPECT_PREFIX.
fail=0
expect() {
    if plain_log | grep -qF "${EXPECT_PREFIX}$1"; then
        echo "${NAME}: OK   $1"
    else
        echo "${NAME}: FAIL -- missing output line: $1" >&2
        fail=1
    fi
}
