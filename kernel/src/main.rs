// SPDX-License-Identifier: GPL-2.0-only

//! The OchraOS kernel.
//!
//! For now this is only a placeholder that compiles for `x86_64-unknown-none`;
//! the real entry point and the Limine boot protocol come in the next step.

// No standard library: it needs an operating system underneath, and we *are* the OS.
#![no_std]
// No `main`: nothing calls it. The bootloader jumps straight to our entry symbol.
#![no_main]

use core::panic::PanicInfo;

/// Called by `core` when the kernel panics. There is nothing to unwind to, so we stop here.
#[panic_handler]
fn panic(_info: &PanicInfo) -> ! {
    loop {
        core::hint::spin_loop();
    }
}
