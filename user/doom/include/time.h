#ifndef _TIME_H
#define _TIME_H
#include <stddef.h>
typedef long time_t;
struct tm {
    int tm_sec, tm_min, tm_hour, tm_mday, tm_mon, tm_year, tm_wday, tm_yday, tm_isdst;
};
time_t time(time_t *t);
struct tm *localtime(const time_t *t);
size_t strftime(char *buf, size_t max, const char *fmt, const struct tm *tm);
#endif
