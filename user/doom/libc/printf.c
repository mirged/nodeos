/* The printf family.
 *
 * One engine (`format`) drives every entry point by way of a sink, so the
 * bounded, unbounded and stream variants cannot drift apart. Conversions
 * implemented are the ones the Doom tree actually uses: d i u x X o c s p %,
 * with the '-', '0', '+', ' ' and '#' flags, a field width, a precision, and
 * the h/hh/l/ll/z length modifiers.
 *
 * There is no %f, %e or %g. The port is built -mgeneral-regs-only (no SSE, so
 * no floating point at all -- see user/doom/build.rs), and the handful of
 * float conversions in the original tree are in code paths this port removes.
 */
#include <stdarg.h>
#include <stddef.h>
#include <stdint.h>
#include <stdio.h>
#include <string.h>

#include "nodeos_platform.h"

/* Where a formatted character goes. `limit == 0` means "count only". */
typedef struct
{
    char *buf;
    size_t limit;
    size_t written;  /* what would have been written, ignoring the limit */
    FILE *stream;    /* when non-NULL, characters go here instead */
} sink_t;

/* Defined in stdio.c: the one place that knows how to push bytes at a FILE. */
void __nodeos_stream_putc(FILE *f, char c);

static void emit(sink_t *sink, char c)
{
    if (sink->stream != NULL)
    {
        __nodeos_stream_putc(sink->stream, c);
    }
    else if (sink->buf != NULL && sink->written + 1 < sink->limit)
    {
        sink->buf[sink->written] = c;
    }
    sink->written++;
}

static void emit_str(sink_t *sink, const char *s, size_t n)
{
    while (n--)
    {
        emit(sink, *s++);
    }
}

static void emit_pad(sink_t *sink, char c, int n)
{
    while (n-- > 0)
    {
        emit(sink, c);
    }
}

#define FLAG_LEFT  (1 << 0)
#define FLAG_ZERO  (1 << 1)
#define FLAG_PLUS  (1 << 2)
#define FLAG_SPACE (1 << 3)
#define FLAG_ALT   (1 << 4)

/* Render `value` in `base` into `out` (which must hold at least 24 bytes),
   returning its length. Digits are produced least significant first and then
   reversed, which is shorter than working out the length up front. */
static int render_unsigned(unsigned long long value, unsigned base, int upper, char *out)
{
    const char *digits = upper ? "0123456789ABCDEF" : "0123456789abcdef";
    char tmp[24];
    int n = 0;

    do
    {
        tmp[n++] = digits[value % base];
        value /= base;
    } while (value != 0);

    for (int i = 0; i < n; i++)
    {
        out[i] = tmp[n - 1 - i];
    }
    return n;
}

static void format(sink_t *sink, const char *fmt, va_list ap)
{
    for (; *fmt; fmt++)
    {
        if (*fmt != '%')
        {
            emit(sink, *fmt);
            continue;
        }

        fmt++;
        if (*fmt == '\0')
        {
            break;
        }

        /* --- flags --- */
        int flags = 0;
        for (;; fmt++)
        {
            if (*fmt == '-')      flags |= FLAG_LEFT;
            else if (*fmt == '0') flags |= FLAG_ZERO;
            else if (*fmt == '+') flags |= FLAG_PLUS;
            else if (*fmt == ' ') flags |= FLAG_SPACE;
            else if (*fmt == '#') flags |= FLAG_ALT;
            else break;
        }

        /* --- width --- */
        int width = 0;
        if (*fmt == '*')
        {
            width = va_arg(ap, int);
            if (width < 0)
            {
                flags |= FLAG_LEFT;
                width = -width;
            }
            fmt++;
        }
        else
        {
            while (*fmt >= '0' && *fmt <= '9')
            {
                width = width * 10 + (*fmt++ - '0');
            }
        }

        /* --- precision --- */
        int precision = -1;
        if (*fmt == '.')
        {
            fmt++;
            precision = 0;
            if (*fmt == '*')
            {
                precision = va_arg(ap, int);
                fmt++;
            }
            else
            {
                while (*fmt >= '0' && *fmt <= '9')
                {
                    precision = precision * 10 + (*fmt++ - '0');
                }
            }
            if (precision < 0)
            {
                precision = -1;
            }
        }

        /* --- length modifier --- */
        enum { LEN_INT, LEN_CHAR, LEN_SHORT, LEN_LONG, LEN_LLONG, LEN_SIZE } length = LEN_INT;
        if (*fmt == 'h')
        {
            fmt++;
            length = LEN_SHORT;
            if (*fmt == 'h')
            {
                fmt++;
                length = LEN_CHAR;
            }
        }
        else if (*fmt == 'l')
        {
            fmt++;
            length = LEN_LONG;
            if (*fmt == 'l')
            {
                fmt++;
                length = LEN_LLONG;
            }
        }
        else if (*fmt == 'z')
        {
            fmt++;
            length = LEN_SIZE;
        }

        /* --- conversion --- */
        char conv = *fmt;
        char body[24];
        int body_len = 0;
        const char *prefix = "";
        int prefix_len = 0;
        char sign = '\0';

        switch (conv)
        {
        case '%':
            emit(sink, '%');
            continue;

        case 'c':
        {
            char c = (char)va_arg(ap, int);
            if (!(flags & FLAG_LEFT))
            {
                emit_pad(sink, ' ', width - 1);
            }
            emit(sink, c);
            if (flags & FLAG_LEFT)
            {
                emit_pad(sink, ' ', width - 1);
            }
            continue;
        }

        case 's':
        {
            const char *s = va_arg(ap, const char *);
            if (s == NULL)
            {
                s = "(null)";
            }
            size_t len = strlen(s);
            if (precision >= 0 && (size_t)precision < len)
            {
                len = (size_t)precision;
            }
            if (!(flags & FLAG_LEFT))
            {
                emit_pad(sink, ' ', width - (int)len);
            }
            emit_str(sink, s, len);
            if (flags & FLAG_LEFT)
            {
                emit_pad(sink, ' ', width - (int)len);
            }
            continue;
        }

        case 'd':
        case 'i':
        {
            long long value;
            switch (length)
            {
            case LEN_CHAR:  value = (signed char)va_arg(ap, int); break;
            case LEN_SHORT: value = (short)va_arg(ap, int); break;
            case LEN_LONG:  value = va_arg(ap, long); break;
            case LEN_LLONG: value = va_arg(ap, long long); break;
            case LEN_SIZE:  value = (long long)va_arg(ap, size_t); break;
            default:        value = va_arg(ap, int); break;
            }
            unsigned long long magnitude =
                value < 0 ? (unsigned long long)-(value + 1) + 1 : (unsigned long long)value;
            if (value < 0)         sign = '-';
            else if (flags & FLAG_PLUS)  sign = '+';
            else if (flags & FLAG_SPACE) sign = ' ';
            body_len = render_unsigned(magnitude, 10, 0, body);
            break;
        }

        case 'u':
        case 'x':
        case 'X':
        case 'o':
        case 'p':
        {
            unsigned long long value;
            unsigned base = 10;
            int upper = 0;

            if (conv == 'p')
            {
                value = (unsigned long long)(uintptr_t)va_arg(ap, void *);
                base = 16;
                prefix = "0x";
                prefix_len = 2;
            }
            else
            {
                switch (length)
                {
                case LEN_CHAR:  value = (unsigned char)va_arg(ap, unsigned int); break;
                case LEN_SHORT: value = (unsigned short)va_arg(ap, unsigned int); break;
                case LEN_LONG:  value = va_arg(ap, unsigned long); break;
                case LEN_LLONG: value = va_arg(ap, unsigned long long); break;
                case LEN_SIZE:  value = va_arg(ap, size_t); break;
                default:        value = va_arg(ap, unsigned int); break;
                }
                if (conv == 'x') { base = 16; }
                if (conv == 'X') { base = 16; upper = 1; }
                if (conv == 'o') { base = 8; }
                if ((flags & FLAG_ALT) && value != 0)
                {
                    if (base == 16) { prefix = upper ? "0X" : "0x"; prefix_len = 2; }
                    if (base == 8)  { prefix = "0"; prefix_len = 1; }
                }
            }
            body_len = render_unsigned(value, base, upper, body);
            break;
        }

        default:
            /* An unknown conversion is reproduced verbatim rather than
               silently swallowing an argument we cannot interpret. */
            emit(sink, '%');
            emit(sink, conv);
            continue;
        }

        /* --- pad and emit the numeric conversions --- */
        int zeros = precision > body_len ? precision - body_len : 0;
        /* '0' padding is ignored when a precision is given, as C requires. */
        if ((flags & FLAG_ZERO) && !(flags & FLAG_LEFT) && precision < 0)
        {
            zeros = width - body_len - prefix_len - (sign ? 1 : 0);
            if (zeros < 0)
            {
                zeros = 0;
            }
        }
        int total = body_len + zeros + prefix_len + (sign ? 1 : 0);

        if (!(flags & FLAG_LEFT))
        {
            emit_pad(sink, ' ', width - total);
        }
        if (sign)
        {
            emit(sink, sign);
        }
        emit_str(sink, prefix, (size_t)prefix_len);
        emit_pad(sink, '0', zeros);
        emit_str(sink, body, (size_t)body_len);
        if (flags & FLAG_LEFT)
        {
            emit_pad(sink, ' ', width - total);
        }
    }
}

int vsnprintf(char *buf, size_t size, const char *fmt, va_list ap)
{
    sink_t sink = { buf, size, 0, NULL };

    format(&sink, fmt, ap);
    if (buf != NULL && size != 0)
    {
        buf[sink.written < size ? sink.written : size - 1] = '\0';
    }
    return (int)sink.written;
}

int vsprintf(char *buf, const char *fmt, va_list ap)
{
    return vsnprintf(buf, (size_t)-1, fmt, ap);
}

int snprintf(char *buf, size_t size, const char *fmt, ...)
{
    va_list ap;
    va_start(ap, fmt);
    int n = vsnprintf(buf, size, fmt, ap);
    va_end(ap);
    return n;
}

int sprintf(char *buf, const char *fmt, ...)
{
    va_list ap;
    va_start(ap, fmt);
    int n = vsnprintf(buf, (size_t)-1, fmt, ap);
    va_end(ap);
    return n;
}

int vfprintf(FILE *f, const char *fmt, va_list ap)
{
    sink_t sink = { NULL, 0, 0, f };

    format(&sink, fmt, ap);
    return (int)sink.written;
}

int fprintf(FILE *f, const char *fmt, ...)
{
    va_list ap;
    va_start(ap, fmt);
    int n = vfprintf(f, fmt, ap);
    va_end(ap);
    return n;
}

int vprintf(const char *fmt, va_list ap)
{
    return vfprintf(stdout, fmt, ap);
}

int printf(const char *fmt, ...)
{
    va_list ap;
    va_start(ap, fmt);
    int n = vfprintf(stdout, fmt, ap);
    va_end(ap);
    return n;
}
