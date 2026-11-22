// SPDX-License-Identifier: GPL-2.0-only

//! The OchraOS kernel.

// No standard library: it needs an operating system underneath, and we *are* the OS.
#![no_std]
// No `main`: nothing calls it. The bootloader jumps straight to `kmain` (see `linker.ld`).
#![no_main]

use core::arch::asm;
use core::panic::PanicInfo;

use limine::BaseRevision;
use limine::request::{RequestsEndMarker, RequestsStartMarker};

// Limine requests.
//
// A request is a static with a magic number that Limine looks for in the kernel file before
// jumping to it; if Limine understands it, it fills in a pointer to the answer. The statics
// live in the `.requests*` sections, which `linker.ld` keeps between the start and end
// markers. `#[used]` stops the compiler from deleting them, since no code reads them directly.

/// Asks for revision 3 of the Limine protocol, the newest one the `limine` crate (0.5) knows.
#[used]
#[unsafe(link_section = ".requests")]
static BASE_REVISION: BaseRevision = BaseRevision::new();

#[used]
#[unsafe(link_section = ".requests_start_marker")]
static REQUESTS_START: RequestsStartMarker = RequestsStartMarker::new();

#[used]
#[unsafe(link_section = ".requests_end_marker")]
static REQUESTS_END: RequestsEndMarker = RequestsEndMarker::new();

/// Kernel entry point. Limine jumps here in 64-bit mode, with paging set up, the kernel
/// mapped in the higher half, interrupts off and a 64 KiB stack.
#[unsafe(no_mangle)]
extern "C" fn kmain() -> ! {
    // A bootloader that does not speak our protocol revision may have set things up
    // differently than we expect. There is no output yet, so all we can do is stop.
    if !BASE_REVISION.is_supported() {
        halt_forever();
    }

    halt_forever();
}

/// Called by `core` when the kernel panics. There is nothing to unwind to, so we stop here.
#[panic_handler]
fn panic(_info: &PanicInfo) -> ! {
    halt_forever();
}

/// Stops this CPU for good.
fn halt_forever() -> ! {
    loop {
        // SAFETY: `cli` and `hlt` touch no memory and no Rust-visible state. With interrupts
        // off, `hlt` sleeps until a non-maskable interrupt; the loop puts us back to sleep.
        unsafe { asm!("cli", "hlt", options(nomem, nostack)) };
    }
}
