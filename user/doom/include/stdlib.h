#ifndef _STDLIB_H
#define _STDLIB_H
#include <stddef.h>

#define EXIT_SUCCESS 0
#define EXIT_FAILURE 1
#define RAND_MAX 32767

void *malloc(size_t size);
void *calloc(size_t count, size_t size);
void *realloc(void *ptr, size_t size);
void free(void *ptr);

void exit(int code) __attribute__((noreturn));
void abort(void) __attribute__((noreturn));
int atexit(void (*func)(void));

int atoi(const char *s);
long atol(const char *s);
long strtol(const char *s, char **end, int base);
unsigned long strtoul(const char *s, char **end, int base);

int abs(int v);
long labs(long v);

char *getenv(const char *name);

void qsort(void *base, size_t count, size_t size,
           int (*compare)(const void *, const void *));

int rand(void);
void srand(unsigned int seed);
#endif
