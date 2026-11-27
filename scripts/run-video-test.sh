#!/usr/bin/env bash
# Boot NodeOS and drive the video player: open the list, then play the
# homebrew demo with its controls, statistics and help up, pause it, and quit.
#
# The soundtrack goes through QEMU's `wav` backend, so "is there sound?" means
# "is the recording loud?". The picture is a screendump taken mid-video, while
# the demo's metaballs are on screen: a frame that came through has plenty of
# both colours. Screendumps of the list and the help panel are kept in build/
# for a human to look at.
set -uo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
NAME=run-video-test
WAV="${ROOT}/build/video-test.wav"
LOG="${ROOT}/build/video-test.log"
PORT="${PORT:-45481}"
AUDIODEV="wav,id=snd0,path=${WAV}"
ACCEL="${ACCEL:--accel kvm -accel tcg}"
source "${ROOT}/scripts/lib/guest.sh"

fail() {
    echo "${NAME}: FAIL -- $1" >&2
    [[ -f "${LOG}" ]] && plain_log | grep -iE "audio|video|fs:" | tail -15 >&2
    exit 1
}
shot() { monitor "screendump ${ROOT}/build/video-$1.ppm"; sleep 0.5; }

require_files "${ISO}" "${DISK}"
mdir -i "${DISK}" ::/VIDEOS/DEMO.BAP >/dev/null 2>&1 || fail "the disk has no /VIDEOS/DEMO.BAP; run 'make'"
rm -f "${WAV}" "${ROOT}"/build/video-*.ppm
boot_guest
wait_for_mount 20

# The list: it opens, finds the demo, and Esc leaves it.
type_line "video"
wait_for_log "video: [0-9]* video(s) in the list" 30 || fail "the list did not open"
plain_log | grep -q "video: 0 video(s)" && fail "the list found no videos"
sleep 1
shot list
monitor "sendkey esc"
wait_for_log "shell: completed /bin/video" 10 || fail "Esc did not leave the list"

# The demo, from the command line. Its metaballs run from 12 s to 24 s.
type_line "video /VIDEOS/DEMO.BAP"
wait_for_log "video: playing /VIDEOS/DEMO.BAP" 60 || fail "the demo did not start"
plain_log | grep -q "video: sound is on" || fail "no channel to the mixer"
sleep 14
monitor "sendkey i"; sleep 0.5
shot playing
monitor "sendkey up"; monitor "sendkey down"; monitor "sendkey right"; sleep 1
monitor "sendkey spc"; sleep 0.5; monitor "sendkey h"; sleep 0.5
shot help
monitor "sendkey esc"; sleep 0.3; monitor "sendkey spc"; sleep 2
monitor "sendkey esc"
wait_for_log "video: DEMO.BAP stopped (Back)" 10 || fail "Esc did not stop the video"
wait_for_log "shell: completed /bin/video" 10 || fail "the shell did not get its prompt back"
sleep 1
shutdown_guest

plain_log | grep -o "video: DEMO.BAP stopped.*"

python3 - "${WAV}" "${ROOT}/build/video-playing.ppm" <<'PY'
import array, sys, wave

with wave.open(sys.argv[1]) as wav:
    frames, rate = wav.getnframes(), wav.getframerate()
    raw = wav.readframes(frames)
samples = array.array('h')
samples.frombytes(raw[: len(raw) // 2 * 2])
seconds = frames / rate if rate else 0
loud = sum(1 for v in samples[::7] if abs(v) > 256) / max(1, len(samples[::7]))
peak = max((abs(v) for v in samples), default=0)
print(f"run-video-test: {seconds:.1f}s recorded, {100 * loud:.1f}% audible, peak {peak}")

data = open(sys.argv[2], 'rb').read()
fields, at = [], 0
while len(fields) < 4:  # P6 header: magic, width, height, maxval
    while data[at:at + 1].isspace():
        at += 1
    end = at
    while not data[end:end + 1].isspace():
        end += 1
    fields.append(data[at:end])
    at = end
pixels = data[at + 1:]
white = black = 0
for i in range(0, len(pixels) - 2, 15):
    level = pixels[i] + pixels[i + 1] + pixels[i + 2]
    white += level > 600
    black += level < 60
total = len(pixels) // 15
print(f"run-video-test: {int(fields[1])}x{int(fields[2])} screen, "
      f"{100 * white / total:.1f}% white, {100 * black / total:.1f}% black")

ok = True
if seconds < 10 or loud < 0.2 or peak < 2000:
    print("run-video-test: FAIL -- the soundtrack did not play", file=sys.stderr); ok = False
if white / total < 0.03 or black / total < 0.03:
    print("run-video-test: FAIL -- the screen does not look like a video frame", file=sys.stderr); ok = False
sys.exit(0 if ok else 1)
PY
[[ $? -eq 0 ]] || fail "see above"
echo "${NAME}: PASS (screens in build/video-{list,playing,help}.ppm)"
