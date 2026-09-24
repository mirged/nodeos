/* Streams, over a filesystem that fits in this file.
 *
 * NodeOS has no filesystem service wired to this process, and Doom only ever
 * needs three kinds of stream, so all three live here:
 *
 *   LOG   stdout/stderr, line-buffered into the kernel debug log.
 *   IMAGE the IWAD, read-only, backed by the memory object the launcher
 *         granted -- so `fread` from the WAD is a memcpy out of a mapping and
 *         the 14 MiB image is never copied into the heap.
 *   RAM   anything written (savegames, the config Doom rewrites on exit),
 *         kept in a small table of growable buffers for the life of the
 *         process.
 *
 * A savegame therefore survives until reboot but no longer, which is what a
 * system with no persistent storage can honestly offer. Nothing above this
 * layer knows the difference.
 */
#include <stddef.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <ctype.h>
#include <errno.h>
#include <stdarg.h>

#include "nodeos_platform.h"

int errno;

#define LOG_BUFFER    256
#define RAM_FILES     16
#define RAM_NAME_MAX  128
#define RAM_INITIAL   4096

enum stream_kind
{
    STREAM_LOG,
    STREAM_IMAGE,
    STREAM_RAM,
};

/* A file that lives in memory for the life of the process. */
typedef struct
{
    char name[RAM_NAME_MAX];
    unsigned char *data;
    size_t len;
    size_t cap;
    int used;
} ram_file_t;

static ram_file_t ram_files[RAM_FILES];

struct _FILE
{
    enum stream_kind kind;
    int open;

    /* STREAM_IMAGE */
    const unsigned char *image;
    size_t image_len;

    /* STREAM_RAM */
    ram_file_t *ram;

    size_t pos;
    int eof;
    int error;
    int append;

    /* STREAM_LOG */
    char pending[LOG_BUFFER];
    size_t pending_len;
};

static struct _FILE log_stream = { STREAM_LOG, 1, NULL, 0, NULL, 0, 0, 0, 0, { 0 }, 0 };
static struct _FILE open_files[8];

FILE *stdin = &log_stream;
FILE *stdout = &log_stream;
FILE *stderr = &log_stream;

/* ------------------------------------------------------------------ log --- */

static void log_flush(FILE *f)
{
    if (f->pending_len != 0)
    {
        ndos_write(f->pending, f->pending_len);
        f->pending_len = 0;
    }
}

/* The one place that writes a byte to a stream; printf.c calls it too. */
void __nodeos_stream_putc(FILE *f, char c)
{
    if (f == NULL)
    {
        return;
    }

    if (f->kind == STREAM_LOG)
    {
        f->pending[f->pending_len++] = c;
        if (c == '\n' || f->pending_len == sizeof(f->pending))
        {
            log_flush(f);
        }
        return;
    }
    fwrite(&c, 1, 1, f);
}

/* ------------------------------------------------------------- ram files --- */

static ram_file_t *ram_find(const char *name)
{
    for (int i = 0; i < RAM_FILES; i++)
    {
        if (ram_files[i].used && strcasecmp(ram_files[i].name, name) == 0)
        {
            return &ram_files[i];
        }
    }
    return NULL;
}

static ram_file_t *ram_create(const char *name)
{
    for (int i = 0; i < RAM_FILES; i++)
    {
        if (!ram_files[i].used)
        {
            ram_file_t *file = &ram_files[i];
            strlcpy(file->name, name, sizeof(file->name));
            file->data = malloc(RAM_INITIAL);
            if (file->data == NULL)
            {
                return NULL;
            }
            file->cap = RAM_INITIAL;
            file->len = 0;
            file->used = 1;
            return file;
        }
    }
    return NULL;
}

static int ram_reserve(ram_file_t *file, size_t need)
{
    if (need <= file->cap)
    {
        return 1;
    }
    size_t cap = file->cap;
    while (cap < need)
    {
        cap *= 2;
    }
    unsigned char *grown = realloc(file->data, cap);
    if (grown == NULL)
    {
        return 0;
    }
    file->data = grown;
    file->cap = cap;
    return 1;
}

/* ---------------------------------------------------------------- fopen --- */

/* Doom passes whole paths around ("/doom1.wad", "savegames/doomsav0.dsg").
   Nothing here has directories, so a name is matched on its last component. */
static const char *basename_of(const char *path)
{
    const char *slash = strrchr(path, '/');
    return slash != NULL ? slash + 1 : path;
}

static int has_suffix(const char *s, const char *suffix)
{
    size_t ls = strlen(s), lx = strlen(suffix);
    return ls >= lx && strcasecmp(s + ls - lx, suffix) == 0;
}

static FILE *alloc_stream(void)
{
    for (size_t i = 0; i < sizeof(open_files) / sizeof(open_files[0]); i++)
    {
        if (!open_files[i].open)
        {
            memset(&open_files[i], 0, sizeof(open_files[i]));
            open_files[i].open = 1;
            return &open_files[i];
        }
    }
    return NULL;
}

FILE *fopen(const char *path, const char *mode)
{
    if (path == NULL || mode == NULL)
    {
        return NULL;
    }

    const char *name = basename_of(path);
    int writing = strchr(mode, 'w') != NULL || strchr(mode, 'a') != NULL || strchr(mode, '+') != NULL;

    /* The IWAD: read-only, straight out of the granted mapping. */
    if (!writing && has_suffix(name, ".wad"))
    {
        size_t len = 0;
        const unsigned char *image = ndos_wad_data(&len);
        if (image == NULL)
        {
            errno = ENOENT;
            return NULL;
        }
        FILE *f = alloc_stream();
        if (f == NULL)
        {
            return NULL;
        }
        f->kind = STREAM_IMAGE;
        f->image = image;
        f->image_len = len;
        f->ram = NULL;
        f->pos = 0;
        f->eof = 0;
        return f;
    }

    ram_file_t *file = ram_find(name);

    if (file == NULL)
    {
        if (!writing)
        {
            errno = ENOENT;
            return NULL;
        }
        file = ram_create(name);
        if (file == NULL)
        {
            errno = ENOMEM;
            return NULL;
        }
    }

    if (strchr(mode, 'w') != NULL)
    {
        file->len = 0; /* truncate */
    }

    FILE *f = alloc_stream();
    if (f == NULL)
    {
        return NULL;
    }
    f->kind = STREAM_RAM;
    f->ram = file;
    f->append = strchr(mode, 'a') != NULL;
    f->pos = f->append ? file->len : 0;
    return f;
}

int fclose(FILE *f)
{
    if (f == NULL)
    {
        return EOF;
    }
    if (f->kind == STREAM_LOG)
    {
        log_flush(f);
        return 0;
    }
    f->open = 0;
    return 0;
}

/* --------------------------------------------------------------- access --- */

/* Size of whatever the stream is over. */
static size_t stream_length(FILE *f)
{
    switch (f->kind)
    {
    case STREAM_IMAGE: return f->image_len;
    case STREAM_RAM:   return f->ram->len;
    default:           return 0;
    }
}

size_t fread(void *ptr, size_t size, size_t count, FILE *f)
{
    if (f == NULL || size == 0 || count == 0)
    {
        return 0;
    }

    size_t want = size * count;
    size_t len = stream_length(f);
    const unsigned char *base = f->kind == STREAM_IMAGE ? f->image :
                                f->kind == STREAM_RAM   ? f->ram->data : NULL;

    if (base == NULL)
    {
        return 0;
    }
    if (f->pos >= len)
    {
        f->eof = 1;
        return 0;
    }
    if (want > len - f->pos)
    {
        want = len - f->pos;
        f->eof = 1;
    }
    memcpy(ptr, base + f->pos, want);
    f->pos += want;
    return want / size;
}

size_t fwrite(const void *ptr, size_t size, size_t count, FILE *f)
{
    if (f == NULL || size == 0 || count == 0)
    {
        return 0;
    }

    size_t want = size * count;

    if (f->kind == STREAM_LOG)
    {
        const char *bytes = ptr;
        for (size_t i = 0; i < want; i++)
        {
            __nodeos_stream_putc(f, bytes[i]);
        }
        return count;
    }
    if (f->kind != STREAM_RAM)
    {
        f->error = 1;
        return 0;
    }

    if (f->append)
    {
        f->pos = f->ram->len;
    }
    if (!ram_reserve(f->ram, f->pos + want))
    {
        f->error = 1;
        return 0;
    }
    memcpy(f->ram->data + f->pos, ptr, want);
    f->pos += want;
    if (f->pos > f->ram->len)
    {
        f->ram->len = f->pos;
    }
    return count;
}

int fseek(FILE *f, long offset, int whence)
{
    if (f == NULL || f->kind == STREAM_LOG)
    {
        return -1;
    }

    long base;
    switch (whence)
    {
    case SEEK_SET: base = 0; break;
    case SEEK_CUR: base = (long)f->pos; break;
    case SEEK_END: base = (long)stream_length(f); break;
    default:       return -1;
    }
    if (base + offset < 0)
    {
        return -1;
    }
    f->pos = (size_t)(base + offset);
    f->eof = 0;
    return 0;
}

long ftell(FILE *f)
{
    return f == NULL || f->kind == STREAM_LOG ? -1L : (long)f->pos;
}

void rewind(FILE *f)
{
    fseek(f, 0, SEEK_SET);
}

int feof(FILE *f)   { return f != NULL && f->eof; }
int ferror(FILE *f) { return f != NULL && f->error; }

int fflush(FILE *f)
{
    if (f != NULL && f->kind == STREAM_LOG)
    {
        log_flush(f);
    }
    return 0;
}

int fileno(FILE *f)
{
    return f == stdout ? 1 : f == stderr ? 2 : 3;
}

int fgetc(FILE *f)
{
    unsigned char c;
    return fread(&c, 1, 1, f) == 1 ? (int)c : EOF;
}

char *fgets(char *buf, int size, FILE *f)
{
    int i = 0;

    if (size <= 0)
    {
        return NULL;
    }
    while (i < size - 1)
    {
        int c = fgetc(f);
        if (c == EOF)
        {
            break;
        }
        buf[i++] = (char)c;
        if (c == '\n')
        {
            break;
        }
    }
    if (i == 0)
    {
        return NULL;
    }
    buf[i] = '\0';
    return buf;
}

int fputc(int c, FILE *f)
{
    char byte = (char)c;
    __nodeos_stream_putc(f, byte);
    return c;
}

int fputs(const char *s, FILE *f)
{
    while (*s)
    {
        __nodeos_stream_putc(f, *s++);
    }
    return 0;
}

int ungetc(int c, FILE *f)
{
    if (f == NULL || f->pos == 0)
    {
        return EOF;
    }
    f->pos--;
    f->eof = 0;
    return c;
}

int puts(const char *s)
{
    fputs(s, stdout);
    __nodeos_stream_putc(stdout, '\n');
    return 0;
}

int putchar(int c)
{
    __nodeos_stream_putc(stdout, (char)c);
    return c;
}

int remove(const char *path)
{
    ram_file_t *file = ram_find(basename_of(path));

    if (file == NULL)
    {
        return -1;
    }
    free(file->data);
    memset(file, 0, sizeof(*file));
    return 0;
}

int rename(const char *from, const char *to)
{
    ram_file_t *file = ram_find(basename_of(from));

    if (file == NULL)
    {
        return -1;
    }
    ram_file_t *existing = ram_find(basename_of(to));
    if (existing != NULL && existing != file)
    {
        free(existing->data);
        memset(existing, 0, sizeof(*existing));
    }
    strlcpy(file->name, basename_of(to), sizeof(file->name));
    return 0;
}

/* sscanf, for the integer conversions Doom actually uses.
 *
 * `M_StrToInt` tries "%x", "%X", "%o" and "%d" in turn against a string and
 * relies on the return value to say which matched, so a stub that always
 * returned 0 would not merely be incomplete -- it would leave the caller
 * reading an uninitialised int and calling it a parsed number.
 *
 * Supported: whitespace (matches any run, including none), literal
 * characters, and %d %i %u %x %X %o %c %s, with an optional field width and
 * the assignment-suppressing `*`. Returns the number of items assigned, or
 * EOF if the input ended before the first conversion. */
int sscanf(const char *buf, const char *fmt, ...)
{
    va_list ap;
    int assigned = 0;

    if (buf == NULL || fmt == NULL)
    {
        return EOF;
    }
    va_start(ap, fmt);

    for (; *fmt; fmt++)
    {
        if (isspace((unsigned char)*fmt))
        {
            while (isspace((unsigned char)*buf))
            {
                buf++;
            }
            continue;
        }
        if (*fmt != '%')
        {
            if (*buf != *fmt)
            {
                goto done;
            }
            buf++;
            continue;
        }

        fmt++;
        int suppress = 0;
        if (*fmt == '*')
        {
            suppress = 1;
            fmt++;
        }
        int width = 0;
        while (*fmt >= '0' && *fmt <= '9')
        {
            width = width * 10 + (*fmt++ - '0');
        }
        if (width == 0)
        {
            width = (int)((unsigned)-1 >> 1);
        }
        /* Length modifiers are accepted and ignored: every conversion here
           is assigned through an `int *`, which is what Doom passes. */
        while (*fmt == 'l' || *fmt == 'h')
        {
            fmt++;
        }

        if (*fmt == '%')
        {
            if (*buf != '%')
            {
                goto done;
            }
            buf++;
            continue;
        }

        if (*fmt == 'c')
        {
            if (*buf == '\0')
            {
                goto done;
            }
            if (!suppress)
            {
                *va_arg(ap, char *) = *buf;
                assigned++;
            }
            buf++;
            continue;
        }

        while (isspace((unsigned char)*buf))
        {
            buf++;
        }

        if (*fmt == 's')
        {
            if (*buf == '\0')
            {
                goto done;
            }
            char *out = suppress ? NULL : va_arg(ap, char *);
            int n = 0;
            while (*buf && !isspace((unsigned char)*buf) && n < width)
            {
                if (out != NULL)
                {
                    out[n] = *buf;
                }
                buf++;
                n++;
            }
            if (out != NULL)
            {
                out[n] = '\0';
                assigned++;
            }
            continue;
        }

        int base;
        switch (*fmt)
        {
        case 'd': case 'u': base = 10; break;
        case 'i': base = 0; break;  /* 0x -> hex, leading 0 -> octal */
        case 'x': case 'X': base = 16; break;
        case 'o': base = 8; break;
        default:  goto done;          /* unsupported conversion */
        }

        /* strtol does the parsing, but it would happily run past `width`;
           the widths Doom uses are unbounded, so copy only when one is set. */
        char bounded[32];
        const char *start = buf;
        if (width < (int)sizeof(bounded))
        {
            int n = 0;
            while (start[n] && n < width)
            {
                bounded[n] = start[n];
                n++;
            }
            bounded[n] = '\0';
            start = bounded;
        }

        char *end = NULL;
        long value = strtol(start, &end, base);
        if (end == start)
        {
            goto done;  /* nothing numeric here */
        }
        buf += end - start;
        if (!suppress)
        {
            *va_arg(ap, int *) = (int)value;
            assigned++;
        }
    }

done:
    va_end(ap);
    return assigned == 0 && *buf == '\0' ? EOF : assigned;
}
