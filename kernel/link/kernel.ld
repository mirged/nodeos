/* NodeOS kernel image layout.
 *
 * The image is loaded by a Multiboot2 boot loader at KERNEL_LMA in physical
 * memory while running in 32-bit protected mode. A small trampoline lives in
 * `.boot`, which is linked *identity* (VMA == LMA) so it is addressable before
 * paging exists. Everything else is linked into the top 2 GiB of the canonical
 * 64-bit address space, at a fixed +KERNEL_VMA offset from its physical
 * address, which lets the trampoline install a single 1 GiB huge-page mapping
 * to bring the whole kernel online.
 */

OUTPUT_FORMAT(elf64-x86-64)
OUTPUT_ARCH(i386:x86-64)
ENTRY(_boot_entry32)

KERNEL_VMA = 0xFFFFFFFF80000000;
KERNEL_LMA = 0x0000000000200000;   /* 2 MiB: above the legacy hole, below 16 MiB */

PHDRS
{
    boot   PT_LOAD FLAGS(7);   /* RWX -- 32-bit trampoline + bootstrap tables */
    text   PT_LOAD FLAGS(5);   /* R-X */
    rodata PT_LOAD FLAGS(4);   /* R-- */
    data   PT_LOAD FLAGS(6);   /* RW- */
}

SECTIONS
{
    . = KERNEL_LMA;
    __image_phys_start = .;

    .boot ALIGN(4096) : {
        KEEP(*(.multiboot_header))
        KEEP(*(.boot.text))
        KEEP(*(.boot.data))

        . = ALIGN(4096);
        __boot_pt_start = .;
        . += 0x9000;            /* 9 bootstrap page-table pages, see boot.rs */
        __boot_pt_end = .;

        . = ALIGN(4096);
        __boot_stack_bottom = .;
        . += 0x4000;            /* 16 KiB trampoline stack (identity mapped) */
        __boot_stack_top = .;
    } :boot

    . = ALIGN(4096);
    __boot_end = .;

    /* Switch to high-half virtual addresses; LMA stays contiguous via AT(). */
    . = __boot_end + KERNEL_VMA;

    .text ALIGN(4096) : AT(ADDR(.text) - KERNEL_VMA) {
        __text_start = .;
        *(.text.hot .text.hot.*)
        *(.text .text.*)
        . = ALIGN(4096);
        __text_end = .;
    } :text

    .rodata ALIGN(4096) : AT(ADDR(.rodata) - KERNEL_VMA) {
        __rodata_start = .;
        *(.rodata .rodata.*)
        *(.gcc_except_table .gcc_except_table.*)
        . = ALIGN(8);
        /* Linker-set of kernel self-tests (see kernel::selftest). */
        __selftest_start = .;
        KEEP(*(.nodeos.selftest))
        __selftest_end = .;
        . = ALIGN(4096);
        __rodata_end = .;
    } :rodata

    .data ALIGN(4096) : AT(ADDR(.data) - KERNEL_VMA) {
        __data_start = .;
        *(.data .data.*)
        *(.data.rel.ro .data.rel.ro.*)
        *(.got .got.plt)
        . = ALIGN(4096);
        __data_end = .;
    } :data

    .bss ALIGN(4096) (NOLOAD) : AT(ADDR(.bss) - KERNEL_VMA) {
        __bss_start = .;
        *(.bss .bss.*)
        *(COMMON)

        . = ALIGN(4096);
        __init_stack_bottom = .;
        . += 0x10000;           /* 64 KiB bootstrap/idle stack for CPU 0 */
        __init_stack_top = .;

        . = ALIGN(4096);
        __bss_end = .;
    } :data

    . = ALIGN(4096);
    __image_end = .;
    __image_phys_end = . - KERNEL_VMA;

    /DISCARD/ : {
        *(.eh_frame) *(.eh_frame_hdr)
        *(.note .note.*)
        *(.comment)
        *(.dynamic) *(.dynsym) *(.dynstr) *(.hash) *(.gnu.hash)
        *(.interp)
    }
}
