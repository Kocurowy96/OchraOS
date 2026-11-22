// SPDX-License-Identifier: GPL-2.0-only

//! x86-64 specific code: everything that needs inline assembly lives here.
//!
//! Keeping the assembly in one place makes the `unsafe` easy to find and review. It is meant
//! to move into the dedicated low-level crate once that exists (see CLAUDE.md, rule 3).

pub mod port;

use core::arch::asm;

/// Stops this CPU for good.
pub fn halt_forever() -> ! {
    loop {
        // SAFETY: `cli` and `hlt` touch no memory and no Rust-visible state. With interrupts
        // off, `hlt` sleeps until a non-maskable interrupt; the loop puts us back to sleep.
        unsafe { asm!("cli", "hlt", options(nomem, nostack)) };
    }
}
