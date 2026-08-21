/* NodeOS userspace image layout.
 *
 * Linked at the base of the program window (see `mm::layout`). The image is
 * position dependent: NXF supports relocations, but a research OS gains more
 * from images whose addresses are predictable in a debugger than from ASLR it
 * is not yet ready to use meaningfully.
 *
 * Sections are grouped so that `nxfpack` can emit one NXF segment per
 * permission class, which is what makes W^X enforceable by the page tables.
 */

OUTPUT_FORMAT(elf64-x86-64)
OUTPUT_ARCH(i386:x86-64)
ENTRY(_start)

PROGRAM_BASE = 0x0000000000400000;

PHDRS
{
    text   PT_LOAD FLAGS(5);   /* R-X */
    rodata PT_LOAD FLAGS(4);   /* R-- */
    data   PT_LOAD FLAGS(6);   /* RW- */
}

SECTIONS
{
    . = PROGRAM_BASE;

    .text ALIGN(4096) : {
        __text_start = .;
        KEEP(*(.text.start))
        *(.text .text.*)
        . = ALIGN(4096);
        __text_end = .;
    } :text

    .rodata ALIGN(4096) : {
        __rodata_start = .;
        *(.rodata .rodata.*)
        *(.gcc_except_table .gcc_except_table.*)
        . = ALIGN(4096);
        __rodata_end = .;
    } :rodata

    .data ALIGN(4096) : {
        __data_start = .;
        *(.data .data.*)
        *(.data.rel.ro .data.rel.ro.*)
        *(.got .got.plt)
        . = ALIGN(4096);
        __data_end = .;
    } :data

    .bss ALIGN(4096) (NOLOAD) : {
        __bss_start = .;
        *(.bss .bss.*)
        *(COMMON)
        . = ALIGN(4096);
        __bss_end = .;
    } :data

    __image_end = .;

    /DISCARD/ : {
        *(.eh_frame) *(.eh_frame_hdr)
        *(.note .note.*)
        *(.comment)
        *(.dynamic) *(.dynsym) *(.dynstr) *(.hash) *(.gnu.hash) *(.interp)
    }
}
