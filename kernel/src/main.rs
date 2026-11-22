// SPDX-License-Identifier: GPL-2.0-only

//! The OchraOS kernel.
//!
//! Everything here is safe Rust: all code that needs `unsafe` lives in `ochra-frame`
//! (`frame/`), which also owns the boot entry point and calls our [`main`].

// No standard library: it needs an operating system underneath, and we *are* the OS.
#![no_std]
// No `main` in the usual sense: `ochra-frame` gets control from the bootloader (see `entry!`).
#![no_main]
// The point of the framekernel design: a bug in this crate cannot corrupt memory.
#![forbid(unsafe_code)]

mod banner;
mod logger;
mod selftest;

use log::{debug, info, trace};
use ochra_frame::BootInfo;
use ochra_frame::arch::{self, exceptions};
use ochra_frame::boot::MemoryKind;

ochra_frame::entry!(main);

/// Called by `ochra-frame` once the serial port works and the boot protocol is checked.
fn main(boot: &'static BootInfo) -> ! {
    banner::print();
    logger::init();

    log_boot_info(boot);
    let command_line = boot.command_line();
    info!("command line: \"{command_line}\"");

    let protections = arch::init();
    info!("GDT, TSS and IDT loaded");
    info!("CPU protections: {protections}");
    exceptions::breakpoint_self_test();

    // `test=<name>` runs one of the built-in self-tests (see `selftest.rs`).
    if let Some(test) = command_line
        .split_whitespace()
        .find_map(|argument| argument.strip_prefix("test="))
    {
        selftest::run(test);
    }

    info!("boot finished, halting");
    arch::halt_forever();
}

/// Logs what the bootloader told us about the machine.
fn log_boot_info(boot: &BootInfo) {
    if let Some((name, version)) = boot.bootloader() {
        info!(
            "bootloader: {name} {version} (protocol base revision {})",
            boot.protocol_revision()
        );
    }

    // A logged address gives away the random KASLR offset (see `SHOW_ADDRESSES`).
    if ochra_frame::SHOW_ADDRESSES {
        if let Some(address) = boot.kernel_address() {
            info!(
                "loaded at physical {:#x}, virtual {:#x}",
                address.physical, address.virtual_
            );
        }
        if let Some(offset) = boot.direct_map_offset() {
            debug!("direct map of physical memory at {offset:#x}");
        }
    } else {
        info!("kernel addresses hidden (KASLR)");
    }

    let mut usable_bytes = 0;
    let mut regions = 0;
    for region in boot.memory_regions() {
        trace!(
            "memory {:#012x}..{:#012x} {}",
            region.start,
            region.end(),
            region.kind.name()
        );
        if region.kind == MemoryKind::Usable {
            usable_bytes += region.length;
        }
        regions += 1;
    }
    info!(
        "memory: {} MiB usable, {regions} regions in the map",
        usable_bytes / (1024 * 1024)
    );
}
