#!/usr/bin/env bash
# Convert any video ffmpeg can read into a .BAP file for NodeOS's `video`.
#
#   user/video/tools/encode.sh [options] INPUT OUTPUT.BAP
#
#   --size WxH     picture size (default 480x360; keep it near the source's
#                  aspect ratio, the player letterboxes, it does not crop)
#   --fps N        frame rate (default 30)
#   --rate HZ      sound sample rate (default 48000, the mixer's own)
#   --dither       Bayer-dither the picture instead of a hard threshold:
#                  better for live action, worse for silhouettes and cartoons
#   --start T      start at time T (ffmpeg syntax, e.g. 1:30)
#   --length T     encode only T of it
#
# FAT keeps 8.3 names, so call the output something like CLIP.BAP. Put it in
# videos/ at the top of the tree and `make` copies it to /VIDEOS on the disk.
set -euo pipefail

usage() { sed -n '2,17p' "$0" | sed 's/^# \{0,1\}//'; exit "${1:-2}"; }

SIZE=480x360 FPS=30 RATE=48000 DITHER=0 SEEK=() LENGTH=()
while [[ $# -gt 0 && "$1" == --* ]]; do
    case "$1" in
        --size) SIZE="$2"; shift 2 ;;
        --fps) FPS="$2"; shift 2 ;;
        --rate) RATE="$2"; shift 2 ;;
        --dither) DITHER=1; shift ;;
        --start) SEEK=(-ss "$2"); shift 2 ;;
        --length) LENGTH=(-t "$2"); shift 2 ;;
        --help) usage 0 ;;
        *) echo "encode.sh: unknown option $1" >&2; usage ;;
    esac
done
[[ $# -eq 2 ]] || usage
INPUT="$1" OUTPUT="$2"
[[ "${SIZE}" =~ ^([0-9]+)x([0-9]+)$ ]] || { echo "encode.sh: --size wants WxH" >&2; exit 2; }
W="${BASH_REMATCH[1]}" H="${BASH_REMATCH[2]}"

command -v ffmpeg >/dev/null || { echo "encode.sh: ffmpeg is required" >&2; exit 1; }
[[ -f "${INPUT}" ]] || { echo "encode.sh: no such file: ${INPUT}" >&2; exit 1; }

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
WORK="$(mktemp -d)"
trap 'rm -rf "${WORK}"' EXIT

FILTER="[0:v:0]fps=${FPS},scale=${W}:${H}:flags=area"
if [[ ${DITHER} -eq 1 ]]; then
    # Through a two-colour palette with Bayer dithering, then back to grey for
    # the packer, whose threshold leaves the 0/255 pixels as they are.
    FILTER="${FILTER},split[a][b];[a]palettegen=max_colors=2:reserve_transparent=0[p];[b][p]paletteuse=dither=bayer:bayer_scale=2,format=gray[out]"
else
    FILTER="${FILTER},format=gray[out]"
fi

ffmpeg -loglevel error -y "${SEEK[@]}" "${LENGTH[@]}" -i "${INPUT}" \
    -filter_complex "${FILTER}" -map "[out]" -f rawvideo "${WORK}/video.gray"
# A video with no sound still packs: the soundtrack is just empty.
if ! ffmpeg -loglevel error -y "${SEEK[@]}" "${LENGTH[@]}" -i "${INPUT}" \
    -map 0:a:0 -ac 1 -ar "${RATE}" -f s16le "${WORK}/audio.pcm" 2>/dev/null; then
    : > "${WORK}/audio.pcm"
fi

cc -O2 -o "${WORK}/pack_bap" "${HERE}/pack_bap.c"
"${WORK}/pack_bap" "${WORK}/video.gray" "${WORK}/audio.pcm" "${OUTPUT}" "${W}" "${H}" "${FPS}" "${RATE}"
