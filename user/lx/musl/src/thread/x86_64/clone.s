/* NodeOS: __clone(func, stack, flags, arg, ptid, tls, ctid).
 *
 * Threads are not wired up yet (docs/MUSL.md, phase 1 threads), so this
 * reports ENOSYS and pthread_create fails cleanly with EAGAIN rather than
 * reaching the kernel with a Linux syscall number. */
.text
.global __clone
.hidden __clone
.type   __clone,@function
__clone:
	mov $-38,%eax           /* -ENOSYS */
	ret
