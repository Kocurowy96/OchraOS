// SPDX-License-Identifier: GPL-2.0-only

//! Kernel time: a counter of timer interrupts ("ticks").

use core::sync::atomic::{AtomicU64, Ordering};

/// Timer interrupts per second. Each tick will also be a chance for the scheduler to switch
/// tasks, so this is the length of a time slice: 10 ms.
pub const HZ: u32 = 100;

static TICKS: AtomicU64 = AtomicU64::new(0);

/// Timer interrupts since the timer was started.
pub fn ticks() -> u64 {
    TICKS.load(Ordering::Relaxed)
}

/// The timer interrupt handler.
pub fn on_tick(_vector: u8) {
    TICKS.fetch_add(1, Ordering::Relaxed);
}
