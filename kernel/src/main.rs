// SPDX-License-Identifier: GPL-2.0-only

//! The OchraOS kernel.

// No standard library: it needs an operating system underneath, and we *are* the OS.
#![no_std]
// No `main`: nothing calls it. The bootloader jumps straight to `kmain` (see `linker.ld`).
#![no_main]

mod arch;
mod banner;
mod color;
mod logger;
mod selftest;
mod serial;

use core::fmt::Write;
use core::panic::PanicInfo;

use limine::BaseRevision;
use limine::memory_map::EntryType;
use limine::request::{
    BootloaderInfoRequest, ExecutableAddressRequest, ExecutableCmdlineRequest, HhdmRequest,
    MemoryMapRequest, RequestsEndMarker, RequestsStartMarker,
};
use log::{debug, error, info, trace};

use crate::arch::halt_forever;
use crate::color::{BOLD, RED, RESET};

// Limine requests.
//
// A request is a static with a magic number that Limine looks for in the kernel file before
// jumping to it; if Limine understands it, it fills in a pointer to the answer. The statics
// live in the `.requests*` sections, which `linker.ld` keeps between the start and end
// markers. `#[used]` stops the compiler from deleting the ones no code reads directly.

/// Asks for revision 3 of the Limine protocol, the newest one the `limine` crate (0.5) knows.
#[used]
#[unsafe(link_section = ".requests")]
static BASE_REVISION: BaseRevision = BaseRevision::new();

/// Name and version of the bootloader, for the log.
#[used]
#[unsafe(link_section = ".requests")]
static BOOTLOADER_INFO: BootloaderInfoRequest = BootloaderInfoRequest::new();

/// The kernel command line (`cmdline:` in `limine.conf`).
#[used]
#[unsafe(link_section = ".requests")]
static COMMAND_LINE: ExecutableCmdlineRequest = ExecutableCmdlineRequest::new();

/// The physical memory map: which RAM is free, reserved, used by the kernel…
#[used]
#[unsafe(link_section = ".requests")]
static MEMORY_MAP: MemoryMapRequest = MemoryMapRequest::new();

/// Where the kernel was loaded, physically and virtually.
#[used]
#[unsafe(link_section = ".requests")]
static EXECUTABLE_ADDRESS: ExecutableAddressRequest = ExecutableAddressRequest::new();

/// The "higher half direct map": Limine maps all physical memory at one fixed virtual offset.
#[used]
#[unsafe(link_section = ".requests")]
static HHDM: HhdmRequest = HhdmRequest::new();

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
    // Without a serial port we have no way to say anything, so there is nothing to do but stop.
    if serial::COM1.lock().init().is_err() {
        halt_forever();
    }
    banner::print();
    logger::init();

    // A bootloader that does not speak our protocol revision may have set things up
    // differently than we expect; stop before relying on any of it.
    if !BASE_REVISION.is_supported() {
        error!("the bootloader does not support Limine protocol base revision 3");
        halt_forever();
    }

    log_boot_info();
    let command_line = command_line();
    info!("command line: \"{command_line}\"");

    arch::init();
    info!("GDT, TSS and IDT loaded");
    arch::exceptions::breakpoint_self_test();

    // `test=<name>` runs one of the built-in self-tests (see `selftest.rs`).
    if let Some(test) = command_line
        .split_whitespace()
        .find_map(|argument| argument.strip_prefix("test="))
    {
        selftest::run(test);
    }

    info!("stage 0 boot finished, halting");
    halt_forever();
}

/// The kernel command line, or an empty string if there is none (or it is not UTF-8).
fn command_line() -> &'static str {
    COMMAND_LINE
        .get_response()
        .and_then(|response| response.cmdline().to_str().ok())
        .unwrap_or("")
}

/// Logs what Limine told us about the machine.
fn log_boot_info() {
    if let Some(bootloader) = BOOTLOADER_INFO.get_response() {
        info!(
            "bootloader: {} {} (protocol base revision {})",
            bootloader.name(),
            bootloader.version(),
            BASE_REVISION.loaded_revision().unwrap_or(0)
        );
    }

    if let Some(address) = EXECUTABLE_ADDRESS.get_response() {
        info!(
            "loaded at physical {:#x}, virtual {:#x}",
            address.physical_base(),
            address.virtual_base()
        );
    }

    if let Some(hhdm) = HHDM.get_response() {
        debug!("direct map of physical memory at {:#x}", hhdm.offset());
    }

    if let Some(memory_map) = MEMORY_MAP.get_response() {
        let entries = memory_map.entries();
        let mut usable_bytes = 0;
        for entry in entries {
            trace!(
                "memory {:#012x}..{:#012x} {}",
                entry.base,
                entry.base + entry.length,
                memory_type_name(entry.entry_type)
            );
            if entry.entry_type == EntryType::USABLE {
                usable_bytes += entry.length;
            }
        }
        info!(
            "memory: {} MiB usable, {} regions in the map",
            usable_bytes / (1024 * 1024),
            entries.len()
        );
    }
}

fn memory_type_name(entry_type: EntryType) -> &'static str {
    match entry_type {
        EntryType::USABLE => "usable",
        EntryType::RESERVED => "reserved",
        EntryType::ACPI_RECLAIMABLE => "ACPI reclaimable",
        EntryType::ACPI_NVS => "ACPI NVS",
        EntryType::BAD_MEMORY => "bad",
        EntryType::BOOTLOADER_RECLAIMABLE => "bootloader reclaimable",
        EntryType::EXECUTABLE_AND_MODULES => "kernel and modules",
        EntryType::FRAMEBUFFER => "framebuffer",
        _ => "unknown",
    }
}

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
    halt_forever();
}
