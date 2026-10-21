#!/usr/bin/env bash
# Boot NodeOS, start Doom from the shell, and check that sound reached the card.
#
# QEMU's `wav` audio backend writes everything the AC'97 played to a file, so
# the question "does the driver work?" becomes a question about bytes: how much
# of the recording is not silence, and how loud is it. Every stage has to have
# worked for a non-zero sample to exist at all: the shell launching Doom, the
# filesystem handing it the IWAD, the mixer, the DMX decode, and Doom's own
# decision to make a noise.
set -uo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
NAME=run-audio-test
WAV="${ROOT}/build/audio-test.wav"
LOG="${ROOT}/build/audio-test.log"
PORT="${PORT:-45475}"
# QEMU's `wav` backend records everything the card played.
AUDIODEV="wav,id=snd0,path=${WAV}"
# How long Doom is given to play after it reports sound is on.
PLAY_SECONDS="${PLAY_SECONDS:-20}"
source "${ROOT}/scripts/lib/guest.sh"

fail() {
    echo "${NAME}: FAIL -- $1" >&2
    [[ -f "${LOG}" ]] && plain_log | grep -iE "audio|doom:|fs:" | tail -15 >&2
    exit 1
}

require_files "${ISO}" "${DISK}"
rm -f "${WAV}"
boot_guest
wait_for_mount 20

# Doom is an ordinary disk program now: type its name at the shell.
type_line "run /bin/doom.nxf"
wait_for_log "doom: sound is on" 90 || fail "Doom did not get a channel to the mixer"

sleep "${PLAY_SECONDS}"
shutdown_guest

grep -q "audio: .* Hz stereo" "${LOG}" || fail "the audio service never initialised the card"
[[ -s "${WAV}" ]] || fail "QEMU recorded nothing at all"

python3 - "${WAV}" <<'PY'
import array, sys, wave

with wave.open(sys.argv[1]) as wav:
    frames = wav.getnframes()
    rate = wav.getframerate()
    raw = wav.readframes(frames)

samples = array.array('h')
samples.frombytes(raw[: len(raw) // 2 * 2])
if not samples:
    print("run-audio-test: FAIL -- the recording holds no samples", file=sys.stderr)
    raise SystemExit(1)

seconds = frames / rate
loud = sum(1 for value in samples if abs(value) > 64)
peak = max(abs(value) for value in samples)
energy = (sum((value / 32768) ** 2 for value in samples[::17]) / len(samples[::17])) ** 0.5

print(f"run-audio-test: {seconds:.1f}s recorded, "
      f"{100 * loud / len(samples):.1f}% above the noise floor, peak {peak}, rms {energy:.3f}")

# Thresholds chosen with a lot of room underneath: this is here to catch
# silence and a dead ring, not to grade the mix. The recording includes the
# silence of boot and of Doom loading its IWAD, so the audible fraction is
# lower than it was when Doom was a boot module.
if seconds < 5:
    print("run-audio-test: FAIL -- the card stopped playing almost immediately "
          "(a stalled DMA ring looks exactly like this)", file=sys.stderr)
    raise SystemExit(1)
if loud / len(samples) < 0.03:
    print("run-audio-test: FAIL -- the recording is essentially silent", file=sys.stderr)
    raise SystemExit(1)
if peak < 1000:
    print("run-audio-test: FAIL -- samples reached the card but at nothing like a "
          "usable level", file=sys.stderr)
    raise SystemExit(1)
PY
STATUS=$?
[[ ${STATUS} -eq 0 ]] || exit ${STATUS}

if grep -q "underrun" "${LOG}"; then
    grep -o "[0-9]* buffers mixed, [0-9]* underrun(s)" "${LOG}" | tail -1
fi
echo "run-audio-test: PASS"
