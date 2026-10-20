/* The first musl program on NodeOS: stdio, the heap, floating-point
 * formatting, errno through TLS, and a file round trip. */
#include <errno.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

int main(int argc, char **argv)
{
	printf("hello from musl on NodeOS\n");
	printf("argc=%d argv[0]=%s\n", argc, argv[0]);
	printf("float: %.3f %g\n", 3.14159265, 1e-5 * 3);

	char *p = malloc(1 << 20);
	memset(p, 'x', 1 << 20);
	printf("malloc: %s\n", p[(1 << 20) - 1] == 'x' ? "ok" : "bad");
	free(p);

	FILE *f = fopen("/nope/missing.txt", "r");
	printf("fopen missing: %s (errno %d)\n", f ? "opened?" : strerror(errno), errno);
	return 0;
}
