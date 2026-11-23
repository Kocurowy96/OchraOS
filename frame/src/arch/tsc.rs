// SPDX-License-Identifier: GPL-2.0-only

//! The TSC (time stamp counter): a 64-bit counter every x86-64 CPU increments, read with one
//! instruction (`rdtsc`), so it is the cheapest precise clock there is.
//!
//! It only measures time if it counts at a constant rate. Old CPUs slowed it down with the
//! core clock (power saving) or stopped it in deep sleep; CPUs with an "invariant TSC" (CPUID
//! leaf 0x8000_0007, EDX bit 8) count at the same rate always, and in sync on every core. The
//! kernel uses it for `CLOCK_MONOTONIC` only then (docs/decisions.md).
//!
//! Its rate is rarely reported, so it is measured once against the PIT, as the APIC timer is.

use core::arch::x86_64::{__cpuid, _rdtsc};
use core::cell::Cell;

use spin::Once;

use super::pit;

/// What [`init`] found.
#[derive(Clone, Copy, Debug)]
pub struct Info {
    /// Counts per second.
    pub hz: u64,
    /// Whether the rate is constant (and the counters of all cores agree).
    pub invariant: bool,
}

static INFO: Once<Info> = Once::new();

/// Measures the TSC rate (50 ms against the PIT) and checks whether it is invariant. Call once,
/// on the boot CPU, before other CPUs start (the PIT is not shared safely).
pub fn init() -> Info {
    *INFO.call_once(|| {
        let start = Cell::new(0);
        let counted = pit::measure(50, || start.set(read()), || read() - start.get());
        Info {
            hz: counted * 20,
            invariant: invariant(),
        }
    })
}

/// The result of [`init`], once it has run.
pub fn info() -> Option<Info> {
    INFO.get().copied()
}

/// The counter now.
pub fn read() -> u64 {
    // SAFETY: RDTSC exists on every x86-64 CPU and only reads the counter (CR4.TSD is clear,
    // so it works in any ring; the kernel does not restrict it).
    unsafe { _rdtsc() }
}

/// CPUID: "invariant TSC" (leaf 0x8000_0007, EDX bit 8), if the CPU has that leaf at all.
fn invariant() -> bool {
    const LEAF: u32 = 0x8000_0007;
    let highest_extended = __cpuid(0x8000_0000).eax;
    highest_extended >= LEAF && __cpuid(LEAF).edx & (1 << 8) != 0
}
