// SPDX-License-Identifier: GPL-2.0-only

//! TLB shootdown: telling other CPUs to forget a translation.
//!
//! Every CPU caches page table translations in its TLB (translation lookaside buffer). When
//! a mapping is removed or restricted, the CPU that changed it flushes its own TLB entry
//! (`invlpg`), but other CPUs that run the same address space may still use the old
//! translation, e.g. keep writing to a frame that was already freed. So they are sent an IPI
//! asking them to flush too, and the changing CPU waits until all have done so before it
//! frees anything.
//!
//! Only one request is in flight at a time. A CPU waiting to send one, or waiting for the
//! answers, keeps answering requests addressed to it, so two CPUs shooting down at the same
//! time cannot wait for each other for ever. Callers must not hold any lock: a CPU spinning on
//! that lock with interrupts off could never answer.
//!
//! An address space that no CPU has in CR3 needs no shootdown at all: loading CR3 drops
//! every non-global translation (we do not use PCIDs).

use core::arch::asm;
use core::sync::atomic::{AtomicU32, AtomicU64, Ordering};

use spin::Mutex;

use super::PhysAddr;
use super::paging::{ADDRESS_MASK, read_cr3, write_cr3};
use crate::arch::{apic, interrupts};
use crate::{cpu_local, smp};

/// Interrupt vector of the shootdown IPI.
pub(crate) const VECTOR: u8 = 0xF0;

/// Beyond this many pages, reloading CR3 (flushing everything) is cheaper than `invlpg`s.
const MAX_SINGLE_FLUSHES: u64 = 32;

/// The request in flight. Written only by the CPU holding `SENDER`.
struct Request {
    root: AtomicU64,
    start: AtomicU64,
    pages: AtomicU64,
    /// Incremented for every request, after the fields above are written.
    sequence: AtomicU64,
    /// CPUs that have flushed for the current request.
    done: AtomicU32,
}

static REQUEST: Request = Request {
    root: AtomicU64::new(0),
    start: AtomicU64::new(0),
    pages: AtomicU64::new(0),
    sequence: AtomicU64::new(0),
    done: AtomicU32::new(0),
};

/// Held by the CPU sending a request. A plain spinlock, not an `IrqMutex`: it is only ever
/// taken with `try_lock`, while answering other requests.
static SENDER: Mutex<()> = Mutex::new(());

/// Registers the IPI handler. Call once, before any user address space exists.
pub(crate) fn init() {
    interrupts::set_handler(VECTOR, |_| answer());
}

/// Flushes `pages` pages from `start` of the address space with PML4 `root` on this CPU and,
/// if `on_other_cpus`, on every other CPU. Returns once all CPUs have flushed.
pub(super) fn flush(root: PhysAddr, start: u64, pages: u64, on_other_cpus: bool) {
    flush_local(root, start, pages);
    let others = smp::other_cpus_online();
    if !on_other_cpus || others == 0 {
        return;
    }
    let were_enabled = interrupts::disable();
    let guard = loop {
        if let Some(guard) = SENDER.try_lock() {
            break guard;
        }
        answer();
        core::hint::spin_loop();
    };
    REQUEST.root.store(root.0, Ordering::Relaxed);
    REQUEST.start.store(start, Ordering::Relaxed);
    REQUEST.pages.store(pages, Ordering::Relaxed);
    REQUEST.done.store(0, Ordering::Relaxed);
    REQUEST.sequence.fetch_add(1, Ordering::Release);
    apic::send_to_others(VECTOR);
    // The others answer from the IPI handler, or while spinning for `SENDER` themselves.
    while REQUEST.done.load(Ordering::Acquire) < others {
        core::hint::spin_loop();
    }
    drop(guard);
    if were_enabled {
        interrupts::enable();
    }
}

/// Answers the current request if this CPU has not done so yet.
fn answer() {
    let Some(cpu) = cpu_local::this() else {
        return;
    };
    let sequence = REQUEST.sequence.load(Ordering::Acquire);
    if cpu.tlb_sequence.swap(sequence, Ordering::Relaxed) == sequence {
        return;
    }
    let root = PhysAddr(REQUEST.root.load(Ordering::Relaxed));
    let start = REQUEST.start.load(Ordering::Relaxed);
    let pages = REQUEST.pages.load(Ordering::Relaxed);
    flush_local(root, start, pages);
    REQUEST.done.fetch_add(1, Ordering::Release);
}

/// Flushes the pages on this CPU, if it is using that address space.
fn flush_local(root: PhysAddr, start: u64, pages: u64) {
    let current = read_cr3() & ADDRESS_MASK;
    if current != root.0 {
        return;
    }
    if pages > MAX_SINGLE_FLUSHES {
        // SAFETY: reloading the current CR3 changes no mapping; it only empties the TLB.
        unsafe { write_cr3(PhysAddr(current)) };
        return;
    }
    for page in 0..pages {
        let address = start + page * super::FRAME_SIZE;
        // SAFETY: `invlpg` only drops a cached translation.
        unsafe { asm!("invlpg [{}]", in(reg) address, options(nostack, preserves_flags)) };
    }
}

/// Self-test support: sends an empty request to every other CPU and returns how many
/// answered (each one must).
#[cfg(feature = "selftest")]
pub fn broadcast_test() -> u32 {
    // A root no CPU uses: everyone answers, nobody flushes anything.
    flush(PhysAddr(0), 0, 1, true);
    REQUEST.done.load(Ordering::Acquire)
}
