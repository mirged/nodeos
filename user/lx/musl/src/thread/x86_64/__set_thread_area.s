/* NodeOS: arch_prctl(ARCH_SET_FS, p), through liblx. */
.text
.global __set_thread_area
.hidden __set_thread_area
.type __set_thread_area,@function
__set_thread_area:
	mov %rdi,%rdx           /* p                     */
	mov $0x1002,%esi        /* ARCH_SET_FS           */
	mov $158,%edi           /* SYS_arch_prctl        */
	jmp __lx_syscall
