/* Process control, number parsing, sorting, and the few POSIX-shaped calls
   the Doom tree makes. */
#include <stddef.h>
#include <stdint.h>
#include <stdlib.h>
#include <stdio.h>
#include <string.h>
#include <ctype.h>
#include <limits.h>
#include <time.h>
#include <sys/time.h>
#include <sys/stat.h>
#include <unistd.h>

#include "nodeos_platform.h"

/* ------------------------------------------------------ process control --- */

#define ATEXIT_MAX 16

static void (*atexit_funcs[ATEXIT_MAX])(void);
static int atexit_count;

int atexit(void (*func)(void))
{
    if (atexit_count == ATEXIT_MAX)
    {
        return -1;
    }
    atexit_funcs[atexit_count++] = func;
    return 0;
}

void exit(int code)
{
    /* Registered handlers run in reverse order, as C requires. */
    while (atexit_count > 0)
    {
        atexit_funcs[--atexit_count]();
    }
    fflush(stdout);
    ndos_exit(code);
}

void abort(void)
{
    fputs("doom: abort()\n", stderr);
    fflush(stderr);
    ndos_exit(134);
}

void __nodeos_assert_fail(const char *expr, const char *file, int line)
{
    fprintf(stderr, "doom: assertion failed: %s at %s:%d\n", expr, file, line);
    fflush(stderr);
    ndos_exit(134);
}

/* Doom consults DOOMWADDIR, DOOMWADPATH and TEMP. This process has no
   environment: the WAD arrives as a capability, not as a path to search. */
char *getenv(const char *name)
{
    (void)name;
    return NULL;
}

/* ------------------------------------------------------- number parsing --- */

long strtol(const char *s, char **end, int base)
{
    const char *p = s;
    int negative = 0;
    long value = 0;

    while (isspace((unsigned char)*p))
    {
        p++;
    }
    if (*p == '+' || *p == '-')
    {
        negative = *p++ == '-';
    }
    if ((base == 0 || base == 16) && p[0] == '0' && (p[1] == 'x' || p[1] == 'X'))
    {
        p += 2;
        base = 16;
    }
    else if (base == 0)
    {
        base = p[0] == '0' ? 8 : 10;
    }

    const char *digits_start = p;
    for (;; p++)
    {
        int digit;
        if (isdigit((unsigned char)*p))       digit = *p - '0';
        else if (isalpha((unsigned char)*p))  digit = tolower((unsigned char)*p) - 'a' + 10;
        else                                  break;

        if (digit >= base)
        {
            break;
        }
        value = value * base + digit;
    }

    if (end != NULL)
    {
        *end = (char *)(p == digits_start ? s : p);
    }
    return negative ? -value : value;
}

unsigned long strtoul(const char *s, char **end, int base)
{
    return (unsigned long)strtol(s, end, base);
}

int atoi(const char *s)
{
    return (int)strtol(s, NULL, 10);
}

long atol(const char *s)
{
    return strtol(s, NULL, 10);
}

int abs(int v)   { return v < 0 ? -v : v; }
long labs(long v) { return v < 0 ? -v : v; }

/* Doom seeds and uses its own generator (m_random.c); these exist only so
   that a stray reference links. */
static unsigned long rand_state = 1;

int rand(void)
{
    rand_state = rand_state * 1103515245 + 12345;
    return (int)((rand_state >> 16) & 0x7fff);
}

void srand(unsigned int seed)
{
    rand_state = seed;
}

/* --------------------------------------------------------------- qsort --- */

static void swap_bytes(char *a, char *b, size_t size)
{
    while (size--)
    {
        char t = *a;
        *a++ = *b;
        *b++ = t;
    }
}

/* Insertion sort. The only caller in the tree sorts a WAD's lump-name hash
   input -- a few thousand elements once, at load. */
void qsort(void *base, size_t count, size_t size,
           int (*compare)(const void *, const void *))
{
    char *items = base;

    for (size_t i = 1; i < count; i++)
    {
        for (size_t j = i; j > 0 && compare(items + (j - 1) * size, items + j * size) > 0; j--)
        {
            swap_bytes(items + (j - 1) * size, items + j * size, size);
        }
    }
}

/* ------------------------------------------------------- time and POSIX --- */

time_t time(time_t *t)
{
    /* There is no wall clock on this machine, only the monotonic one. Doom
       uses this to seed a name for a screenshot and nothing else. */
    time_t now = (time_t)(ndos_time_ns() / 1000000000ULL);

    if (t != NULL)
    {
        *t = now;
    }
    return now;
}

static struct tm broken_down;

struct tm *localtime(const time_t *t)
{
    long seconds = t != NULL ? *t : 0;

    memset(&broken_down, 0, sizeof(broken_down));
    broken_down.tm_sec = (int)(seconds % 60);
    broken_down.tm_min = (int)((seconds / 60) % 60);
    broken_down.tm_hour = (int)((seconds / 3600) % 24);
    broken_down.tm_mday = 1;
    broken_down.tm_year = 70; /* the epoch this machine actually knows: boot */
    return &broken_down;
}

size_t strftime(char *buf, size_t max, const char *fmt, const struct tm *tm)
{
    (void)fmt;
    return (size_t)snprintf(buf, max, "%02d:%02d:%02d", tm->tm_hour, tm->tm_min, tm->tm_sec);
}

int gettimeofday(struct timeval *tv, struct timezone *tz)
{
    uint64_t ns = ndos_time_ns();

    (void)tz;
    if (tv != NULL)
    {
        tv->tv_sec = (time_t)(ns / 1000000000ULL);
        tv->tv_usec = (long)((ns % 1000000000ULL) / 1000);
    }
    return 0;
}

/* The log is not a terminal: I_ConsoleStdout uses this to decide whether to
   print the ENDOOM text screen with ANSI colours. */
int isatty(int fd)
{
    (void)fd;
    return 0;
}

/* There are no directories; savegames are keyed by name alone (see stdio.c). */
int mkdir(const char *path, mode_t mode)
{
    (void)path;
    (void)mode;
    return 0;
}

int access(const char *path, int mode)
{
    (void)path;
    (void)mode;
    return -1;
}
