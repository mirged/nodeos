#!/usr/bin/env bash
# Boot Doom and check that sound actually reached the sound card.
#
# QEMU's `wav` audio backend writes everything the AC'97 played to a file, so
# the question "does the driver work?" becomes a question about bytes: how much
# of the recording is not silence, and how loud is it. That is a weaker claim
# than "it sounds right" and a much stronger one than "it did not crash" --
# every stage has to have worked for a non-zero sample to exist at all: the
# DMA ring, the mixer, the DMX decode, the protocol, and Doom's own decision to
# make a noise.
set -uo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
ISO="${ISO:-${ROOT}/build/nodeos-doom.iso}"
DISK="${DISK:-${ROOT}/build/disk.img}"
WAV="${ROOT}/build/audio-test.wav"
LOG="${ROOT}/build/audio-test.log"
SECONDS_TO_RUN="${SECONDS_TO_RUN:-40}"

[[ -f "${ISO}" ]] || { echo "run-audio-test: ${ISO} not found; run 'make iso-doom'" >&2; exit 1; }
[[ -f "${DISK}" ]] || { echo "run-audio-test: ${DISK} not found; run 'make disk'" >&2; exit 1; }

rm -f "${WAV}" "${LOG}"
echo "run-audio-test: booting ${ISO} for ${SECONDS_TO_RUN}s with the wav backend"
timeout "${SECONDS_TO_RUN}" qemu-system-x86_64 \
    -machine q35 -cpu qemu64 -smp 1 -m 512M -boot d -no-reboot \
    -cdrom "${ISO}" -serial "file:${LOG}" -display none \
    -audiodev wav,id=snd0,path="${WAV}" -device AC97,audiodev=snd0 \
    -drive file="${DISK}",if=virtio,format=raw >/dev/null 2>&1

fail() { echo "run-audio-test: FAIL -- $1" >&2; [[ -f "${LOG}" ]] && grep -i "audio\|doom: sound" "${LOG}" | tail -10 >&2; exit 1; }

grep -q "audio: .* Hz stereo" "${LOG}" || fail "the audio service never initialised the card"
grep -q "doom: sound is on" "${LOG}" || fail "Doom did not get a channel to the mixer"
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

# Thresholds chosen against a healthy run (about 90% audible, rms 0.14) with a
# lot of room underneath: this is here to catch silence and a dead ring, not to
# grade the mix.
if seconds < 5:
    print("run-audio-test: FAIL -- the card stopped playing almost immediately "
          "(a stalled DMA ring looks exactly like this)", file=sys.stderr)
    raise SystemExit(1)
if loud / len(samples) < 0.05:
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
