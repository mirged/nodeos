// pack_bap.c -- pack raw video and audio into a .BAP file for `video`.
//
//   pack_bap VIDEO.gray AUDIO.pcm OUT.BAP [WIDTH HEIGHT FPS RATE]
//
// VIDEO.gray is 8-bit grey frames, WIDTH x HEIGHT each (default 480x360), at
// FPS (default 30); a pixel of 128 or more is white. AUDIO.pcm is signed
// 16-bit little-endian mono at RATE Hz (default 48000), and may be empty or
// shorter than the video. encode.sh makes both from any video with ffmpeg;
// gen_demo makes them from nothing. The format is described in
// ../src/bap.rs.
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>

static void put16(uint8_t *p, unsigned v) { p[0] = v & 255, p[1] = (v >> 8) & 255; }
static void put32(uint8_t *p, uint32_t v) { put16(p, v & 0xffff), put16(p + 2, v >> 16); }

int main(int argc, char **argv) {
    if (argc != 4 && argc != 8) {
        fprintf(stderr, "usage: %s VIDEO.gray AUDIO.pcm OUT.BAP [WIDTH HEIGHT FPS RATE]\n", argv[0]);
        return 2;
    }
    unsigned w = 480, h = 360, fps = 30, rate = 48000;
    if (argc == 8) w = atoi(argv[4]), h = atoi(argv[5]), fps = atoi(argv[6]), rate = atoi(argv[7]);
    if (!w || !h || w > 65535 || h > 65535 || !fps || fps > 240 || !rate) {
        fprintf(stderr, "pack_bap: bad size, frame rate or sample rate\n");
        return 2;
    }
    FILE *vf = fopen(argv[1], "rb");
    FILE *af = fopen(argv[2], "rb");
    FILE *out = fopen(argv[3], "wb");
    if (!vf || !af || !out) {
        perror("pack_bap");
        return 1;
    }

    size_t npix = (size_t)w * h;
    uint8_t *vbuf = malloc(npix);
    // Frame n carries the samples from n*rate/fps up to (n+1)*rate/fps, so a
    // rate that is not a multiple of the frame rate still adds up exactly.
    size_t max_audio = ((size_t)rate / fps + 1) * 2;
    uint8_t *abuf = malloc(max_audio);
    // The worst case is a run of one per pixel, one byte each.
    uint8_t *rle = malloc(npix + 16);
    if (!vbuf || !abuf || !rle) return 1;

    uint8_t hdr[20] = {'B', 'A', 'P', '2'};
    put16(hdr + 4, w), put16(hdr + 6, h), put16(hdr + 8, fps), put32(hdr + 14, rate), put16(hdr + 18, 1);
    fwrite(hdr, 1, sizeof hdr, out);

    uint32_t frames = 0;
    while (fread(vbuf, 1, npix, vf) == npix) {
        uint64_t from = (uint64_t)frames * rate / fps, to = (uint64_t)(frames + 1) * rate / fps;
        size_t ab = fread(abuf, 1, (to - from) * 2, af) & ~(size_t)1;

        size_t rlen = 0;
        uint8_t cur = 0; // every frame starts with a black run, possibly empty
        for (size_t i = 0; i < npix;) {
            uint32_t run = 0;
            while (i + run < npix && (vbuf[i + run] >= 128) == cur) run++;
            i += run;
            cur ^= 1;
            while (run >= 0x80) { // unsigned LEB128
                rle[rlen++] = (uint8_t)((run & 0x7f) | 0x80);
                run >>= 7;
            }
            rle[rlen++] = (uint8_t)run;
        }

        uint8_t lens[8];
        put32(lens, (uint32_t)ab), put32(lens + 4, (uint32_t)rlen);
        fwrite(lens, 1, 8, out);
        fwrite(abuf, 1, ab, out);
        fwrite(rle, 1, rlen, out);
        frames++;
    }

    uint8_t count[4];
    put32(count, frames);
    fseek(out, 10, SEEK_SET);
    fwrite(count, 1, 4, out);
    if (fclose(out)) {
        perror("pack_bap");
        return 1;
    }
    printf("pack_bap: %u frames, %ux%u at %u fps, %u Hz -> %s\n", frames, w, h, fps, rate, argv[3]);
    return 0;
}
