/* doomgeneric for NodeOS.
 *
 * The six DG_* entry points, plus the scancode decoding and the blit that
 * gets Doom's 320x200 frame into whatever size window the compositor granted.
 * Everything below this file that touches the machine goes through
 * nodeos_platform.h, which is implemented in Rust on top of libnode.
 *
 * Doom renders into DG_ScreenBuffer as 32-bit 0x00RRGGBB (i_video.c's
 * "rgba8888" mode), which is byte-for-byte the layout a NodeOS window surface
 * uses, so the blit is a scale and a copy with no pixel conversion.
 *
 * The only thing the move from a raw framebuffer to a window changed here is
 * the ndos_fb_commit() at the end of DG_DrawFrame: pixels written to a surface
 * are not on screen until the compositor has been told to copy them.
 */
#include <stddef.h>
#include <stdint.h>
#include <stdio.h>
#include <string.h>

#include "doomkeys.h"
#include "doomgeneric.h"
#include "m_argv.h"

#include "nodeos_platform.h"

/* ------------------------------------------------------------- display --- */

static uint8_t *fb_base;
static uint32_t fb_width;
static uint32_t fb_height;
static uint32_t fb_pitch;   /* bytes per scanline */

static uint32_t blit_scale;    /* integer magnification */
static uint32_t blit_x;        /* left edge of the image, in pixels */
static uint32_t blit_y;        /* top edge of the image, in pixels */
static uint32_t fb_serial_seen; /* ndos_fb_serial() when the blit was set up */

/* One scaled output row, reused every row of every frame. Sized for the
   widest window we are willing to drive. */
#define MAX_FB_WIDTH 4096
static uint32_t row_scratch[MAX_FB_WIDTH];

static void display_init(void)
{
    fb_base = ndos_fb_info(&fb_width, &fb_height, &fb_pitch);
    /* Remember which surface this setup describes, so DG_DrawFrame can tell a
       compositor resize from an ordinary frame. Set even on the failure paths
       below, or a window that was never granted would re-run this every
       frame. */
    fb_serial_seen = ndos_fb_serial();

    if (fb_base == NULL)
    {
        printf("doom: no window was granted; running headless\n");
        return;
    }
    if (fb_width > MAX_FB_WIDTH)
    {
        printf("doom: window is %u px wide, wider than this port drives\n", fb_width);
        fb_base = NULL;
        return;
    }
    if (fb_width < DOOMGENERIC_RESX || fb_height < DOOMGENERIC_RESY)
    {
        /* Below one Doom pixel per screen pixel there is nothing sensible to
           draw: the scale would round to 1 and each row would run off the end
           of its scanline. */
        printf("doom: window %ux%u is smaller than %dx%d\n",
               fb_width, fb_height, DOOMGENERIC_RESX, DOOMGENERIC_RESY);
        fb_base = NULL;
        return;
    }

    /* Largest whole-pixel magnification that still fits. Integer scaling
       keeps every Doom pixel the same size; a fractional scale would shimmer
       as the view moves, which is worse than a smaller picture. */
    blit_scale = fb_width / DOOMGENERIC_RESX;
    if (fb_height / DOOMGENERIC_RESY < blit_scale)
    {
        blit_scale = fb_height / DOOMGENERIC_RESY;
    }
    if (blit_scale == 0)
    {
        blit_scale = 1;
    }

    uint32_t drawn_w = DOOMGENERIC_RESX * blit_scale;
    uint32_t drawn_h = DOOMGENERIC_RESY * blit_scale;
    blit_x = drawn_w < fb_width ? (fb_width - drawn_w) / 2 : 0;
    blit_y = drawn_h < fb_height ? (fb_height - drawn_h) / 2 : 0;

    printf("doom: window %ux%u, pitch %u, %ux scale at (%u,%u)\n",
           fb_width, fb_height, fb_pitch, blit_scale, blit_x, blit_y);

    /* Black out the whole window once, so the letterbox around the image is
       not whatever the surface happened to contain. */
    for (uint32_t y = 0; y < fb_height; y++)
    {
        memset(fb_base + (size_t)y * fb_pitch, 0, (size_t)fb_width * 4);
    }
    ndos_fb_commit(0, 0, fb_width, fb_height);
}

void DG_DrawFrame(void)
{
    /* The compositor replaces the surface when the window is maximized or
       restored. Re-read the geometry and recompute the scale before drawing
       into what may be a different object than the last frame used. */
    if (ndos_fb_serial() != fb_serial_seen)
    {
        display_init();
    }

    if (fb_base == NULL)
    {
        return;
    }

    const uint32_t *src = (const uint32_t *)DG_ScreenBuffer;
    uint32_t drawn_w = DOOMGENERIC_RESX * blit_scale;
    size_t row_bytes = (size_t)drawn_w * 4;

    for (uint32_t y = 0; y < DOOMGENERIC_RESY; y++)
    {
        /* Expand one source row horizontally... */
        if (blit_scale == 1)
        {
            memcpy(row_scratch, src + (size_t)y * DOOMGENERIC_RESX, row_bytes);
        }
        else
        {
            const uint32_t *in = src + (size_t)y * DOOMGENERIC_RESX;
            uint32_t *out = row_scratch;
            for (uint32_t x = 0; x < DOOMGENERIC_RESX; x++)
            {
                uint32_t pixel = in[x];
                for (uint32_t n = 0; n < blit_scale; n++)
                {
                    *out++ = pixel;
                }
            }
        }

        /* ...then write it out `blit_scale` times, which is the vertical
           expansion and costs one memcpy per output line. */
        for (uint32_t n = 0; n < blit_scale; n++)
        {
            uint32_t out_y = blit_y + y * blit_scale + n;
            if (out_y >= fb_height)
            {
                break;
            }
            memcpy(fb_base + (size_t)out_y * fb_pitch + (size_t)blit_x * 4, row_scratch, row_bytes);
        }
    }

    /* Only the image is ever redrawn -- the letterbox around it was blacked
       out once in display_init and nothing writes it since -- so the commit
       covers the image and not the window. */
    ndos_fb_commit(blit_x, blit_y, drawn_w, DOOMGENERIC_RESY * blit_scale);
}

/* -------------------------------------------------------------- keyboard --- */

#define KEY_QUEUE_LEN 32

/* (pressed << 8) | doom key code */
static uint16_t key_queue[KEY_QUEUE_LEN];
static unsigned key_write;
static unsigned key_read;

/* PS/2 set 1 make codes -> Doom key codes. Release codes are the make code
   with bit 7 set; extended keys arrive prefixed with 0xE0 and are handled
   separately below, because 0xE0 0x4B (left arrow) and 0x4B (keypad 4) are
   the same low byte. */
static unsigned char doom_key_for(unsigned char make, int extended)
{
    if (extended)
    {
        switch (make)
        {
        case 0x48: return KEY_UPARROW;
        case 0x50: return KEY_DOWNARROW;
        case 0x4B: return KEY_LEFTARROW;
        case 0x4D: return KEY_RIGHTARROW;
        case 0x1D: return KEY_FIRE;    /* right ctrl */
        case 0x38: return KEY_RALT;    /* right alt: strafe */
        case 0x1C: return KEY_ENTER;   /* keypad enter */
        case 0x47: return KEY_HOME;
        case 0x4F: return KEY_END;
        case 0x49: return KEY_PGUP;
        case 0x51: return KEY_PGDN;
        case 0x53: return KEY_DEL;
        case 0x52: return KEY_INS;
        default:   return 0;
        }
    }

    switch (make)
    {
    case 0x01: return KEY_ESCAPE;
    case 0x1C: return KEY_ENTER;
    case 0x0F: return KEY_TAB;         /* automap */
    case 0x0E: return KEY_BACKSPACE;
    case 0x39: return KEY_USE;         /* space */
    case 0x1D: return KEY_FIRE;        /* left ctrl */
    case 0x38: return KEY_RALT;        /* left alt: strafe */
    case 0x2A: case 0x36: return KEY_RSHIFT; /* either shift: run */

    case 0x48: return KEY_UPARROW;     /* keypad, when numlock is off */
    case 0x50: return KEY_DOWNARROW;
    case 0x4B: return KEY_LEFTARROW;
    case 0x4D: return KEY_RIGHTARROW;

    case 0x0C: return KEY_MINUS;
    case 0x0D: return KEY_EQUALS;
    case 0x3A: return KEY_CAPSLOCK;
    case 0x45: return KEY_PAUSE;

    /* Weapon selection and the menu's numeric answers. */
    case 0x02: return '1';
    case 0x03: return '2';
    case 0x04: return '3';
    case 0x05: return '4';
    case 0x06: return '5';
    case 0x07: return '6';
    case 0x08: return '7';
    case 0x09: return '8';
    case 0x0A: return '9';
    case 0x0B: return '0';

    /* Letters: cheats, and the y/n the menu asks for. Doom compares against
       lowercase ASCII. */
    case 0x10: return 'q';
    case 0x11: return 'w';
    case 0x12: return 'e';
    case 0x13: return 'r';
    case 0x14: return 't';
    case 0x15: return 'y';
    case 0x16: return 'u';
    case 0x17: return 'i';
    case 0x18: return 'o';
    case 0x19: return 'p';
    case 0x1E: return 'a';
    case 0x1F: return 's';
    case 0x20: return 'd';
    case 0x21: return 'f';
    case 0x22: return 'g';
    case 0x23: return 'h';
    case 0x24: return 'j';
    case 0x25: return 'k';
    case 0x26: return 'l';
    case 0x2C: return 'z';
    case 0x2D: return 'x';
    case 0x2E: return 'c';
    case 0x2F: return 'v';
    case 0x30: return 'b';
    case 0x31: return 'n';
    case 0x32: return 'm';

    /* Function keys: F1 help through F12, contiguous except F11/F12. */
    case 0x3B: return KEY_F1;
    case 0x3C: return KEY_F2;
    case 0x3D: return KEY_F3;
    case 0x3E: return KEY_F4;
    case 0x3F: return KEY_F5;
    case 0x40: return KEY_F6;
    case 0x41: return KEY_F7;
    case 0x42: return KEY_F8;
    case 0x43: return KEY_F9;
    case 0x44: return KEY_F10;
    case 0x57: return KEY_F11;
    case 0x58: return KEY_F12;

    default:   return 0;
    }
}

static void queue_key(int pressed, unsigned char key)
{
    if (key == 0)
    {
        return;
    }
    key_queue[key_write] = (uint16_t)((pressed << 8) | key);
    key_write = (key_write + 1) % KEY_QUEUE_LEN;

    /* A full queue drops the oldest event rather than the newest: losing the
       start of a burst is less confusing than losing the key just pressed. */
    if (key_write == key_read)
    {
        key_read = (key_read + 1) % KEY_QUEUE_LEN;
    }
}

static void poll_keyboard(void)
{
    static int extended;
    unsigned char bytes[32];

    size_t n = ndos_kbd_read(bytes, sizeof(bytes));

    for (size_t i = 0; i < n; i++)
    {
        unsigned char code = bytes[i];

        if (code == 0xE0)
        {
            extended = 1;
            continue;
        }
        /* 0xE1 introduces the six-byte Pause sequence; skipping the prefix
           leaves its remaining bytes to decode as harmless make codes. */
        if (code == 0xE1)
        {
            continue;
        }

        int released = (code & 0x80) != 0;
        queue_key(!released, doom_key_for(code & 0x7F, extended));
        extended = 0;
    }
}

int DG_GetKey(int *pressed, unsigned char *key)
{
    poll_keyboard();

    if (key_read == key_write)
    {
        return 0;
    }
    uint16_t event = key_queue[key_read];
    key_read = (key_read + 1) % KEY_QUEUE_LEN;

    *pressed = event >> 8;
    *key = (unsigned char)(event & 0xFF);
    return 1;
}

/* ----------------------------------------------------------------- time --- */

uint32_t DG_GetTicksMs(void)
{
    return (uint32_t)(ndos_time_ns() / 1000000ULL);
}

void DG_SleepMs(uint32_t ms)
{
    uint64_t deadline = ndos_time_ns() + (uint64_t)ms * 1000000ULL;

    /* NodeOS has no sleep syscall yet -- `wait` with a deadline is defined on
       an object, and there is no object to wait on here -- so give up the
       timeslice until the clock says otherwise. */
    while (ndos_time_ns() < deadline)
    {
        ndos_yield();
    }
}

/* ---------------------------------------------------------------- entry --- */

void DG_Init(void)
{
    display_init();
}

void DG_SetWindowTitle(const char *title)
{
    (void)title;
}

/* Called from Rust once the capabilities are in place. */
int doom_main(int argc, char **argv)
{
    doomgeneric_Create(argc, argv);

    for (;;)
    {
        doomgeneric_Tick();
    }
    return 0;
}
