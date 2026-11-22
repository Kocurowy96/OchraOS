// SPDX-License-Identifier: GPL-2.0-only

//! The panic handler for the whole kernel.

use core::fmt::Write;
use core::panic::PanicInfo;

use crate::arch::halt_forever;
use crate::color::{BOLD, RED, RESET};
use crate::serial;

/// Called by `core` when the kernel panics: print what happened, then stop.
#[panic_handler]
fn panic(info: &PanicInfo) -> ! {
    // Not `println!`: the panic may have happened while the COM1 lock was held.
    let mut serial = serial::emergency_writer();
    // If even this write fails there is nobody left to tell, so the result is ignored.
    let _ = writeln!(
        serial,
        "\n{RED}{BOLD}KERNEL PANIC{RESET}{RED}: {info}{RESET}"
    );
    crate::arch::exceptions::write_stopped(&mut serial);
    halt_forever();
}
