/*
 * i_nodeossound.c -- Doom's sound module, talking to the NodeOS mixer.
 *
 * Doom's sound interface (i_sound.h's sound_module_t) is a table of eight
 * functions, and this is the whole of NodeOS's implementation of it. The port
 * needed no changes to Doom's own sound logic: s_sound.c still decides which
 * effects play on which of its channels, at what volume and how far to the
 * left, and this file turns each of those decisions into one message to the
 * `audio` service.
 *
 * The division of labour is the interesting part. Doom's channel number
 * becomes a mixer voice number unchanged, so "channel 3 is now this sound
 * instead" is a single AUDIO_PLAY rather than a stop and a start; and the
 * mixing itself happens in the service, where it has to, because Doom is not
 * the only program that can make a noise.
 *
 * What is *not* here is music. Doom's music is MUS data meant for an OPL2 or
 * a MIDI device, and playing it means a synthesiser -- which is a much larger
 * piece of work than a mixer and shares none of its code. `music_module` stays
 * null, which Doom already handles: I_PlaySong and friends check for it.
 *
 * # DMX lumps
 *
 * A Doom sound effect is a lump in the WAD with an eight-byte header:
 *
 *     u16 format   always 3
 *     u16 rate     samples per second, usually 11025
 *     u32 length   sample count, including the padding below
 *
 * followed by unsigned 8-bit mono samples. The first and last sixteen samples
 * are padding that the original DMX library required and every player since
 * has skipped; skipping them here is what stops each effect starting and
 * ending with a click.
 */

#include <stdint.h>
#include <string.h>

#include "config.h"
#include "doomtype.h"
#include "i_sound.h"
#include "m_misc.h"
#include "w_wad.h"
#include "z_zone.h"

#include "nodeos_platform.h"

#ifdef __NODEOS_SOUND__

/* Doom asks for SNDDEVICE_SB because that is what its configuration defaults
 * to; the name means nothing here beyond "a device that plays samples". */
static snddevice_t sound_devices[] = { SNDDEVICE_SB };

/* Doom's own volume scale is 0..127 and the mixer's is 0..255. */
#define VOLUME_SCALE 2

/* Sample padding at each end of a DMX lump. */
#define DMX_PADDING 16

/* Doom's channel count; the mixer has more voices than this. */
#define MAX_CHANNELS 16

/* When each channel's sound is due to finish, in milliseconds since boot.
 *
 * SoundIsPlaying is answered from this rather than by asking the service,
 * because Doom calls it for every channel every tic and the answer is one that
 * can be computed exactly: a sound of n samples at r hertz lasts n/r seconds
 * and the mixer does not stretch it. Thirty-five round trips a second to learn
 * something arithmetic already knows would be a poor trade.
 */
static uint64_t channel_ends_ms[MAX_CHANNELS];

static boolean use_sfx_prefix;

static uint64_t now_ms(void)
{
    return ndos_time_ns() / 1000000ull;
}

static boolean I_NodeOS_InitSound(boolean sfx_prefix)
{
    int i;

    use_sfx_prefix = sfx_prefix;
    for (i = 0; i < MAX_CHANNELS; ++i)
    {
        channel_ends_ms[i] = 0;
    }

    /* No mixer, no sound module: returning false here leaves Doom's
     * sound_module null, and every sound call after it is a no-op inside
     * Doom rather than an error path in this file. */
    return ndos_audio_available() != 0;
}

static void I_NodeOS_ShutdownSound(void)
{
    int i;

    for (i = 0; i < MAX_CHANNELS; ++i)
    {
        ndos_audio_stop((uint32_t) i);
    }
}

static int I_NodeOS_GetSfxLumpNum(sfxinfo_t *sfx)
{
    char name[9];

    if (use_sfx_prefix)
    {
        M_snprintf(name, sizeof(name), "ds%s", sfx->name);
    }
    else
    {
        M_StringCopy(name, sfx->name, sizeof(name));
    }

    return W_GetNumForName(name);
}

static void I_NodeOS_UpdateSound(void)
{
    /* The service mixes on its own schedule; there is nothing to pump. */
}

static void I_NodeOS_UpdateSoundParams(int channel, int vol, int sep)
{
    if (channel < 0 || channel >= MAX_CHANNELS)
    {
        return;
    }
    ndos_audio_set_volume((uint32_t) channel,
                          (uint32_t) (vol * VOLUME_SCALE),
                          (uint32_t) sep);
}

static int I_NodeOS_StartSound(sfxinfo_t *sfxinfo, int channel, int vol, int sep)
{
    const uint8_t *lump;
    const uint8_t *samples;
    uint32_t rate;
    uint32_t count;
    int length;

    if (channel < 0 || channel >= MAX_CHANNELS || sfxinfo->lumpnum < 0)
    {
        return -1;
    }

    length = W_LumpLength(sfxinfo->lumpnum);
    if (length < 8)
    {
        return -1;
    }
    lump = (const uint8_t *) W_CacheLumpNum(sfxinfo->lumpnum, PU_STATIC);
    if (lump == NULL)
    {
        return -1;
    }

    /* Header fields are little-endian and unaligned; read them a byte at a
     * time rather than casting, which is what an alignment-fussy compiler
     * would otherwise be entitled to turn into a fault. */
    if (lump[0] != 0x03 || lump[1] != 0x00)
    {
        W_ReleaseLumpNum(sfxinfo->lumpnum);
        return -1;
    }
    rate = (uint32_t) lump[2] | ((uint32_t) lump[3] << 8);
    count = (uint32_t) lump[4] | ((uint32_t) lump[5] << 8)
          | ((uint32_t) lump[6] << 16) | ((uint32_t) lump[7] << 24);

    /* The length in the header is not to be trusted over the length of the
     * lump: a truncated WAD would otherwise have this reading past the end of
     * the mapping. */
    if (count > (uint32_t) (length - 8))
    {
        count = (uint32_t) (length - 8);
    }
    if (rate == 0 || count == 0)
    {
        W_ReleaseLumpNum(sfxinfo->lumpnum);
        return -1;
    }

    samples = lump + 8;
    if (count > 2 * DMX_PADDING)
    {
        samples += DMX_PADDING;
        count -= 2 * DMX_PADDING;
    }

    if (!ndos_audio_play((uint32_t) channel, samples, count, rate,
                         (uint32_t) (vol * VOLUME_SCALE), (uint32_t) sep))
    {
        W_ReleaseLumpNum(sfxinfo->lumpnum);
        return -1;
    }

    /* The samples have been copied into the message by now, so the lump can go
     * back to the zone allocator immediately -- there is no asynchronous read
     * of it still outstanding. */
    W_ReleaseLumpNum(sfxinfo->lumpnum);

    channel_ends_ms[channel] = now_ms() + (uint64_t) count * 1000ull / rate;
    return channel;
}

static void I_NodeOS_StopSound(int channel)
{
    if (channel < 0 || channel >= MAX_CHANNELS)
    {
        return;
    }
    ndos_audio_stop((uint32_t) channel);
    channel_ends_ms[channel] = 0;
}

static boolean I_NodeOS_SoundIsPlaying(int channel)
{
    if (channel < 0 || channel >= MAX_CHANNELS)
    {
        return false;
    }
    return now_ms() < channel_ends_ms[channel];
}

static void I_NodeOS_CacheSounds(sfxinfo_t *sounds, int num_sounds)
{
    /* Nothing to precache: a sound is read from the WAD when it is played and
     * copied straight into the message. Doom's own zone allocator caches the
     * lump, which is the caching that matters. */
    (void) sounds;
    (void) num_sounds;
}

sound_module_t nodeos_sound_module =
{
    sound_devices,
    arrlen(sound_devices),
    I_NodeOS_InitSound,
    I_NodeOS_ShutdownSound,
    I_NodeOS_GetSfxLumpNum,
    I_NodeOS_UpdateSound,
    I_NodeOS_UpdateSoundParams,
    I_NodeOS_StartSound,
    I_NodeOS_StopSound,
    I_NodeOS_SoundIsPlaying,
    I_NodeOS_CacheSounds,
};

#endif /* __NODEOS_SOUND__ */
