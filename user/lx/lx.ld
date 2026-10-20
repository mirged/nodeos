/* NodeOS layout for C programs linked against musl and liblx.
 *
 * The same program window and the same three permission-class segments as
 * `user/link/user.ld`, so `nxfpack` and the loader treat a musl program
 * exactly like any other NodeOS program. Three additions, all for the C
 * runtime:
 *
 *  - The ELF header and program headers are mapped, at the start of the text
 *    segment. musl finds its TLS template by walking the program headers
 *    from AT_PHDR, and liblx points AT_PHDR at `__ehdr_start + e_phoff`.
 *  - `.tdata`/`.tbss` and a PT_TLS header describing them. The loader ignores
 *    PT_TLS (only PT_LOAD is packed); musl reads it to build each thread's
 *    TLS block.
 *  - `.init_array`/`.fini_array`, which musl's `__libc_start_init` and
 *    `exit` walk, with the bounds symbols it looks for.
 */

OUTPUT_FORMAT(elf64-x86-64)
OUTPUT_ARCH(i386:x86-64)
ENTRY(_start)

PROGRAM_BASE = 0x0000000000400000;

PHDRS
{
    text   PT_LOAD FILEHDR PHDRS FLAGS(5);   /* R-X, headers first */
    rodata PT_LOAD FLAGS(4);                 /* R-- */
    data   PT_LOAD FLAGS(6);                 /* RW- */
    tls    PT_TLS FLAGS(4);
}

SECTIONS
{
    . = PROGRAM_BASE + SIZEOF_HEADERS;

    .text : {
        __text_start = .;
        KEEP(*(.text.start))
        *(.text .text.*)
        *(.init) *(.fini)
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
    } :data

    .init_array : {
        PROVIDE_HIDDEN(__init_array_start = .);
        KEEP(*(SORT_BY_INIT_PRIORITY(.init_array.*)))
        KEEP(*(.init_array .ctors))
        PROVIDE_HIDDEN(__init_array_end = .);
    } :data

    .fini_array : {
        PROVIDE_HIDDEN(__fini_array_start = .);
        KEEP(*(SORT_BY_INIT_PRIORITY(.fini_array.*)))
        KEEP(*(.fini_array .dtors))
        PROVIDE_HIDDEN(__fini_array_end = .);
    } :data

    .tdata : {
        *(.tdata .tdata.*)
    } :data :tls

    .tbss : {
        *(.tbss .tbss.*)
    } :data :tls

    .data.end : {
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
