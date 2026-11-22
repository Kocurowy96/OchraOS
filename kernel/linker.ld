/* SPDX-License-Identifier: GPL-2.0-only */

/*
 * Linker script for the OchraOS kernel.
 *
 * Places the kernel in the top 2 GiB of the virtual address space ("higher half"), which is
 * what the Limine boot protocol expects, and splits it into segments with separate
 * permissions so that no page is both writable and executable (W^X).
 */

OUTPUT_FORMAT(elf64-x86-64)
ENTRY(kmain)

/* Program headers = segments. FLAGS are ELF permission bits: 4 = read, 2 = write, 1 = execute. */
PHDRS
{
    text   PT_LOAD FLAGS(5); /* r-x */
    rodata PT_LOAD FLAGS(4); /* r-- */
    data   PT_LOAD FLAGS(6); /* rw- */
}

SECTIONS
{
    /* -2 GiB: lets the compiler use the compact "kernel" code model (32-bit signed addresses). */
    . = 0xffffffff80000000;

    .text : {
        *(.text .text.*)
    } :text

    /* Each segment starts on a new page, so a page never mixes two kinds of permission. */
    . = ALIGN(CONSTANT(MAXPAGESIZE));

    .rodata : {
        *(.rodata .rodata.*)
    } :rodata

    . = ALIGN(CONSTANT(MAXPAGESIZE));

    .data : {
        /* Limine scans the area between the start and end markers for requests and writes
         * its responses back into them, so they must be writable. KEEP stops the linker from
         * throwing them away: nothing in the kernel refers to the markers. */
        KEEP(*(.requests_start_marker))
        KEEP(*(.requests))
        KEEP(*(.requests_end_marker))

        *(.data .data.*)
    } :data

    .bss : {
        *(.bss .bss.*)
        *(COMMON)
    } :data

    /* Unwinding tables and notes are useless without unwinding; drop them. */
    /DISCARD/ : {
        *(.eh_frame*)
        *(.note .note.*)
    }
}
