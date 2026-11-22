// SPDX-License-Identifier: GPL-2.0-only

//! Per-CPU data: one [`PerCpu`] structure for every CPU, found through the GS register.
//!
//! Each CPU has its own GS base (the `IA32_GS_BASE` MSR). We point it at that CPU's
//! `PerCpu`, whose first field holds the structure's own address; so `mov rax, gs:[0]` gives
//! every CPU a pointer to its own data in one instruction.
//!
//! The data must be read with interrupts off: otherwise the task could be preempted between
//! reading the pointer and using it, and continue on another CPU with that CPU's data.
//!
//! When user space arrives (stage 2), `swapgs` will exchange the kernel's GS base with the
//! user's on every kernel entry and exit.

use alloc::boxed::Box;
use alloc::sync::Arc;
use core::arch::asm;

use crate::arch::cpu::{read_msr, write_msr};
use crate::arch::interrupts;
use crate::sync::IrqMutex;
use crate::task::Task;

const IA32_GS_BASE: u32 = 0xC000_0101;

#[repr(C)]
pub(crate) struct PerCpu {
    /// The address of this structure; must stay the first field (read as `gs:[0]`).
    this: *const PerCpu,
    index: u32,
    apic_id: u32,
    /// The task running on this CPU (`None` before the scheduler starts here).
    pub(crate) current_task: IrqMutex<Option<Arc<Task>>>,
}

/// Creates this CPU's `PerCpu` and points GS at it. Call once per CPU, after the GDT is
/// loaded (loading segment registers may clear the GS base) and the heap works.
pub(crate) fn init(index: u32, apic_id: u32) {
    let data = Box::leak(Box::new(PerCpu {
        this: core::ptr::null(),
        index,
        apic_id,
        current_task: IrqMutex::new(None),
    }));
    data.this = &raw const *data;
    // SAFETY: `data` is leaked, so it lives for ever, and it belongs to this CPU alone (each
    // CPU calls `init` once for itself); its first field holds its own address, as `this()`
    // expects. The GS base affects only `gs:` memory accesses, which only this module makes.
    unsafe { write_msr(IA32_GS_BASE, data.this as u64) };
}

/// This CPU's data, or `None` before [`init`] ran on it. Interrupts must be off (see the
/// module documentation), or the answer may belong to the CPU the task just left.
pub(crate) fn this() -> Option<&'static PerCpu> {
    // Before `init` the GS base is 0 and `gs:[0]` would read address 0.
    if read_msr(IA32_GS_BASE) == 0 {
        return None;
    }
    let pointer: *const PerCpu;
    // SAFETY: the GS base points to a `PerCpu` set up by `init`, whose first field is its
    // own address.
    unsafe {
        asm!("mov {}, gs:[0]", out(reg) pointer, options(nostack, readonly, preserves_flags))
    };
    // SAFETY: `init` leaked the structure, so it lives for ever; it is only accessed through
    // shared references (mutable parts are behind locks).
    Some(unsafe { &*pointer })
}

/// Logical index of the CPU this code runs on (the boot CPU is 0; also 0 before per-CPU data
/// is set up). A task may move to another CPU right after asking, so treat it as a hint.
pub fn cpu_index() -> u32 {
    let were_enabled = interrupts::disable();
    let index = this().map_or(0, |cpu| cpu.index);
    if were_enabled {
        interrupts::enable();
    }
    index
}

/// Local APIC ID of the CPU this code runs on (0 before per-CPU data is set up).
pub fn apic_id() -> u32 {
    let were_enabled = interrupts::disable();
    let id = this().map_or(0, |cpu| cpu.apic_id);
    if were_enabled {
        interrupts::enable();
    }
    id
}
