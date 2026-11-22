// SPDX-License-Identifier: GPL-2.0-only

//! The legacy PIC (8259 Programmable Interrupt Controller), switched off.
//!
//! The 8259 delivered device interrupts on the original IBM PC; modern systems use the APIC
//! instead, but the PIC is still there and starts up mapping its 16 lines onto vectors 8–15,
//! where they would look like CPU exceptions (8 = double fault!). So we move its vectors to
//! 32–47, where a stray ("spurious") one cannot be mistaken for an exception, and mask all
//! its lines.

use super::port::outb;

const MASTER_COMMAND: u16 = 0x20;
const MASTER_DATA: u16 = 0x21;
const SLAVE_COMMAND: u16 = 0xA0;
const SLAVE_DATA: u16 = 0xA1;

/// First vector of the master PIC after remapping; the slave follows at +8.
pub const VECTOR_BASE: u8 = 32;

/// Remaps the PIC to vectors 32–47 and masks every line.
pub fn disable() {
    // SAFETY: ports 0x20/0x21/0xA0/0xA1 belong to the PIC, which nothing else drives. The
    // sequence is the standard initialisation (ICW1–ICW4) followed by masking all lines; port
    // 0x80 (POST diagnostics) is written only to give the old chip time between commands.
    unsafe {
        let pause = || outb(0x80, 0);
        outb(MASTER_COMMAND, 0x11); // ICW1: initialise, expect ICW4
        pause();
        outb(SLAVE_COMMAND, 0x11);
        pause();
        outb(MASTER_DATA, VECTOR_BASE); // ICW2: vector base
        pause();
        outb(SLAVE_DATA, VECTOR_BASE + 8);
        pause();
        outb(MASTER_DATA, 4); // ICW3: slave on line 2
        pause();
        outb(SLAVE_DATA, 2); // ICW3: slave identity
        pause();
        outb(MASTER_DATA, 1); // ICW4: 8086 mode
        pause();
        outb(SLAVE_DATA, 1);
        pause();
        outb(MASTER_DATA, 0xFF); // mask all lines
        outb(SLAVE_DATA, 0xFF);
    }
}
