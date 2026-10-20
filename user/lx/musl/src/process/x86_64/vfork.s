/* NodeOS: vfork, through liblx (which reports ENOSYS for now). */
.global vfork
.type vfork,@function
vfork:
	sub $8,%rsp
	mov $58,%edi            /* SYS_vfork */
	call __lx_syscall
	add $8,%rsp
	mov %rax,%rdi
	.hidden __syscall_ret
	jmp __syscall_ret
