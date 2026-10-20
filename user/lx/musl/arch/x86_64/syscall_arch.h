/* NodeOS: musl's syscall layer, rerouted.
 *
 * Upstream issues a `syscall` instruction here. On NodeOS that instruction
 * reaches a kernel whose numbering is unrelated to Linux's (Linux 1 is
 * `write`, NodeOS 1 is `process_create`), so instead every call becomes an
 * ordinary function call into liblx, which answers it in-process with NodeOS
 * handles. Everything else in musl keeps the Linux numbering and structure
 * layouts it was written against. See docs/MUSL.md. */

#define __SYSCALL_LL_E(x) (x)
#define __SYSCALL_LL_O(x) (x)

long __lx_syscall(long, long, long, long, long, long, long);

static __inline long __syscall0(long n)
{
	return __lx_syscall(n, 0, 0, 0, 0, 0, 0);
}

static __inline long __syscall1(long n, long a1)
{
	return __lx_syscall(n, a1, 0, 0, 0, 0, 0);
}

static __inline long __syscall2(long n, long a1, long a2)
{
	return __lx_syscall(n, a1, a2, 0, 0, 0, 0);
}

static __inline long __syscall3(long n, long a1, long a2, long a3)
{
	return __lx_syscall(n, a1, a2, a3, 0, 0, 0);
}

static __inline long __syscall4(long n, long a1, long a2, long a3, long a4)
{
	return __lx_syscall(n, a1, a2, a3, a4, 0, 0);
}

static __inline long __syscall5(long n, long a1, long a2, long a3, long a4, long a5)
{
	return __lx_syscall(n, a1, a2, a3, a4, a5, 0);
}

static __inline long __syscall6(long n, long a1, long a2, long a3, long a4, long a5, long a6)
{
	return __lx_syscall(n, a1, a2, a3, a4, a5, a6);
}

/* No vDSO: there is no kernel-provided ELF to find `clock_gettime` in, and
 * liblx's answer is already a plain function call. */

#define IPC_64 0
