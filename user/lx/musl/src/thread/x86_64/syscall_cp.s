/* NodeOS: a cancellable syscall, through liblx.
 *
 * __syscall_cp_asm(&cancel, nr, u, v, w, x, y, z) becomes
 * __lx_syscall(nr, u, v, w, x, y, z). The cancellation check stays: a
 * pending cancel still jumps to __cancel before the call. The __cp_begin /
 * __cp_end range is kept for pthread_cancel's benefit, though nothing on
 * NodeOS can interrupt a thread inside it yet. */
.text
.global __cp_begin
.hidden __cp_begin
.global __cp_end
.hidden __cp_end
.global __cp_cancel
.hidden __cp_cancel
.hidden __cancel
.global __syscall_cp_asm
.hidden __syscall_cp_asm
.type   __syscall_cp_asm,@function
__syscall_cp_asm:

__cp_begin:
	mov (%rdi),%eax
	test %eax,%eax
	jnz __cp_cancel
	mov 16(%rsp),%rax       /* z: the one argument that goes on the stack */
	mov 8(%rsp),%r10        /* y                                          */
	mov %rsi,%rdi           /* nr                                         */
	mov %rdx,%rsi           /* u                                          */
	mov %rcx,%rdx           /* v                                          */
	mov %r8,%rcx            /* w                                          */
	mov %r9,%r8             /* x                                          */
	mov %r10,%r9            /* y                                          */
	push %rax               /* z; also realigns the stack for the call    */
	call __lx_syscall
	add $8,%rsp
__cp_end:
	ret
__cp_cancel:
	jmp __cancel
