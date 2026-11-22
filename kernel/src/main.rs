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

// `Box`, `Vec`, `String`…: the heap part of the standard library, backed by `ochra_frame::mm::heap`.
extern crate alloc;

mod banner;
mod elf;
mod errno;
mod initramfs;
mod logger;
mod memory;
mod process;
mod sched;
mod selftest;
mod syscall;
mod time;

use core::sync::atomic::{AtomicBool, Ordering};

use log::{debug, info, trace};
use ochra_frame::BootInfo;
use ochra_frame::arch::{self, apic, exceptions, interrupts, pic};
use ochra_frame::boot::MemoryKind;
use ochra_frame::mm::{FRAME_SIZE, frames, heap, paging};
use ochra_frame::smp::{self, CpuInfo};

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

    let seeding = ochra_frame::random::init();
    if seeding.is_weak() {
        log::warn!("random: no usable RDSEED or RDRAND, seeded from the time stamp counter only");
    }
    info!(
        "random: ChaCha20 seeded (RDSEED {}, RDRAND {}, TSC)",
        seeding.rdseed, seeding.rdrand
    );

    let memory = frames::init(boot);
    info!(
        "physical memory: {} MiB in {} frames managed, {} free",
        memory.total * FRAME_SIZE / (1024 * 1024),
        memory.total,
        memory.free
    );

    let paging = paging::init(boot);
    info!(
        "page tables: {} kernel pages (code r-x, rodata r--, data rw-), {} MiB direct map \
         ({} huge pages), {} tables; double fault stack has a guard page",
        paging.kernel_pages,
        paging.direct_map_bytes / (1024 * 1024),
        paging.huge_pages,
        paging.page_tables
    );

    let heap = heap::init();
    info!("heap: {} KiB mapped, grows on demand", heap.mapped / 1024);

    match boot
        .initramfs()
        .map(|archive| (archive.len(), initramfs::init(archive)))
    {
        Some((bytes, Some(files))) => info!("initramfs: {files} files, {} KiB", bytes / 1024),
        Some((_, None)) => log::error!("initramfs: not a valid cpio archive, ignored"),
        None => log::warn!("initramfs: none loaded"),
    }

    selftest::prepare(command_line);
    smp::init_boot_cpu();
    time::init(smp::cpu_count());

    let local_apic = apic::init();
    pic::disable();
    info!(
        "interrupts: legacy PIC masked, local APIC in {} mode (id {})",
        local_apic.kind, local_apic.id
    );
    interrupts::set_handler(apic::TIMER_VECTOR, time::on_tick);
    let timer = apic::start_timer(time::HZ);
    info!(
        "timer: local APIC timer at {} Hz ({} counts/ms, calibrated with the PIT)",
        timer.hz, timer.counts_per_ms
    );
    interrupts::enable();

    // The other CPUs set up their own local APIC and timer, which needs the boot CPU's done.
    let others = smp::start_other_cpus(ap_main);
    info!(
        "smp: {} CPUs, {others} started besides the boot CPU",
        smp::cpu_count()
    );

    selftest::run_checks();
    exceptions::breakpoint_self_test();

    // `test=<name>` runs one of the built-in self-tests (see `selftest.rs`).
    if let Some(test) = command_line
        .split_whitespace()
        .find_map(|argument| argument.strip_prefix("test="))
    {
        selftest::run(test);
    }

    // From here on the kernel runs as tasks, each on its own stack with a guard page; the
    // boot stack (Limine's) is left behind.
    syscall::init();
    selftest::spawn_tasks();
    if !selftest::runs_user_tests(command_line) {
        start_init(command_line);
    }
    // One idle task per CPU, so every CPU always has something to run.
    for _ in 0..smp::cpu_count() {
        sched::spawn("idle", idle);
    }
    sched::start(true);
}

/// Starts the first user program: `/init`, or the one named by `rdinit=` (as in Linux).
/// Like Linux, it gets `HOME=/` and `TERM=linux` as its environment.
fn start_init(command_line: &str) {
    let path = command_line
        .split_whitespace()
        .find_map(|argument| argument.strip_prefix("rdinit="))
        .unwrap_or("/init");
    match process::spawn(path, &[path], &["HOME=/", "TERM=linux"], false) {
        Ok(process) => {
            info!("started {path} as process {}", process.id);
            selftest::watch_init(&process);
        }
        Err(errno) => log::error!("cannot start {path} (errno {errno})"),
    }
}

/// Where the other CPUs (APs) arrive once their CPU state is set up. They start their timer
/// and join the scheduler.
fn ap_main(cpu: CpuInfo) -> ! {
    let local_apic = apic::init();
    apic::start_timer(time::HZ);
    info!(
        "cpu {} online (APIC id {}, {}), CPU protections: {}",
        cpu.index, cpu.apic_id, local_apic.kind, cpu.protections
    );
    interrupts::enable();
    selftest::on_ap(cpu.index);
    sched::start(false);
}

static BOOT_FINISHED: AtomicBool = AtomicBool::new(false);

/// Runs when no other task has work: sleeps until the next interrupt.
fn idle() -> ! {
    while !selftest::tasks_finished() {
        interrupts::wait();
    }
    // Every idle task gets here; only the first one reports.
    if !BOOT_FINISHED.swap(true, Ordering::Relaxed) {
        info!(
            "boot finished, idling ({} timer ticks so far)",
            time::ticks()
        );
    }
    loop {
        interrupts::wait();
    }
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
