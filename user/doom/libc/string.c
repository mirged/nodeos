/* String functions.
 *
 * memcpy, memmove, memset, memcmp and strlen are deliberately absent: every
 * NodeOS program already links libnode's versions of them (libnode/src/rt.rs
 * exists because the compiler emits calls to those names and there is no
 * libc), and a stable toolchain has no weak linkage, so defining them again
 * here would be a link error rather than an override.
 */
#include <stddef.h>
#include <stdint.h>
#include <string.h>
#include <stdlib.h>
#include <ctype.h>

void *memchr(const void *s, int c, size_t n)
{
    const unsigned char *p = s;

    while (n--)
    {
        if (*p == (unsigned char)c)
        {
            return (void *)p;
        }
        p++;
    }
    return NULL;
}

char *strcpy(char *dst, const char *src)
{
    char *d = dst;

    while ((*d++ = *src++) != '\0')
    {
    }
    return dst;
}

char *strncpy(char *dst, const char *src, size_t n)
{
    size_t i = 0;

    for (; i < n && src[i]; i++)
    {
        dst[i] = src[i];
    }
    for (; i < n; i++)
    {
        dst[i] = '\0';
    }
    return dst;
}

char *strcat(char *dst, const char *src)
{
    strcpy(dst + strlen(dst), src);
    return dst;
}

char *strncat(char *dst, const char *src, size_t n)
{
    char *d = dst + strlen(dst);

    while (n-- && *src)
    {
        *d++ = *src++;
    }
    *d = '\0';
    return dst;
}

int strcmp(const char *a, const char *b)
{
    while (*a && *a == *b)
    {
        a++;
        b++;
    }
    return (int)(unsigned char)*a - (int)(unsigned char)*b;
}

int strncmp(const char *a, const char *b, size_t n)
{
    while (n && *a && *a == *b)
    {
        a++;
        b++;
        n--;
    }
    if (n == 0)
    {
        return 0;
    }
    return (int)(unsigned char)*a - (int)(unsigned char)*b;
}

int strcasecmp(const char *a, const char *b)
{
    while (*a && tolower((unsigned char)*a) == tolower((unsigned char)*b))
    {
        a++;
        b++;
    }
    return tolower((unsigned char)*a) - tolower((unsigned char)*b);
}

int strncasecmp(const char *a, const char *b, size_t n)
{
    while (n && *a && tolower((unsigned char)*a) == tolower((unsigned char)*b))
    {
        a++;
        b++;
        n--;
    }
    if (n == 0)
    {
        return 0;
    }
    return tolower((unsigned char)*a) - tolower((unsigned char)*b);
}

char *strchr(const char *s, int c)
{
    for (;; s++)
    {
        if (*s == (char)c)
        {
            return (char *)s;
        }
        if (*s == '\0')
        {
            return NULL;
        }
    }
}

char *strrchr(const char *s, int c)
{
    const char *found = NULL;

    for (;; s++)
    {
        if (*s == (char)c)
        {
            found = s;
        }
        if (*s == '\0')
        {
            return (char *)found;
        }
    }
}

char *strstr(const char *haystack, const char *needle)
{
    size_t n = strlen(needle);

    if (n == 0)
    {
        return (char *)haystack;
    }
    for (; *haystack; haystack++)
    {
        if (strncmp(haystack, needle, n) == 0)
        {
            return (char *)haystack;
        }
    }
    return NULL;
}

char *strdup(const char *s)
{
    size_t n = strlen(s) + 1;
    char *copy = malloc(n);

    if (copy != NULL)
    {
        memcpy(copy, s, n);
    }
    return copy;
}

size_t strspn(const char *s, const char *accept)
{
    const char *p = s;

    while (*p && strchr(accept, *p) != NULL)
    {
        p++;
    }
    return (size_t)(p - s);
}

char *strpbrk(const char *s, const char *accept)
{
    for (; *s; s++)
    {
        if (strchr(accept, *s) != NULL)
        {
            return (char *)s;
        }
    }
    return NULL;
}

char *strtok(char *s, const char *delim)
{
    static char *saved;
    char *start;

    if (s == NULL)
    {
        s = saved;
    }
    if (s == NULL)
    {
        return NULL;
    }
    s += strspn(s, delim);
    if (*s == '\0')
    {
        saved = NULL;
        return NULL;
    }
    start = s;
    s = strpbrk(start, delim);
    if (s == NULL)
    {
        saved = NULL;
    }
    else
    {
        *s = '\0';
        saved = s + 1;
    }
    return start;
}

size_t strlcpy(char *dst, const char *src, size_t n)
{
    size_t len = strlen(src);

    if (n != 0)
    {
        size_t copy = len < n - 1 ? len : n - 1;
        memcpy(dst, src, copy);
        dst[copy] = '\0';
    }
    return len;
}

size_t strlcat(char *dst, const char *src, size_t n)
{
    size_t used = strlen(dst);

    if (used >= n)
    {
        return n + strlen(src);
    }
    return used + strlcpy(dst + used, src, n - used);
}

char *strerror(int errnum)
{
    (void)errnum;
    return "error";
}
