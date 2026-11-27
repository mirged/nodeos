// gen_demo.c -- the NodeOS homebrew demo: an original 1-bit video and a
// chiptune soundtrack, generated from nothing but this file.
//
//   gen_demo VIDEO.gray AUDIO.pcm
//
// Writes 480x360 8-bit grey frames at 30 fps and signed 16-bit mono PCM at
// 48 kHz -- exactly what pack_bap takes. Every pixel is 0 or 255, so the
// packer's threshold changes nothing and the dithering below survives intact.
// The output is deterministic: the same source always makes the same file.
//
// 64 seconds, 120 BPM, 32 bars of 2 seconds, in six scenes:
//
//   bars  0- 5  starfield, then the title irises in
//   bars  6-11  metaballs that swell on the kick
//   bars 12-17  XOR moire rings
//   bars 18-23  a dithered, lit, rotating cube over a scrolling marquee
//   bars 24-29  Conway's Life, seeded with the word NODEOS
//   bars 30-31  credits, dissolving to black
//
// Scenes cross-dissolve through a 4x4 Bayer matrix, the 1-bit way to fade.

#include <math.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

#define W 480
#define H 360
#define FPS 30
#define RATE 48000
#define BPM 120
#define BARS 32
#define SECONDS_PER_BAR (60.0 * 4 / BPM)
#define FRAMES (BARS * 4 * 60 * FPS / BPM)
#define SAMPLES (BARS * 4 * 60 * RATE / BPM)
#define BARS_PER_SCENE 6
#define FADE_FRAMES 12

static uint8_t frame[W * H], other[W * H];

// --- Deterministic randomness ------------------------------------------------

static uint32_t rng_state = 0x6e6f6465; // "node"
static uint32_t rng(void) {
    rng_state ^= rng_state << 13;
    rng_state ^= rng_state >> 17;
    rng_state ^= rng_state << 5;
    return rng_state;
}
static double frand(void) { return (rng() & 0xffffff) / (double)0x1000000; }

// --- Drawing ----------------------------------------------------------------

static const uint8_t bayer[4][4] = {{0, 8, 2, 10}, {12, 4, 14, 6}, {3, 11, 1, 9}, {15, 7, 13, 5}};

// 1 if a pixel at (x, y) of brightness `level` (0..1) should be white.
static int dither(int x, int y, double level) { return level * 16 > bayer[y & 3][x & 3] + 0.5; }

static void clear(uint8_t *buf, uint8_t v) { memset(buf, v, W * H); }

static void rect(uint8_t *buf, int x, int y, int w, int h, uint8_t v) {
    for (int j = y < 0 ? 0 : y; j < y + h && j < H; j++)
        for (int i = x < 0 ? 0 : x; i < x + w && i < W; i++) buf[j * W + i] = v;
}

// 5x7 capitals, digits and a little punctuation. Each row is five bits, MSB left.
static const struct { char c; uint8_t rows[7]; } font[] = {
    {'A', {14, 17, 17, 31, 17, 17, 17}}, {'B', {30, 17, 17, 30, 17, 17, 30}},
    {'C', {14, 17, 16, 16, 16, 17, 14}}, {'D', {30, 17, 17, 17, 17, 17, 30}},
    {'E', {31, 16, 16, 30, 16, 16, 31}}, {'F', {31, 16, 16, 30, 16, 16, 16}},
    {'G', {14, 17, 16, 23, 17, 17, 15}}, {'H', {17, 17, 17, 31, 17, 17, 17}},
    {'I', {14, 4, 4, 4, 4, 4, 14}},      {'J', {7, 2, 2, 2, 2, 18, 12}},
    {'K', {17, 18, 20, 24, 20, 18, 17}}, {'L', {16, 16, 16, 16, 16, 16, 31}},
    {'M', {17, 27, 21, 21, 17, 17, 17}}, {'N', {17, 17, 25, 21, 19, 17, 17}},
    {'O', {14, 17, 17, 17, 17, 17, 14}}, {'P', {30, 17, 17, 30, 16, 16, 16}},
    {'Q', {14, 17, 17, 17, 21, 18, 13}}, {'R', {30, 17, 17, 30, 20, 18, 17}},
    {'S', {15, 16, 16, 14, 1, 1, 30}},   {'T', {31, 4, 4, 4, 4, 4, 4}},
    {'U', {17, 17, 17, 17, 17, 17, 14}}, {'V', {17, 17, 17, 17, 17, 10, 4}},
    {'W', {17, 17, 17, 21, 21, 21, 10}}, {'X', {17, 17, 10, 4, 10, 17, 17}},
    {'Y', {17, 17, 10, 4, 4, 4, 4}},     {'Z', {31, 1, 2, 4, 8, 16, 31}},
    {'0', {14, 17, 19, 21, 25, 17, 14}}, {'1', {4, 12, 4, 4, 4, 4, 14}},
    {'2', {14, 17, 1, 2, 4, 8, 31}},     {'3', {30, 1, 1, 14, 1, 1, 30}},
    {'4', {2, 6, 10, 18, 31, 2, 2}},     {'5', {31, 16, 30, 1, 1, 17, 14}},
    {'6', {6, 8, 16, 30, 17, 17, 14}},   {'7', {31, 1, 2, 4, 8, 8, 8}},
    {'8', {14, 17, 17, 14, 17, 17, 14}}, {'9', {14, 17, 17, 15, 1, 2, 12}},
    {'-', {0, 0, 0, 31, 0, 0, 0}},       {'.', {0, 0, 0, 0, 0, 12, 12}},
    {'!', {4, 4, 4, 4, 4, 0, 4}},        {'/', {1, 1, 2, 4, 8, 16, 16}},
    {'*', {0, 21, 14, 31, 14, 21, 0}},
};

static const uint8_t *glyph(char c) {
    for (size_t i = 0; i < sizeof font / sizeof font[0]; i++)
        if (font[i].c == c) return font[i].rows;
    return NULL;
}

static int text_width(const char *s, int scale) { return (int)strlen(s) * 6 * scale - scale; }

// Draw `s` with its top-left at (x, y), each font pixel a scale x scale block.
static void text(uint8_t *buf, const char *s, int x, int y, int scale, uint8_t v) {
    for (; *s; s++, x += 6 * scale) {
        const uint8_t *g = glyph(*s);
        if (!g) continue;
        for (int r = 0; r < 7; r++)
            for (int c = 0; c < 5; c++)
                if (g[r] & (16 >> c)) rect(buf, x + c * scale, y + r * scale, scale, scale, v);
    }
}

static void text_centered(uint8_t *buf, const char *s, int cy, int scale, uint8_t v) {
    text(buf, s, (W - text_width(s, scale)) / 2, cy - 7 * scale / 2, scale, v);
}

// --- Music timing, shared by picture and sound ------------------------------

static double beat_of(double seconds) { return seconds * BPM / 60.0; }

// 1 on a kick, decaying towards 0 before the next. The kick is on beats 1 and 3.
static double kick_pulse(double seconds) {
    double b = beat_of(seconds);
    double since = fmod(b, 2.0) * 60.0 / BPM;
    return exp(-since * 8);
}

// --- Scenes -----------------------------------------------------------------

typedef struct { double x, y, z; } Star;
static Star stars[400];

static void scene_intro(uint8_t *buf, double t) {
    clear(buf, 0);
    for (size_t i = 0; i < sizeof stars / sizeof stars[0]; i++) {
        Star *s = &stars[i];
        double z = fmod(s->z - t * 0.35, 1.0);
        if (z <= 0.02) z += 1.0;
        int sx = W / 2 + (int)(s->x / z * 160), sy = H / 2 + (int)(s->y / z * 160);
        int size = z < 0.3 ? 3 : z < 0.6 ? 2 : 1;
        rect(buf, sx, sy, size, size, 255);
    }
    // The title irises in from bar 2: a white disc grows from the centre, and
    // the title is cut out of it in black.
    double iris = (t - 2 * SECONDS_PER_BAR) / SECONDS_PER_BAR;
    if (iris > 0) {
        double radius = (iris > 1 ? 1 : iris) * 330;
        for (int y = 0; y < H; y++)
            for (int x = 0; x < W; x++) {
                double dx = x - W / 2, dy = y - H / 2;
                if (dx * dx + dy * dy < radius * radius) buf[y * W + x] = 255;
            }
        int bob = (int)(kick_pulse(t) * 6);
        text_centered(buf, "NODEOS", H / 2 - 30 - bob, 12, 0);
        if (t > 4 * SECONDS_PER_BAR) text_centered(buf, "VIDEO PLAYER", H / 2 + 50, 4, 0);
        if (t > 5 * SECONDS_PER_BAR) text_centered(buf, "A 1-BIT HOMEBREW DEMO", H - 30, 2, 0);
    }
}

static void scene_metaballs(uint8_t *buf, double t) {
    double bx[6], by[6], br[6];
    double pulse = kick_pulse(t);
    for (int i = 0; i < 6; i++) {
        double a = t * (0.5 + i * 0.13) + i * 1.7;
        bx[i] = W / 2 + cos(a) * (120 + 40 * sin(t * 0.3 + i)) * (i & 1 ? 1.2 : 0.9);
        by[i] = H / 2 + sin(a * 1.3) * (90 + 30 * cos(t * 0.4 + i));
        br[i] = (34 + 6 * i % 18) * (1 + 0.25 * pulse);
    }
    for (int y = 0; y < H; y++)
        for (int x = 0; x < W; x++) {
            double field = 0;
            for (int i = 0; i < 6; i++) {
                double dx = x - bx[i], dy = y - by[i];
                field += br[i] * br[i] / (dx * dx + dy * dy + 1);
            }
            // A dithered halo just outside the surface softens the edge.
            buf[y * W + x] = field >= 1 ? 255 : (field > 0.6 && dither(x, y, (field - 0.6) / 0.4 * 0.5)) ? 255 : 0;
        }
}

static void scene_rings(uint8_t *buf, double t) {
    double ring = 14 + 6 * kick_pulse(t);
    double ax = W / 2 + cos(t * 0.7) * 140, ay = H / 2 + sin(t * 0.9) * 100;
    double bx = W / 2 + cos(t * 0.5 + 2) * 150, by = H / 2 + sin(t * 1.1 + 1) * 110;
    double cx = W / 2 + cos(t * 0.3 + 4) * 100, cy = H / 2 + sin(t * 0.6 + 3) * 80;
    int third = t > 3 * SECONDS_PER_BAR;
    for (int y = 0; y < H; y++)
        for (int x = 0; x < W; x++) {
            int a = (int)(hypot(x - ax, y - ay) / ring);
            int b = (int)(hypot(x - bx, y - by) / ring);
            int v = (a ^ b) & 1;
            if (third) v ^= (int)(hypot(x - cx, y - cy) / (ring * 2)) & 1;
            buf[y * W + x] = v ? 255 : 0;
        }
}

static void scene_cube(uint8_t *buf, double t) {
    clear(buf, 0);
    // Marquee: scrolls along the bottom on a white band.
    rect(buf, 0, H - 34, W, 30, 255);
    const char *marquee = "1-BIT * RLE * 30 FPS * 48 KHZ * NO FRAME DEPENDS ON ANOTHER * ";
    int mw = text_width(marquee, 3) + 18;
    int off = (int)(t * 90) % mw;
    for (int x = -off; x < W; x += mw) text(buf, marquee, x, H - 30, 3, 0);

    static const double v[8][3] = {{-1, -1, -1}, {1, -1, -1}, {1, 1, -1}, {-1, 1, -1},
                                   {-1, -1, 1},  {1, -1, 1},  {1, 1, 1},  {-1, 1, 1}};
    static const int faces[6][4] = {{0, 3, 2, 1}, {4, 5, 6, 7}, {0, 1, 5, 4},
                                    {2, 3, 7, 6}, {1, 2, 6, 5}, {0, 4, 7, 3}};
    double ax = t * 0.9, ay = t * 0.6, scale = 1 + 0.12 * kick_pulse(t);
    double p[8][3], sx[8], sy[8];
    for (int i = 0; i < 8; i++) {
        double x = v[i][0] * scale, y = v[i][1] * scale, z = v[i][2] * scale;
        double y1 = y * cos(ax) - z * sin(ax), z1 = y * sin(ax) + z * cos(ax);
        double x2 = x * cos(ay) + z1 * sin(ay), z2 = -x * sin(ay) + z1 * cos(ay);
        p[i][0] = x2, p[i][1] = y1, p[i][2] = z2 + 4.5;
        sx[i] = W / 2 + p[i][0] / p[i][2] * 300;
        sy[i] = (H - 34) / 2 + p[i][1] / p[i][2] * 300;
    }
    const double light[3] = {-0.4, -0.6, -0.7};
    for (int f = 0; f < 6; f++) {
        const int *q = faces[f];
        // Back faces wind the other way on screen.
        double cross = (sx[q[1]] - sx[q[0]]) * (sy[q[2]] - sy[q[0]]) - (sy[q[1]] - sy[q[0]]) * (sx[q[2]] - sx[q[0]]);
        if (cross >= 0) continue;
        double e1[3], e2[3], n[3];
        for (int k = 0; k < 3; k++) e1[k] = p[q[1]][k] - p[q[0]][k], e2[k] = p[q[2]][k] - p[q[0]][k];
        n[0] = e1[1] * e2[2] - e1[2] * e2[1];
        n[1] = e1[2] * e2[0] - e1[0] * e2[2];
        n[2] = e1[0] * e2[1] - e1[1] * e2[0];
        double len = sqrt(n[0] * n[0] + n[1] * n[1] + n[2] * n[2]);
        double lit = (n[0] * light[0] + n[1] * light[1] + n[2] * light[2]) / len;
        double level = 0.12 + 0.88 * (lit < 0 ? 0 : lit);
        int x0 = W, x1 = 0, y0 = H, y1 = 0;
        for (int k = 0; k < 4; k++) {
            if (sx[q[k]] < x0) x0 = (int)sx[q[k]];
            if (sx[q[k]] > x1) x1 = (int)sx[q[k]] + 1;
            if (sy[q[k]] < y0) y0 = (int)sy[q[k]];
            if (sy[q[k]] > y1) y1 = (int)sy[q[k]] + 1;
        }
        for (int y = y0 < 0 ? 0 : y0; y <= y1 && y < H - 34; y++)
            for (int x = x0 < 0 ? 0 : x0; x <= x1 && x < W; x++) {
                int inside = 1;
                double edge_min = 1e9;
                for (int k = 0; k < 4 && inside; k++) {
                    int a = q[k], b = q[(k + 1) & 3];
                    double ex = sx[b] - sx[a], ey = sy[b] - sy[a];
                    double e = (ex * (y - sy[a]) - ey * (x - sx[a])) / sqrt(ex * ex + ey * ey);
                    if (e > 0) inside = 0;
                    if (-e < edge_min) edge_min = -e;
                }
                if (!inside) continue;
                // A white outline, then the dithered face inside it.
                buf[y * W + x] = (edge_min < 2 || dither(x, y, level)) ? 255 : 0;
            }
    }
}

#define LW 120
#define LH 82
static uint8_t life[LH][LW], life_next[LH][LW];
static int life_gen = -1;

static void life_seed(void) {
    memset(life, 0, sizeof life);
    const char *word = "NODEOS";
    int scale = 3, x0 = (LW - text_width(word, scale)) / 2, y0 = (LH - 7 * scale) / 2;
    for (int i = 0; word[i]; i++) {
        const uint8_t *g = glyph(word[i]);
        for (int r = 0; r < 7; r++)
            for (int c = 0; c < 5; c++)
                if (g[r] & (16 >> c))
                    for (int dy = 0; dy < scale; dy++)
                        for (int dx = 0; dx < scale; dx++) life[y0 + r * scale + dy][x0 + i * 6 * scale + c * scale + dx] = 1;
    }
    life_gen = 0;
}

static void life_step(void) {
    for (int y = 0; y < LH; y++)
        for (int x = 0; x < LW; x++) {
            int n = 0;
            for (int dy = -1; dy <= 1; dy++)
                for (int dx = -1; dx <= 1; dx++)
                    if (dx || dy) n += life[(y + dy + LH) % LH][(x + dx + LW) % LW];
            life_next[y][x] = n == 3 || (n == 2 && life[y][x]);
        }
    memcpy(life, life_next, sizeof life);
    // A glider from a corner every 64 generations keeps the board from dying.
    if (++life_gen % 64 == 0) {
        int gx = (life_gen / 64) % 2 ? 2 : LW - 6, gy = 2;
        static const int g[5][2] = {{1, 0}, {2, 1}, {0, 2}, {1, 2}, {2, 2}};
        for (int k = 0; k < 5; k++) life[gy + g[k][1]][gx + g[k][0]] = 1;
    }
}

static void scene_life(uint8_t *buf, double t) {
    // One generation every two frames, after a bar of showing the word.
    int want = t < SECONDS_PER_BAR ? 0 : (int)((t - SECONDS_PER_BAR) * FPS / 2);
    if (life_gen < 0 || want < life_gen) life_seed();
    while (life_gen < want) life_step();
    clear(buf, 0);
    int top = (H - LH * 4) / 2;
    for (int y = 0; y < LH; y++)
        for (int x = 0; x < LW; x++)
            if (life[y][x]) rect(buf, x * 4, top + y * 4, 3, 3, 255);
    rect(buf, 0, 0, W, top - 2, 255);
    text_centered(buf, "GAME OF LIFE", top / 2 - 1, 1, 0);
    rect(buf, 0, H - top + 2, W, top - 2, 255);
}

static void scene_outro(uint8_t *buf, double t) {
    clear(buf, 0);
    text_centered(buf, "THANKS FOR", H / 2 - 40, 6, 255);
    text_centered(buf, "WATCHING", H / 2 + 30, 6, 255);
    text_centered(buf, "MADE ON NODEOS", H - 24, 2, 255);
    // Dissolve to black through the second bar.
    double fade = (t - SECONDS_PER_BAR) / SECONDS_PER_BAR;
    if (fade > 0)
        for (int y = 0; y < H; y++)
            for (int x = 0; x < W; x++)
                if (!dither(x, y, 1 - fade)) buf[y * W + x] = 0;
}

typedef void (*Scene)(uint8_t *, double);
static const Scene scenes[] = {scene_intro, scene_metaballs, scene_rings, scene_cube, scene_life, scene_outro};
#define SCENES ((int)(sizeof scenes / sizeof scenes[0]))

static void render(int index) {
    double seconds = (double)index / FPS;
    double scene_len = BARS_PER_SCENE * SECONDS_PER_BAR;
    int scene = (int)(seconds / scene_len);
    if (scene >= SCENES) scene = SCENES - 1;
    double local = seconds - scene * scene_len;
    scenes[scene](frame, local);

    // The first FADE_FRAMES of a scene dissolve in from the end of the last.
    int into = (int)(local * FPS);
    if (scene > 0 && into < FADE_FRAMES) {
        scenes[scene - 1](other, local + scene_len);
        double mix = (into + 1.0) / (FADE_FRAMES + 1);
        for (int y = 0; y < H; y++)
            for (int x = 0; x < W; x++)
                if (!dither(x, y, mix)) frame[y * W + x] = other[y * W + x];
    }
    // The flash on every bar line of the metaballs and rings: a white border.
    if ((scene == 1 || scene == 2) && kick_pulse(seconds) > 0.8) {
        rect(frame, 0, 0, W, 6, 255), rect(frame, 0, H - 6, W, 6, 255);
        rect(frame, 0, 0, 6, H, 255), rect(frame, W - 6, 0, 6, H, 255);
    }
}

// --- Music ------------------------------------------------------------------
//
// Am F C G, all the way through. A triangle bass, a 25% square lead, a quiet
// 12.5% square arpeggio, and noise-and-sine drums. Each part comes in with a
// scene, so the soundtrack builds with the picture.

static const int chords[4][3] = {{57, 60, 64}, {53, 57, 60}, {48, 52, 55}, {55, 59, 62}};

// Two phrases of four bars, eight eighth-notes a bar; 0 is a rest.
static const int lead_a[32] = {69, 72, 76, 72, 74, 72, 69, 67, 65, 69, 72, 69, 71, 69, 65, 64,
                               64, 67, 72, 67, 69, 67, 64, 62, 62, 67, 71, 74, 72, 71, 69, 67};
static const int lead_b[32] = {76, 0, 76, 74, 72, 0, 69, 72, 77, 0, 77, 76, 74, 0, 72, 69,
                               76, 0, 76, 74, 72, 74, 76, 79, 79, 0, 77, 0, 76, 0, 74, 71};

static double freq(int midi) { return 440.0 * pow(2.0, (midi - 69) / 12.0); }

static float mix[SAMPLES];

// Add a note of `midi` from `start` for `len` seconds with the given voice.
enum { TRIANGLE, SQUARE25, SQUARE12 };
static void note(int wave, int midi, double start, double len, double gain) {
    if (!midi) return;
    double f = freq(midi), phase = 0;
    int s0 = (int)(start * RATE), n = (int)(len * RATE);
    for (int i = 0; i < n && s0 + i < SAMPLES; i++) {
        double t = (double)i / RATE;
        // A short attack and an exponential tail keep notes from clicking.
        double env = (t < 0.004 ? t / 0.004 : 1) * exp(-t * 3.0) * (i > n - 200 ? (n - i) / 200.0 : 1);
        double v;
        if (wave == TRIANGLE) v = 4 * fabs(phase - 0.5) - 1;
        else v = phase < (wave == SQUARE25 ? 0.25 : 0.125) ? 1 : -1;
        mix[s0 + i] += (float)(v * env * gain);
        phase += f / RATE;
        phase -= floor(phase);
    }
}

static void kick(double start) {
    double phase = 0;
    for (int i = 0; i < RATE / 4; i++) {
        double t = (double)i / RATE;
        int s = (int)(start * RATE) + i;
        if (s >= SAMPLES) break;
        phase += (50 + 110 * exp(-t * 30)) / RATE;
        mix[s] += (float)(sin(2 * M_PI * phase) * exp(-t * 14) * 0.55);
    }
}

static void noise(double start, double len, double decay, double gain, int bright) {
    double held = 0;
    for (int i = 0; i < (int)(len * RATE); i++) {
        int s = (int)(start * RATE) + i;
        if (s >= SAMPLES) break;
        double r = frand() * 2 - 1;
        // A hat is the difference of successive samples: crude, but it is
        // the high half of white noise, which is what a hat is.
        double v = bright ? r - held : r;
        held = r;
        mix[s] += (float)(v * exp(-(double)i / RATE * decay) * gain);
    }
}

static void compose(void) {
    double eighth = 60.0 / BPM / 2;
    for (int bar = 0; bar < BARS; bar++) {
        double t0 = bar * SECONDS_PER_BAR;
        int scene = bar / BARS_PER_SCENE;
        const int *chord = chords[bar % 4];
        int outro = bar >= 30;

        // Bass from bar 2: root and octave on the eighths.
        if (bar >= 2 && !outro)
            for (int e = 0; e < 8; e++) note(TRIANGLE, chord[0] - 12 + (e & 1) * 12, t0 + e * eighth, eighth, 0.30);
        if (outro) note(TRIANGLE, chord[0] - 12, t0, SECONDS_PER_BAR, 0.30);

        // Pads in the intro: the chord, held.
        if (scene == 0)
            for (int k = 0; k < 3; k++) note(SQUARE12, chord[k] + 12, t0, SECONDS_PER_BAR, 0.05);

        // The lead from the metaballs on; phrase B in the rings and Life.
        if (scene >= 1 && !outro) {
            const int *phrase = (scene == 2 || scene == 4) ? lead_b : lead_a;
            for (int e = 0; e < 8; e++) note(SQUARE25, phrase[(bar % 4) * 8 + e], t0 + e * eighth, eighth * 0.9, 0.12);
        }
        if (outro) note(SQUARE25, bar == 30 ? 69 : 81, t0, SECONDS_PER_BAR, 0.10);

        // Arpeggio on the sixteenths in the rings and the cube.
        if (scene == 2 || scene == 3)
            for (int s = 0; s < 16; s++)
                note(SQUARE12, chord[s % 3] + 24, t0 + s * eighth / 2, eighth / 2, 0.05);

        // Drums from bar 4: kick on 1 and 3, snare on 2 and 4, hats on the eighths.
        if (bar >= 4 && !outro) {
            for (int beat = 0; beat < 4; beat++) {
                double tb = t0 + beat * eighth * 2;
                if (beat % 2 == 0) kick(tb);
                else noise(tb, 0.18, 18, 0.30, 0);
            }
            for (int e = 0; e < 8; e++) noise(t0 + e * eighth, 0.04, 80, 0.10, 1);
            // A fill into each new scene.
            if (bar % BARS_PER_SCENE == BARS_PER_SCENE - 1)
                for (int s = 12; s < 16; s++) noise(t0 + s * eighth / 2, 0.08, 30, 0.22, 0);
        }
        if (bar == 30) kick(t0);
    }
}

int main(int argc, char **argv) {
    if (argc != 3) {
        fprintf(stderr, "usage: %s VIDEO.gray AUDIO.pcm\n", argv[0]);
        return 2;
    }
    FILE *vf = fopen(argv[1], "wb"), *af = fopen(argv[2], "wb");
    if (!vf || !af) {
        perror("gen_demo");
        return 1;
    }

    for (size_t i = 0; i < sizeof stars / sizeof stars[0]; i++)
        stars[i] = (Star){frand() * 2 - 1, frand() * 2 - 1, frand()};
    for (int i = 0; i < FRAMES; i++) {
        render(i);
        fwrite(frame, 1, W * H, vf);
    }

    compose();
    // The last half second fades out, and a gentle soft clip keeps the sum
    // of every part out of hard clipping.
    for (int i = 0; i < SAMPLES; i++) {
        double v = mix[i];
        int fade = SAMPLES - RATE / 2;
        if (i > fade) v *= (double)(SAMPLES - i) / (RATE / 2);
        v = tanh(v * 1.2) * 0.9;
        int16_t s = (int16_t)lrint(v * 32767);
        fwrite(&s, 2, 1, af);
    }
    fclose(vf);
    fclose(af);
    fprintf(stderr, "gen_demo: %d frames, %d samples\n", FRAMES, SAMPLES);
    return 0;
}
