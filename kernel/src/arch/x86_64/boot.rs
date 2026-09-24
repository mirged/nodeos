//! Multiboot2 entry trampoline: 32-bit protected mode -> 64-bit long mode.
//!
//! # Why this is hand-written assembly
//!
//! A Multiboot2 loader hands control to the kernel in 32-bit protected mode
//! with paging disabled. No Rust code can run in that state (the compiler is
//! generating 64-bit code and every static lives at a non-canonical-for-32-bit
//! address), so the transition has to be assembly. Everything here runs
//! single-threaded, with interrupts disabled, before any kernel invariant
//! exists, which is precisely why it is quarantined in one file.
//!
//! # Bootstrap page tables
//!
//! Nine 4 KiB pages are reserved by the linker script at `__boot_pt_start`:
//!
//! ```text
//!   +0x0000  PML4
//!   +0x1000  PDPT covering  0x0000_0000_0000_0000  (identity, low 512 GiB slot)
//!   +0x2000  PD   covering  0..1 GiB with 2 MiB pages (shared, see below)
//!   +0x3000  PDPT covering  0xFFFF_FF80_0000_0000  (kernel high half)
//!   +0x4000  PDPT covering  0xFFFF_8000_0000_0000  (direct physical map)
//!   +0x5000  PD   physmap 0..1 GiB
//!   +0x6000  PD   physmap 1..2 GiB
//!   +0x7000  PD   physmap 2..3 GiB
//!   +0x8000  PD   physmap 3..4 GiB
//! ```
//!
//! The identity PD at +0x2000 is deliberately reused for the kernel window at
//! `0xFFFF_FFFF_8000_0000`, which is why `virt == phys + KERNEL_VMA` holds for
//! the whole image: PML4[511] -> PDPT_high, PDPT_high[510] -> PD(0..1 GiB).
//!
//! The identity mapping is torn down by [`crate::mm::vm`] once the kernel is
//! running on its own page tables; it exists only so the trampoline does not
//! fault the instruction after `mov cr0, eax`.

use core::arch::global_asm;

/// Value Multiboot2 loaders leave in `EAX` before jumping to the kernel.
pub const MULTIBOOT2_BOOTLOADER_MAGIC: u32 = 0x36d7_6289;

/// Virtual address of the start of the kernel image window.
pub const KERNEL_VMA: u64 = 0xFFFF_FFFF_8000_0000;

/// Base of the direct map of physical memory.
pub const PHYSMAP_BASE: u64 = 0xFFFF_8000_0000_0000;

/// How much physical memory the bootstrap tables direct-map (4 GiB).
pub const BOOT_PHYSMAP_LEN: u64 = 4 * 1024 * 1024 * 1024;

global_asm!(
    r#"
/* ------------------------------------------------------------------ */
/* Multiboot2 header. Must be 8-byte aligned and within the first      */
/* 32 KiB of the file; the linker script places `.multiboot_header`    */
/* first inside the first PT_LOAD segment.                             */
/* ------------------------------------------------------------------ */
.section .multiboot_header, "a"
.balign 8
.Lmb2_start:
    .long   0xE85250D6                              /* magic            */
    .long   0                                       /* arch: i386       */
    .long   .Lmb2_end - .Lmb2_start                 /* header length    */
    .long   -(0xE85250D6 + 0 + (.Lmb2_end - .Lmb2_start))  /* checksum  */

    /* Tag 6: module alignment. Boot modules (userspace NXF images) are
       page aligned so they can be mapped directly instead of copied.   */
    .balign 8
    .short  6
    .short  0
    .long   8

    /* Tag 5: framebuffer request. Asks the loader for a linear graphics
       mode; GRUB substitutes the closest mode it can set (via VBE on
       BIOS, or via GOP if GRUB itself was booted through UEFI) and
       reports the actual geometry in the matching Multiboot2 info tag
       (parsed by `crate::boot::multiboot2::BootInfo::framebuffer`).      */
    .balign 8
    .short  5
    .short  0
    .long   20
    .long   1024                                     /* width            */
    .long   768                                      /* height           */
    .long   32                                       /* depth (bpp)      */

    /* Tag 0: end of tags. */
    .balign 8
    .short  0
    .short  0
    .long   8
.Lmb2_end:

/* ------------------------------------------------------------------ */
/* 32-bit trampoline                                                    */
/* ------------------------------------------------------------------ */
.section .boot.text, "ax"
.code32
.globl _boot_entry32
.type _boot_entry32, @function
_boot_entry32:
    cli
    cld

    /* The loader guarantees: EAX = 0x36d76289, EBX = &multiboot_info.
       Nothing else is guaranteed, not even a valid stack.               */
    movl    $__boot_stack_top, %esp
    movl    %eax, (.Lmb_magic)
    movl    %ebx, (.Lmb_info)

    /* --- 1. Require CPUID, and CPUID leaf 0x80000001 (long mode bit) --- */
    pushfl
    popl    %eax
    movl    %eax, %ecx
    xorl    $(1 << 21), %eax        /* toggle EFLAGS.ID */
    pushl   %eax
    popfl
    pushfl
    popl    %eax
    pushl   %ecx
    popfl
    xorl    %ecx, %eax
    jz      .Lno_long_mode          /* ID bit stuck => no CPUID */

    movl    $0x80000000, %eax
    cpuid
    cmpl    $0x80000001, %eax
    jb      .Lno_long_mode
    movl    $0x80000001, %eax
    cpuid
    testl   $(1 << 29), %edx        /* EDX.LM */
    jz      .Lno_long_mode

    /* --- 2. Zero the bootstrap page tables ------------------------- */
    movl    $__boot_pt_start, %edi
    movl    $__boot_pt_end, %ecx
    subl    %edi, %ecx
    shrl    $2, %ecx
    xorl    %eax, %eax
    rep stosl

    /* --- 3. Build the bootstrap tables ----------------------------- */
    /* PML4[0]   -> PDPT_low   (identity)                              */
    movl    $__boot_pt_start + 0x1000, %eax
    orl     $0x3, %eax                          /* Present | Write     */
    movl    %eax, (__boot_pt_start + 0)
    /* PML4[256] -> PDPT_pm    (0xFFFF_8000_0000_0000 direct map)      */
    movl    $__boot_pt_start + 0x4000, %eax
    orl     $0x3, %eax
    movl    %eax, (__boot_pt_start + 256 * 8)
    /* PML4[511] -> PDPT_high  (0xFFFF_FF80_0000_0000)                 */
    movl    $__boot_pt_start + 0x3000, %eax
    orl     $0x3, %eax
    movl    %eax, (__boot_pt_start + 511 * 8)

    /* PDPT_low[0]     -> PD_ident */
    movl    $__boot_pt_start + 0x2000, %eax
    orl     $0x3, %eax
    movl    %eax, (__boot_pt_start + 0x1000 + 0)
    /* PDPT_high[510]  -> PD_ident  (0xFFFF_FFFF_8000_0000 -> phys 0)  */
    movl    %eax, (__boot_pt_start + 0x3000 + 510 * 8)

    /* PD_ident[i] = i * 2 MiB, Present|Write|Huge|Global              */
    xorl    %ecx, %ecx
    movl    $__boot_pt_start + 0x2000, %edi
.Lfill_ident:
    movl    %ecx, %eax
    shll    $21, %eax
    orl     $0x183, %eax                /* P | RW | PS | G */
    movl    %eax, (%edi, %ecx, 8)
    movl    $0, 4(%edi, %ecx, 8)
    incl    %ecx
    cmpl    $512, %ecx
    jb      .Lfill_ident

    /* PDPT_pm[k] -> PD_pm[k], k in 0..4  (direct-map the low 4 GiB)   */
    xorl    %ecx, %ecx
    movl    $__boot_pt_start + 0x4000, %edi
.Lfill_pm_pdpt:
    movl    %ecx, %eax
    shll    $12, %eax
    addl    $__boot_pt_start + 0x5000, %eax
    orl     $0x3, %eax
    movl    %eax, (%edi, %ecx, 8)
    movl    $0, 4(%edi, %ecx, 8)
    incl    %ecx
    cmpl    $4, %ecx
    jb      .Lfill_pm_pdpt

    /* PD_pm[0..4][i] = (k * 512 + i) * 2 MiB                          */
    xorl    %ecx, %ecx                  /* ecx = linear 2 MiB index    */
    movl    $__boot_pt_start + 0x5000, %edi
.Lfill_pm:
    movl    %ecx, %eax
    shll    $21, %eax                   /* low 32 bits of phys addr    */
    orl     $0x183, %eax
    movl    %eax, (%edi, %ecx, 8)
    movl    %ecx, %edx
    shrl    $11, %edx                   /* high 32 bits: index >> 11   */
    movl    %edx, 4(%edi, %ecx, 8)
    incl    %ecx
    cmpl    $2048, %ecx                 /* 2048 * 2 MiB == 4 GiB       */
    jb      .Lfill_pm

    /* --- 4. CR4: PAE + PGE + SSE ----------------------------------- */
    movl    %cr4, %eax
    orl     $(1 << 5), %eax             /* PAE  -- mandatory for long mode  */
    orl     $(1 << 7), %eax             /* PGE  -- global pages             */
    orl     $(1 << 9), %eax             /* OSFXSR    -- enable SSE          */
    orl     $(1 << 10), %eax            /* OSXMMEXCPT                       */
    movl    %eax, %cr4

    /* CR0.MP=1, CR0.EM=0: SSE instructions execute rather than trap.
       The kernel is compiled against a `core` that assumes SSE2, so this
       has to happen before the first Rust instruction.                */
    movl    %cr0, %eax
    andl    $~(1 << 2), %eax            /* EM = 0 */
    orl     $(1 << 1), %eax             /* MP = 1 */
    movl    %eax, %cr0

    /* --- 5. CR3 ----------------------------------------------------- */
    movl    $__boot_pt_start, %eax
    movl    %eax, %cr3

    /* --- 6. EFER.LME | EFER.NXE ------------------------------------- */
    movl    $0xC0000080, %ecx
    rdmsr
    orl     $(1 << 8), %eax             /* LME */
    orl     $(1 << 11), %eax            /* NXE -- we rely on NX for W^X   */
    wrmsr

    /* --- 7. Turn on paging => enter IA-32e compatibility mode -------- */
    movl    %cr0, %eax
    orl     $(1 << 31), %eax            /* PG */
    orl     $(1 << 16), %eax            /* WP: honour RO in ring 0        */
    orl     $(1 << 0), %eax             /* PE (already set, be explicit)  */
    movl    %eax, %cr0

    /* --- 8. Load a 64-bit GDT and long-jump into 64-bit mode --------- */
    lgdt    (.Lboot_gdt_ptr)

    /* Hand the Multiboot2 handoff values to the 64-bit code in registers;
       writing 32-bit registers zero-extends, so RDI/RSI are already the
       correct 64-bit values on the other side of the jump.             */
    movl    (.Lmb_magic), %edi
    movl    (.Lmb_info), %esi

    ljmp    $0x08, $_boot_entry64

.Lno_long_mode:
    /* No CPUID or no long mode: scribble "NO64" on the VGA text buffer
       (identity mapped, we are still in protected mode) and stop.      */
    movl    $0x000B8000, %edi
    movl    $0x4F4E4F34, %eax           /* '4' 'O' 'N' -- little endian  */
    movw    $0x4F4E, (%edi)             /* 'N' on red                    */
    movw    $0x4F4F, 2(%edi)            /* 'O'                           */
    movw    $0x4F36, 4(%edi)            /* '6'                           */
    movw    $0x4F34, 6(%edi)            /* '4'                           */
.Lhang32:
    hlt
    jmp     .Lhang32
.size _boot_entry32, . - _boot_entry32

/* ------------------------------------------------------------------ */
/* 64-bit continuation (still executing from the identity mapping)      */
/* ------------------------------------------------------------------ */
.code64
.globl _boot_entry64
.type _boot_entry64, @function
_boot_entry64:
    /* Park the Multiboot2 handoff values where the `rep stosq` below cannot
       reach them: string operations clobber RDI/RCX/RAX.                */
    movq    %rdi, %r12                   /* magic */
    movq    %rsi, %r13                   /* &multiboot_info */

    /* Data segments are ignored in 64-bit mode except for their null-ness;
       load the flat data selector so descriptor caches are sane.       */
    movw    $0x10, %ax
    movw    %ax, %ds
    movw    %ax, %es
    movw    %ax, %ss
    xorw    %ax, %ax
    movw    %ax, %fs
    movw    %ax, %gs

    /* Zero .bss before touching any Rust static. We are still on the
       identity-mapped trampoline stack, and .bss is only reachable
       through the high-half mapping, so use 64-bit absolute addresses. */
    movabsq $__bss_start, %rdi
    movabsq $__bss_end, %rcx
    subq    %rdi, %rcx
    shrq    $3, %rcx
    xorq    %rax, %rax
    rep stosq

    /* Leave the identity mapping for good. */
    movabsq $.Lhigh_half, %rax
    jmpq    *%rax

.Lhigh_half:
    movabsq $__init_stack_top, %rsp
    xorq    %rbp, %rbp
    movq    %r12, %rdi                   /* kmain(magic, ...)            */
    movq    %r13, %rsi                   /* kmain(..., multiboot_info)   */

    /* SysV requires RSP+8 to be 16-byte aligned *at the callee's first
       instruction*, i.e. RSP must be 16-byte aligned immediately before the
       `call`. __init_stack_top is page aligned, so it already is: adjusting it
       here would break every `movaps` the compiler emits for a 16-byte stack
       temporary.                                                          */
    movabsq $kmain, %rax
    callq   *%rax

.Lhang64:
    cli
    hlt
    jmp     .Lhang64
.size _boot_entry64, . - _boot_entry64

/* ------------------------------------------------------------------ */
.section .boot.data, "aw"
.balign 8
.Lmb_magic:
    .long   0
.Lmb_info:
    .long   0

.balign 16
.Lboot_gdt:
    .quad   0x0000000000000000          /* 0x00 null                     */
    .quad   0x00AF9A000000FFFF          /* 0x08 code64: L=1, DPL=0       */
    .quad   0x00CF92000000FFFF          /* 0x10 data                     */
.Lboot_gdt_end:
.Lboot_gdt_ptr:
    .word   .Lboot_gdt_end - .Lboot_gdt - 1
    .long   .Lboot_gdt
    .long   0
"#,
    options(att_syntax)
);
