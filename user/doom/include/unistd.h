#ifndef _UNISTD_H
#define _UNISTD_H
#include <stddef.h>
#define STDIN_FILENO  0
#define STDOUT_FILENO 1
#define STDERR_FILENO 2
int isatty(int fd);
int access(const char *path, int mode);
#define F_OK 0
#define R_OK 4
#define W_OK 2
#define X_OK 1
#endif
