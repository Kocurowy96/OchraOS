// SPDX-License-Identifier: GPL-2.0-only

//! Port I/O: the `in` and `out` instructions.
//!
//! Besides memory, x86 has a separate 16-bit "I/O port" address space (0x0000–0xFFFF) that
//! old-style devices such as the serial port, the PIC or the PS/2 controller listen on.
//! Reading or writing a port talks to the device directly; it is not memory.

use core::arch::asm;

/// Writes one byte to an I/O port.
///
/// # Safety
///
/// Writing to a port can make the device behind it do anything (reset the machine, start a
/// DMA transfer over arbitrary memory…). The caller must own the device at `port` and know
/// what the write does.
pub unsafe fn outb(port: u16, value: u8) {
    // SAFETY: the instruction itself touches no memory and no stack; what the device does
    // with the value is the caller's responsibility (see `# Safety`).
    unsafe {
        asm!("out dx, al", in("dx") port, in("al") value, options(nomem, nostack, preserves_flags));
    }
}

/// Reads one byte from an I/O port.
///
/// # Safety
///
/// Reading a port can have side effects too (e.g. it acknowledges data in many devices).
/// The caller must own the device at `port`.
pub unsafe fn inb(port: u16) -> u8 {
    let value: u8;
    // SAFETY: as in `outb`; the device-level effects are the caller's responsibility.
    unsafe {
        asm!("in al, dx", in("dx") port, out("al") value, options(nomem, nostack, preserves_flags));
    }
    value
}
