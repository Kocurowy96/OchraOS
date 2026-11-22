// SPDX-License-Identifier: GPL-2.0-only

//! The local APIC (Advanced Programmable Interrupt Controller).
//!
//! Every CPU core has its own local APIC. It receives interrupts (from devices through the
//! I/O APIC, from other cores as IPIs, from its own timer) and hands them to its core. After
//! handling one, the kernel must send an "end of interrupt" (EOI) so the APIC delivers the
//! next one of the same or lower priority.
//!
//! There are two ways to talk to it:
//! - **xAPIC**: its registers are memory-mapped at a physical address (normally 0xFEE00000);
//! - **x2APIC**: the same registers as MSRs (`rdmsr`/`wrmsr`), faster and needed for more than
//!   255 cores. Register at offset `o` in xAPIC is MSR `0x800 + o / 16` in x2APIC.
//!
//! We use x2APIC when CPUID reports it and fall back to xAPIC otherwise.

use core::arch::x86_64::__cpuid;
use core::fmt;

use spin::Once;

use super::cpu::{read_msr, write_msr};
use super::pit;
use crate::mm::PhysAddr;
use crate::mm::paging;

/// Vector for spurious interrupts (no handler, no EOI). The low 4 bits must be all ones on
/// older CPUs, so 0xFF is the traditional choice.
pub const SPURIOUS_VECTOR: u8 = 0xFF;

/// MSR holding the APIC base address and its enable bits.
const IA32_APIC_BASE: u32 = 0x1B;
const BASE_ENABLE: u64 = 1 << 11;
const BASE_X2APIC: u64 = 1 << 10;

// Register offsets (xAPIC layout).
const ID: u32 = 0x20;
const TASK_PRIORITY: u32 = 0x80;
const EOI: u32 = 0xB0;
const SPURIOUS: u32 = 0xF0;
const INTERRUPT_COMMAND: u32 = 0x300;
const LVT_TIMER: u32 = 0x320;
const TIMER_INITIAL_COUNT: u32 = 0x380;
const TIMER_CURRENT_COUNT: u32 = 0x390;
const TIMER_DIVIDE: u32 = 0x3E0;
/// x2APIC only: send an interrupt to this core.
const X2APIC_SELF_IPI_MSR: u32 = 0x83F;

const SPURIOUS_ENABLE: u32 = 1 << 8;
/// In an LVT register: the interrupt is masked.
const LVT_MASKED: u32 = 1 << 16;
/// In the timer LVT register: restart automatically ("periodic mode").
const TIMER_PERIODIC: u32 = 1 << 17;
/// Divide configuration value for "divide by 16".
const DIVIDE_BY_16: u32 = 0b0011;

/// Vector of the local APIC timer interrupt.
pub const TIMER_VECTOR: u8 = 0x30;

/// How this core's local APIC is reached.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    X2Apic,
    XApic,
}

impl fmt::Display for Kind {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.write_str(match self {
            Self::X2Apic => "x2APIC",
            Self::XApic => "xAPIC",
        })
    }
}

struct LocalApic {
    kind: Kind,
    /// Virtual address of the register page (xAPIC only).
    registers: u64,
}

static APIC: Once<LocalApic> = Once::new();

/// What [`init`] found.
#[derive(Clone, Copy, Debug)]
pub struct Info {
    pub kind: Kind,
    pub id: u32,
}

/// Enables this core's local APIC. Call once, after paging is set up (xAPIC needs a mapping).
pub fn init() -> Info {
    let features = __cpuid(1);
    assert!(features.edx & (1 << 9) != 0, "this CPU has no local APIC");
    let has_x2apic = features.ecx & (1 << 21) != 0;

    let base = read_msr(IA32_APIC_BASE);
    let apic = APIC.call_once(|| {
        if has_x2apic {
            // SAFETY: CPUID reports x2APIC support; switching the enabled APIC to x2APIC mode
            // only changes how its registers are reached (MSRs instead of memory).
            unsafe { write_msr(IA32_APIC_BASE, base | BASE_ENABLE | BASE_X2APIC) };
            LocalApic {
                kind: Kind::X2Apic,
                registers: 0,
            }
        } else {
            // SAFETY: setting the global enable bit keeps the default base address.
            unsafe { write_msr(IA32_APIC_BASE, base | BASE_ENABLE) };
            let physical = PhysAddr(base & 0x000F_FFFF_FFFF_F000);
            LocalApic {
                kind: Kind::XApic,
                registers: paging::map_device(physical, 1),
            }
        }
    });

    // Accept interrupts of every priority, and enable the APIC with our spurious vector.
    apic.write(TASK_PRIORITY, 0);
    apic.write(SPURIOUS, SPURIOUS_ENABLE | u32::from(SPURIOUS_VECTOR));
    // The timer stays masked until the kernel starts it.
    apic.write(LVT_TIMER, LVT_MASKED);

    let id = match apic.kind {
        Kind::X2Apic => apic.read(ID),
        Kind::XApic => apic.read(ID) >> 24,
    };
    Info {
        kind: apic.kind,
        id,
    }
}

/// Tells the local APIC that the current interrupt has been handled.
pub fn end_of_interrupt() {
    if let Some(apic) = APIC.get() {
        apic.write(EOI, 0);
    }
}

/// Sends interrupt `vector` to this core (an inter-processor interrupt to itself).
pub fn send_self_ipi(vector: u8) {
    let apic = local();
    match apic.kind {
        // SAFETY: the self-IPI MSR exists in x2APIC mode and only raises `vector`.
        Kind::X2Apic => unsafe { write_msr(X2APIC_SELF_IPI_MSR, u64::from(vector)) },
        Kind::XApic => {
            // Destination shorthand 01 = "self", fixed delivery.
            apic.write(INTERRUPT_COMMAND, (1 << 18) | u32::from(vector));
            // Wait until the APIC has sent it (delivery status bit 12 clears).
            while apic.read(INTERRUPT_COMMAND) & (1 << 12) != 0 {
                core::hint::spin_loop();
            }
        }
    }
}

/// Result of [`start_timer`].
#[derive(Clone, Copy, Debug)]
pub struct TimerInfo {
    /// Timer counts per millisecond (after dividing by 16), as measured with the PIT.
    pub counts_per_ms: u32,
    pub hz: u32,
}

/// Calibrates the local APIC timer against the PIT, then starts it in periodic mode: it
/// raises [`TIMER_VECTOR`] `hz` times per second. Register a handler for that vector first.
pub fn start_timer(hz: u32) -> TimerInfo {
    let apic = local();
    apic.write(TIMER_DIVIDE, DIVIDE_BY_16);
    let counted = pit::measure(
        10,
        || apic.write(TIMER_INITIAL_COUNT, u32::MAX),
        || u32::MAX - apic.read(TIMER_CURRENT_COUNT),
    );
    apic.write(TIMER_INITIAL_COUNT, 0);
    let counts_per_ms = (counted / 10).max(1);

    apic.write(LVT_TIMER, TIMER_PERIODIC | u32::from(TIMER_VECTOR));
    apic.write(TIMER_INITIAL_COUNT, (counts_per_ms * 1000 / hz).max(1));
    TimerInfo { counts_per_ms, hz }
}

fn local() -> &'static LocalApic {
    APIC.get().expect("apic::init has run")
}

impl LocalApic {
    fn read(&self, register: u32) -> u32 {
        match self.kind {
            Kind::X2Apic => read_msr(0x800 + register / 16) as u32,
            Kind::XApic => {
                let pointer = (self.registers + u64::from(register)) as *const u32;
                // SAFETY: `registers` maps the APIC's register page uncached, and every
                // register used here is a 32-bit register inside that page.
                unsafe { pointer.read_volatile() }
            }
        }
    }

    fn write(&self, register: u32, value: u32) {
        match self.kind {
            // SAFETY: the MSRs 0x800 + register / 16 are the x2APIC registers; writing them
            // affects only interrupt delivery to this core, which is what the caller asks.
            Kind::X2Apic => unsafe { write_msr(0x800 + register / 16, u64::from(value)) },
            Kind::XApic => {
                let pointer = (self.registers + u64::from(register)) as *mut u32;
                // SAFETY: as in `read`.
                unsafe { pointer.write_volatile(value) }
            }
        }
    }
}
