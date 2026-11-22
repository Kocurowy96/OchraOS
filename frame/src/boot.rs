// SPDX-License-Identifier: GPL-2.0-only

//! Boot: the entry point Limine jumps to, the Limine requests, and [`BootInfo`] — a safe view
//! of what the bootloader told us.
//!
//! The kernel crate cannot export the entry symbol itself (that needs `#[unsafe(no_mangle)]`,
//! and the kernel is `#![forbid(unsafe_code)]`), and `ochra-frame` cannot call into the
//! kernel crate (the dependency points the other way). So:
//!
//! 1. Limine jumps to `kmain` here.
//! 2. `kmain` prepares the minimum (serial port, protocol check) and calls
//!    `__ochra_kernel_main`, which is only *declared* here.
//! 3. The kernel defines that function with [`entry!`](crate::entry), naming its safe `main`.

use limine::BaseRevision;
use limine::memory_map::EntryType;
use limine::request::{
    BootloaderInfoRequest, ExecutableAddressRequest, ExecutableCmdlineRequest, HhdmRequest,
    MemoryMapRequest, RequestsEndMarker, RequestsStartMarker,
};

use crate::arch::halt_forever;
use crate::color::{RED, RESET};
use crate::{println, serial};

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

unsafe extern "Rust" {
    // SAFETY: this symbol is defined exactly once, by `entry!` in the kernel crate, with this
    // exact signature (the macro checks the kernel's function against it). A missing or
    // duplicate definition is a link error, not undefined behaviour.
    safe fn __ochra_kernel_main(boot: &'static BootInfo) -> !;
}

/// Kernel entry point. Limine jumps here in 64-bit mode, with paging set up, the kernel
/// mapped in the higher half, interrupts off and a 64 KiB stack.
#[unsafe(no_mangle)]
extern "C" fn kmain() -> ! {
    // Without a serial port we have no way to say anything, so there is nothing to do but stop.
    if serial::COM1.lock().init().is_err() {
        halt_forever();
    }
    // A bootloader that does not speak our protocol revision may have set things up
    // differently than we expect; stop before relying on any of it.
    if !BASE_REVISION.is_supported() {
        println!("{RED}the bootloader does not support Limine protocol base revision 3{RESET}");
        halt_forever();
    }
    match BOOT_INFO.direct_map_offset() {
        Some(offset) => crate::mm::set_direct_map_offset(offset),
        None => {
            println!("{RED}the bootloader did not provide a direct map of physical memory{RESET}");
            halt_forever();
        }
    }
    __ochra_kernel_main(&BOOT_INFO);
}

/// Defines the kernel's entry point: `ochra_frame::entry!(main);` makes `kmain` call
/// `fn main(boot: &'static BootInfo) -> !` once the boot environment is ready.
///
/// Use it exactly once, in the kernel crate.
#[macro_export]
macro_rules! entry {
    ($main:path) => {
        // SAFETY: `no_mangle` exports this function under the name that `ochra_frame::boot`
        // declares and calls. It is sound because the signature is the declared one — the
        // `let` below fails to compile for any other — and because using the macro twice is
        // a duplicate-symbol link error. The macro takes only a function path, never code.
        #[unsafe(no_mangle)]
        fn __ochra_kernel_main(boot: &'static $crate::BootInfo) -> ! {
            let main: fn(&'static $crate::BootInfo) -> ! = $main;
            main(boot)
        }
    };
}

/// What the bootloader told us, behind safe methods.
///
/// Only `ochra-frame` can create it, and only after the protocol check in `kmain`, so holding
/// a `&BootInfo` means the bootloader's answers can be trusted.
pub struct BootInfo {
    _private: (),
}

static BOOT_INFO: BootInfo = BootInfo { _private: () };

/// Physical and virtual address the kernel was loaded at.
#[derive(Clone, Copy, Debug)]
pub struct KernelAddress {
    pub physical: u64,
    pub virtual_: u64,
}

/// One region of the physical memory map.
#[derive(Clone, Copy, Debug)]
pub struct MemoryRegion {
    /// Physical start address.
    pub start: u64,
    /// Length in bytes.
    pub length: u64,
    pub kind: MemoryKind,
}

impl MemoryRegion {
    /// Physical address just past the end.
    pub fn end(&self) -> u64 {
        self.start + self.length
    }
}

/// What a region of physical memory is used for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MemoryKind {
    /// Free RAM.
    Usable,
    Reserved,
    /// ACPI tables; free once they have been read.
    AcpiReclaimable,
    /// Must be preserved for the firmware.
    AcpiNvs,
    Bad,
    /// Used by Limine (page tables, our stack, the answers); free once we no longer need it.
    BootloaderReclaimable,
    /// The kernel itself and boot modules.
    KernelAndModules,
    Framebuffer,
    Unknown,
}

impl MemoryKind {
    pub fn name(self) -> &'static str {
        match self {
            Self::Usable => "usable",
            Self::Reserved => "reserved",
            Self::AcpiReclaimable => "ACPI reclaimable",
            Self::AcpiNvs => "ACPI NVS",
            Self::Bad => "bad",
            Self::BootloaderReclaimable => "bootloader reclaimable",
            Self::KernelAndModules => "kernel and modules",
            Self::Framebuffer => "framebuffer",
            Self::Unknown => "unknown",
        }
    }

    fn from_limine(entry_type: EntryType) -> Self {
        match entry_type {
            EntryType::USABLE => Self::Usable,
            EntryType::RESERVED => Self::Reserved,
            EntryType::ACPI_RECLAIMABLE => Self::AcpiReclaimable,
            EntryType::ACPI_NVS => Self::AcpiNvs,
            EntryType::BAD_MEMORY => Self::Bad,
            EntryType::BOOTLOADER_RECLAIMABLE => Self::BootloaderReclaimable,
            EntryType::EXECUTABLE_AND_MODULES => Self::KernelAndModules,
            EntryType::FRAMEBUFFER => Self::Framebuffer,
            _ => Self::Unknown,
        }
    }
}

impl BootInfo {
    /// Bootloader name and version.
    pub fn bootloader(&self) -> Option<(&'static str, &'static str)> {
        BOOTLOADER_INFO
            .get_response()
            .map(|info| (info.name(), info.version()))
    }

    /// The boot protocol revision the bootloader actually uses.
    pub fn protocol_revision(&self) -> u64 {
        BASE_REVISION.loaded_revision().unwrap_or(0)
    }

    /// The kernel command line, or an empty string if there is none (or it is not UTF-8).
    pub fn command_line(&self) -> &'static str {
        COMMAND_LINE
            .get_response()
            .and_then(|response| response.cmdline().to_str().ok())
            .unwrap_or("")
    }

    pub fn kernel_address(&self) -> Option<KernelAddress> {
        EXECUTABLE_ADDRESS
            .get_response()
            .map(|address| KernelAddress {
                physical: address.physical_base(),
                virtual_: address.virtual_base(),
            })
    }

    /// Virtual address at which all physical memory is mapped.
    pub fn direct_map_offset(&self) -> Option<u64> {
        HHDM.get_response().map(|hhdm| hhdm.offset())
    }

    /// The physical memory map, in ascending address order.
    pub fn memory_regions(&self) -> impl Iterator<Item = MemoryRegion> {
        MEMORY_MAP
            .get_response()
            .map(|map| map.entries())
            .unwrap_or(&[])
            .iter()
            .map(|entry| MemoryRegion {
                start: entry.base,
                length: entry.length,
                kind: MemoryKind::from_limine(entry.entry_type),
            })
    }
}
