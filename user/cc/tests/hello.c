/* hello.c -- a smoke test for the NodeOS C compiler and its standard library.
 *
 * It exercises functions and recursion, `for` and `while`, compound
 * assignment, prefix and postfix `++`/`--`, a global array and pointer
 * arithmetic into it, string literals and the string builtins, and `malloc`.
 * The expected output is checked by `make test-cc`.
 */

int squares[8];

int fib(int n) {
    if (n < 2) return n;
    return fib(n - 1) + fib(n - 2);
}

int factorial(int n) {
    int result = 1;
    while (n > 1) {
        result = result * n;
        n--;
    }
    return result;
}

int main() {
    int i;
    int total = 0;

    printf("hello, world\n");

    for (i = 1; i <= 10; i++) {
        total += i;
    }
    printf("sum 1..10 = %d\n", total);
    printf("fib(10)   = %d\n", fib(10));
    printf("5!        = %d\n", factorial(5));

    for (i = 0; i < 8; i++) {
        squares[i] = i * i;
    }
    printf("squares   =");
    for (i = 0; i < 8; i++) {
        printf(" %d", squares[i]);
    }
    printf("\n");

    char *name = "nodeos";
    printf("strlen    = %d\n", strlen(name));

    int *p = malloc(8);
    *p = 6 * 7;
    printf("heap      = %d\n", *p);
    free(p);

    return 0;
}
