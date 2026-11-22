// SPDX-License-Identifier: GPL-2.0-only

//! Kernel time: counters of timer interrupts ("ticks"), one per CPU.

use alloc::vec::Vec;

use ochra_frame::cpu_local::cpu_index;
use ochra_frame::sync::IrqMutex;

/// Timer interrupts per second. The scheduler switches tasks on every tick, so this is the
/// length of a time slice: 10 ms.
pub const HZ: u32 = 100;

/// Ticks per CPU, indexed by the CPU's logical index.
static TICKS: IrqMutex<Vec<u64>> = IrqMutex::new(Vec::new());

/// Makes room for `cpus` counters. Call once, before the timer starts.
pub fn init(cpus: u32) {
    TICKS.lock().resize(cpus as usize, 0);
}

/// Timer interrupts on the boot CPU since its timer was started.
pub fn ticks() -> u64 {
    ticks_on(0)
}

/// Timer interrupts on CPU `index` since its timer was started.
pub fn ticks_on(index: u32) -> u64 {
    TICKS.lock().get(index as usize).copied().unwrap_or(0)
}

/// The timer interrupt handler.
pub fn on_tick(_vector: u8) {
    if let Some(count) = TICKS.lock().get_mut(cpu_index() as usize) {
        *count += 1;
    }
    // The time slice of the running task is over.
    crate::sched::schedule();
}
