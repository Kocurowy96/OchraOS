// SPDX-License-Identifier: GPL-2.0-only

//! Kernel time: counters of timer interrupts ("ticks"), one per CPU, and the clocks.
//!
//! - The monotonic clock (`CLOCK_MONOTONIC`) counts nanoseconds since boot: from the TSC when
//!   its rate is constant ("invariant", checked through CPUID), otherwise from the boot CPU's
//!   ticks (10 ms steps).
//! - The wall clock (`CLOCK_REALTIME`) is the RTC's date at boot plus the monotonic clock.

use alloc::vec::Vec;
use core::sync::atomic::{AtomicU64, Ordering};

use log::info;
use ochra_frame::arch::{rtc, tsc};
use ochra_frame::cpu_local::cpu_index;
use ochra_frame::sync::IrqMutex;

use crate::sched::WaitQueue;

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
    // Keyboard input on the serial console, polled by one CPU (decisions.md); sleepers
    // check their deadline.
    if cpu_index() == 0 {
        crate::console::poll();
        SLEEPERS.wake_all();
    }
    // The time slice of the running task is over.
    crate::sched::schedule();
}

pub const NANOSECONDS_PER_SECOND: u64 = 1_000_000_000;

/// The TSC value at boot (when the clocks start), or 0 when the TSC is not used.
static TSC_AT_BOOT: AtomicU64 = AtomicU64::new(0);
/// The wall clock time at monotonic time 0, in nanoseconds since 1970 (UTC).
static WALL_AT_BOOT: AtomicU64 = AtomicU64::new(0);
/// Woken on every tick of the boot CPU: tasks that sleep check their deadline.
static SLEEPERS: WaitQueue = WaitQueue::new();

/// Starts the clocks: measures the TSC, reads the RTC. Call once on the boot CPU, after its
/// timer runs and before the other CPUs start.
pub fn init_clocks() {
    let tsc = tsc::init();
    if tsc.invariant {
        TSC_AT_BOOT.store(tsc::read(), Ordering::Relaxed);
    }
    let date = rtc::read();
    let seconds = unix_seconds(&date);
    let wall = seconds * NANOSECONDS_PER_SECOND;
    WALL_AT_BOOT.store(wall.saturating_sub(monotonic()), Ordering::Relaxed);
    let source = if tsc.invariant {
        "invariant TSC"
    } else {
        "timer ticks (the TSC rate is not constant)"
    };
    info!(
        "clock: {:04}-{:02}-{:02} {:02}:{:02}:{:02} UTC from the RTC; monotonic clock from {source}, TSC at {} MHz",
        date.year,
        date.month,
        date.day,
        date.hour,
        date.minute,
        date.second,
        tsc.hz / 1_000_000
    );
}

/// Nanoseconds since boot; never goes back.
pub fn monotonic() -> u64 {
    let start = TSC_AT_BOOT.load(Ordering::Relaxed);
    match tsc::info() {
        Some(tsc) if start != 0 => {
            let elapsed = u128::from(tsc::read().saturating_sub(start));
            (elapsed * u128::from(NANOSECONDS_PER_SECOND) / u128::from(tsc.hz)) as u64
        }
        _ => ticks() * (NANOSECONDS_PER_SECOND / u64::from(HZ)),
    }
}

/// Nanoseconds since 1970-01-01 00:00 UTC.
pub fn realtime() -> u64 {
    WALL_AT_BOOT.load(Ordering::Relaxed) + monotonic()
}

/// Whether the monotonic clock has nanosecond steps (TSC) rather than ticks.
pub fn precise() -> bool {
    TSC_AT_BOOT.load(Ordering::Relaxed) != 0
}

/// Sleeps until the monotonic clock reaches `deadline`; gives up with `ERESTARTSYS` when a
/// signal is to be delivered. Precise to a tick (10 ms) for now.
pub fn sleep_until(deadline: u64) -> Result<(), crate::errno::Errno> {
    SLEEPERS.wait_interruptible(|| (monotonic() >= deadline).then_some(()))
}

/// Seconds since 1970 of a date (UTC): days from the civil date, as in Howard Hinnant's
/// `days_from_civil`.
fn unix_seconds(date: &rtc::DateTime) -> u64 {
    let month = u64::from(date.month);
    let year = u64::from(date.year) - u64::from(month <= 2);
    let era = year / 400;
    let year_of_era = year - era * 400;
    let shifted_month = (month + 9) % 12; // March is 0
    let day_of_year = (153 * shifted_month + 2) / 5 + u64::from(date.day) - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    let days = era * 146_097 + day_of_era - 719_468;
    days * 86_400
        + u64::from(date.hour) * 3600
        + u64::from(date.minute) * 60
        + u64::from(date.second)
}
