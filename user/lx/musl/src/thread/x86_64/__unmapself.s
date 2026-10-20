/* NodeOS: free this thread's stack and exit. Unreachable until threads
 * exist; kept so that the archive builds and nothing issues `syscall`. */
.text
.global __unmapself
.type   __unmapself,@function
__unmapself:
	mov %rsi,%rdx           /* size */
	mov %rdi,%rsi           /* base */
	mov $11,%edi            /* SYS_munmap */
	call __lx_syscall
	xor %esi,%esi
	mov $60,%edi            /* SYS_exit */
	call __lx_syscall
	ud2
