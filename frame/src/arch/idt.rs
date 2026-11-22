// SPDX-License-Identifier: GPL-2.0-only

//! IDT (Interrupt Descriptor Table).
//!
//! When an exception (divide by zero, page fault…) or an interrupt happens, the CPU looks up
//! its number — the "vector", 0 to 255 — in the IDT, and jumps to the handler address stored
//! there. Vectors 0–31 are reserved for CPU exceptions; device interrupts (stage 1) will use
//! the rest.
//!
//! One IDT can be shared by all CPUs; it is only read by the hardware.

use core::arch::asm;
use core::mem::size_of;

use spin::Once;

use super::gdt::{self, DescriptorTablePointer};
use super::{exceptions, interrupts};

/// One IDT entry ("gate"), 16 bytes, as the CPU expects it. Aligning every entry to its own
/// size also aligns the whole table, which Intel recommends for speed.
#[derive(Clone, Copy)]
#[repr(C, align(16))]
struct Gate {
    offset_low: u16,
    selector: u16,
    /// Bits 0–2: which IST stack to switch to (0 = stay on the current stack).
    ist: u8,
    /// Present bit, privilege level and gate type.
    attributes: u8,
    offset_middle: u16,
    offset_high: u32,
    _reserved: u32,
}

/// Present, ring 0, type 0xE = "64-bit interrupt gate". An interrupt gate (unlike a trap gate)
/// turns interrupts off on entry, so a handler cannot be interrupted before it is ready.
const INTERRUPT_GATE: u8 = 0x8E;

impl Gate {
    const MISSING: Self = Self {
        offset_low: 0,
        selector: 0,
        ist: 0,
        attributes: 0,
        offset_middle: 0,
        offset_high: 0,
        _reserved: 0,
    };

    fn new(handler: u64, ist: u8) -> Self {
        Self {
            offset_low: handler as u16,
            selector: gdt::KERNEL_CODE,
            ist,
            attributes: INTERRUPT_GATE,
            offset_middle: (handler >> 16) as u16,
            offset_high: (handler >> 32) as u32,
            _reserved: 0,
        }
    }
}

type Idt = [Gate; 256];

static IDT: Once<Idt> = Once::new();

/// Vector of the double fault exception.
const DOUBLE_FAULT: usize = 8;

/// Fills in the IDT with the exception handlers and loads it into this CPU.
pub fn init() {
    let idt = IDT.call_once(|| {
        let mut idt = [Gate::MISSING; 256];
        for (vector, entry) in exceptions::ENTRY_POINTS.iter().enumerate() {
            let ist = if vector == DOUBLE_FAULT {
                gdt::DOUBLE_FAULT_IST
            } else {
                0
            };
            idt[vector] = Gate::new(*entry as usize as u64, ist);
        }
        for vector in interrupts::FIRST_VECTOR..=u8::MAX {
            idt[usize::from(vector)] = Gate::new(interrupts::stub_address(vector), 0);
        }
        idt
    });
    load_table(idt);
}

/// Loads the IDT filled in by [`init`] into this CPU (an AP).
pub(crate) fn load() {
    load_table(IDT.get().expect("idt::init has run on the boot CPU"));
}

fn load_table(idt: &'static Idt) {
    let pointer = DescriptorTablePointer {
        limit: (size_of::<Idt>() - 1) as u16,
        base: idt.as_ptr() as u64,
    };
    // SAFETY: the IDT is in a `static Once`, so it lives forever and never changes after
    // `call_once`; every gate points to an entry stub (`exceptions` for 0–31, `interrupts`
    // for 32–255) and uses the kernel code selector loaded by `gdt::init`.
    unsafe {
        asm!("lidt [{0}]", in(reg) &pointer, options(readonly, nostack, preserves_flags));
    }
}
