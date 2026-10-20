/* NodeOS: the signal-return trampoline, through liblx. */
.global __restore_rt
.hidden __restore_rt
.type __restore_rt,@function
__restore_rt:
	mov $15,%edi            /* SYS_rt_sigreturn */
	call __lx_syscall
	ud2
.size __restore_rt,.-__restore_rt
