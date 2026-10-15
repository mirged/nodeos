/* The entire NodeOS surface the C side is allowed to touch.
 *
 * Everything here is implemented in Rust (user/doom/src/platform.rs) on top of
 * libnode. The C code -- Doom itself and the mini-libc under libc/ -- never
 * issues a syscall directly; it calls one of these. Keeping the boundary this
 * narrow is what lets the C tree stay a straight port rather than a fork.
 */
#ifndef NODEOS_PLATFORM_H
#define NODEOS_PLATFORM_H

#include <stddef.h>
#include <stdint.h>

/* Write to the kernel debug log (the serial console). */
void ndos_write(const char *buf, size_t len);

/* Grow the process heap by `len` bytes. Returns NULL when the heap is
   exhausted; never returns a partial extension. */
void *ndos_sbrk(size_t len);

/* Terminate the process. */
void ndos_exit(int code) __attribute__((noreturn));

/* Monotonic nanoseconds since boot. */
uint64_t ndos_time_ns(void);

/* Yield the rest of this timeslice. */
void ndos_yield(void);

/* The IWAD image, mapped read-only from the memory object the launcher
   granted. `*len` receives its size; returns NULL when no WAD was granted. */
const unsigned char *ndos_wad_data(size_t *len);

/* Geometry and base of this program's window surface. Returns NULL when no
   window was granted. The buffer is 32bpp 0x00RRGGBB, like the framebuffer it
   used to be. */
uint8_t *ndos_fb_info(uint32_t *width, uint32_t *height, uint32_t *pitch);

/* A counter that changes whenever the values `ndos_fb_info` returns have
   changed -- the compositor maximized or restored the window and handed over a
   new surface. The blit polls it once a frame and re-reads the geometry. */
uint32_t ndos_fb_serial(void);

/* Publish a finished rectangle of the window to the compositor. Blocks until
   the compositor has copied it, so the next frame cannot overwrite pixels it
   is still reading. */
void ndos_fb_commit(uint32_t x, uint32_t y, uint32_t w, uint32_t h);

/* Drain queued PS/2 scancodes, non-blocking. Returns the count copied. The
   compositor decodes the keyboard and only sends keys while this window has
   the focus; the byte stream itself is unchanged. */
size_t ndos_kbd_read(unsigned char *buf, size_t len);

/* --- Sound ---------------------------------------------------------------
 *
 * The mixer lives in the `audio` service; these are messages to it. Doom's own
 * scales are kept: a voice number is one of Doom's sound channels, and a
 * separation of 128 is dead ahead. Volume is the one exception -- Doom's range
 * is 0..127 and the service's is 0..255, so csrc/i_nodeossound.c doubles it.
 */

/* Whether a mixer was granted at all. Zero means this boot has no `audio`
   service, and the sound module declines to initialise. */
int ndos_audio_available(void);

/* Start `count` unsigned 8-bit samples at `rate` hertz on `voice`, replacing
   whatever it was playing. Returns non-zero on success. */
int ndos_audio_play(uint32_t voice, const uint8_t *samples, uint32_t count,
                    uint32_t rate, uint32_t volume, uint32_t separation);

/* Silence one voice. */
void ndos_audio_stop(uint32_t voice);

/* Change a sounding voice's volume and stereo position without restarting
   it -- what Doom does as the thing making the noise moves. */
void ndos_audio_set_volume(uint32_t voice, uint32_t volume, uint32_t separation);

#endif /* NODEOS_PLATFORM_H */
