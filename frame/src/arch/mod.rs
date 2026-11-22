// SPDX-License-Identifier: GPL-2.0-only

//! x86-64 specific code: CPU tables, exceptions, port I/O and other inline assembly.

pub mod apic;
pub mod cpu;
pub mod exceptions;
pub(crate) mod gdt;
pub(crate) mod idt;
pub mod interrupts;
pub mod pic;
mod pit;
pub(crate) mod port;

use alloc::boxed::Box;
use core::arch::asm;

use crate::mm::paging::KernelStack;

/// Sets up the CPU tables the kernel needs before anything else: the GDT (with the TSS) and
/// the IDT with the exception handlers. Then switches on the CPU protection features.
pub fn init() -> cpu::Protections {
    gdt::init();
    idt::init();
    cpu::enable_protections()
}

/// The same for an AP (see `smp`): its own GDT and TSS, with a double fault stack that has
/// a guard page, the shared IDT, and the protections. Needs paging and the heap.
pub(crate) fn init_ap() -> cpu::Protections {
    let double_fault_stack = Box::leak(Box::new(KernelStack::new(4)));
    gdt::init_ap(double_fault_stack.top());
    idt::load();
    cpu::enable_protections()
}

/// Stops this CPU for good.
pub fn halt_forever() -> ! {
    loop {
        // SAFETY: `cli` and `hlt` touch no memory and no Rust-visible state. With interrupts
        // off, `hlt` sleeps until a non-maskable interrupt; the loop puts us back to sleep.
        unsafe { asm!("cli", "hlt", options(nomem, nostack)) };
    }
}
