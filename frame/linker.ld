/* SPDX-License-Identifier: GPL-2.0-only */

/*
 * Linker script for the OchraOS kernel.
 *
 * Places the kernel in the top 2 GiB of the virtual address space ("higher half"), which is
 * what the Limine boot protocol expects, and splits it into segments with separate
 * permissions so that no page is both writable and executable (W^X).
 *
 * The kernel is a position-independent executable (PIE): the addresses below are only the
 * link-time ones. Limine moves the whole image by a random offset (KASLR) and fixes up every
 * absolute address using the relocation table (.rela.dyn), found through .dynamic.
 */

OUTPUT_FORMAT(elf64-x86-64)
ENTRY(kmain)

/* Program headers = segments. FLAGS are ELF permission bits: 4 = read, 2 = write, 1 = execute. */
PHDRS
{
    text   PT_LOAD FLAGS(5); /* r-x */
    rodata PT_LOAD FLAGS(4); /* r-- */
    data   PT_LOAD FLAGS(6); /* rw- */
    /* Points Limine to .dynamic, which lists the relocations. */
    dynamic PT_DYNAMIC FLAGS(6);
}

SECTIONS
{
    /* -2 GiB: lets the compiler use the compact "kernel" code model (32-bit signed addresses). */
    . = 0xffffffff80000000;

    /* The __kernel_* symbols mark the segment boundaries for the page table code
     * (frame/src/mm/paging.rs), which maps each segment with its own permissions. */
    __kernel_text_start = .;
    .text : {
        *(.text .text.*)
    } :text
    __kernel_text_end = .;

    /* Each segment starts on a new page, so a page never mixes two kinds of permission. */
    . = ALIGN(CONSTANT(MAXPAGESIZE));

    __kernel_rodata_start = .;
    .rodata : {
        *(.rodata .rodata.*)
    } :rodata

    /* Exception fixups for user memory access (frame/src/user_mode.rs): pairs of relative
     * offsets "instruction that may fault, where to continue". */
    .ochra_fixup : ALIGN(4) {
        __ochra_fixup_start = .;
        KEEP(*(.ochra_fixup))
        __ochra_fixup_end = .;
    } :rodata

    /* Symbol and relocation tables used by Limine to relocate the kernel. */
    .dynsym   : { *(.dynsym) } :rodata
    .gnu.hash : { *(.gnu.hash) } :rodata
    .hash     : { *(.hash) } :rodata
    .dynstr   : { *(.dynstr) } :rodata
    .rela.dyn : { *(.rela.dyn) } :rodata
    __kernel_rodata_end = .;

    . = ALIGN(CONSTANT(MAXPAGESIZE));

    __kernel_data_start = .;
    .data : {
        /* Limine scans the area between the start and end markers for requests and writes
         * its responses back into them, so they must be writable. KEEP stops the linker from
         * throwing them away: nothing in the kernel refers to the markers. */
        KEEP(*(.requests_start_marker))
        KEEP(*(.requests))
        KEEP(*(.requests_end_marker))

        *(.data .data.*)
    } :data

    /* The global offset table holds absolute addresses, which relocation rewrites. */
    .got : {
        *(.got .got.*)
    } :data

    .dynamic : {
        *(.dynamic)
    } :data :dynamic

    .bss : {
        *(.bss .bss.*)
        *(COMMON)
    } :data
    __kernel_data_end = .;

    /* Unwinding tables and notes are useless without unwinding; drop them. */
    /DISCARD/ : {
        *(.eh_frame*)
        *(.note .note.*)
    }
}
