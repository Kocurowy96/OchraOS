// SPDX-License-Identifier: GPL-2.0-only

//! Starting the other CPU cores (SMP: symmetric multiprocessing).
//!
//! At power-on only one core runs, the bootstrap processor (BSP); the others, application
//! processors (APs), wait. Waking one up by hand means sending it INIT and SIPI interrupts
//! and running it through 16- and 32-bit mode; Limine does that for us (the MP request) and
//! leaves every AP in 64-bit mode, spinning until we write an address for it to jump to.
//!
//! Each AP then enters [`ap_entry`] on a 64 KiB stack from Limine, still on Limine's page
//! tables, with interrupts off. It sets up its CPU state like the BSP did and calls the
//! kernel's function for APs.
//!
//! CPUs get a logical index: the BSP is 0, the APs 1, 2, … in the order Limine lists them.

use core::sync::atomic::{AtomicBool, AtomicU32, Ordering};

use limine::mp::Cpu;
use spin::Once;

use crate::arch::{self, apic, cpu::Protections};
use crate::mm::paging;
use crate::{boot, cpu_local};

/// What an AP knows about itself when it reaches the kernel.
#[derive(Clone, Copy, Debug)]
pub struct CpuInfo {
    /// Logical index (the BSP is 0).
    pub index: u32,
    /// The local APIC ID, as the firmware (ACPI MADT) reports it.
    pub apic_id: u32,
    /// What [`cpu::enable_protections`](crate::arch::cpu::enable_protections) found on it.
    pub protections: Protections,
}

/// The kernel's function for APs.
static AP_MAIN: Once<fn(CpuInfo) -> !> = Once::new();
/// APs that finished their CPU setup.
static ONLINE: AtomicU32 = AtomicU32::new(0);

/// Number of CPUs, the BSP included (1 if the bootloader started no others).
pub fn cpu_count() -> u32 {
    boot::mp_response().map_or(1, |response| response.cpus().len() as u32)
}

/// Sets up the per-CPU data of the boot CPU (index 0). Call once, after the heap is up and
/// before [`start_other_cpus`].
pub fn init_boot_cpu() {
    let apic_id = boot::mp_response().map_or_else(
        // Without the MP answer: the initial APIC ID from CPUID (bits 24–31 of EBX).
        || core::arch::x86_64::__cpuid(1).ebx >> 24,
        |response| response.bsp_lapic_id(),
    );
    cpu_local::init(0, apic_id);
}

/// Starts every AP. Each one sets up its CPU state, then runs `main`. Returns the number of
/// APs, once all of them have finished the setup. Call once, after `paging::init`.
pub fn start_other_cpus(main: fn(CpuInfo) -> !) -> u32 {
    let Some(response) = boot::mp_response() else {
        return 0;
    };
    AP_MAIN.call_once(|| main);
    let bsp = response.bsp_lapic_id();
    let mut started = 0;
    for cpu in response.cpus().iter().filter(|cpu| cpu.lapic_id != bsp) {
        started += 1;
        cpu.extra.store(u64::from(started), Ordering::Relaxed);
        // Writing the address wakes the AP (Limine's `write` orders our stores before it).
        cpu.goto_address.write(ap_entry);
    }
    while ONLINE.load(Ordering::Acquire) < started {
        core::hint::spin_loop();
    }
    started
}

/// Set once a CPU has started stopping all the others (see [`stop_other_cpus`]).
static STOPPING: AtomicBool = AtomicBool::new(false);
/// CPUs that received the stop NMI and halted.
static STOPPED: AtomicU32 = AtomicU32::new(0);

/// How the other CPUs were stopped, for the crash report.
pub(crate) struct Stopped {
    pub(crate) stopped: u32,
    pub(crate) others: u32,
}

/// Stops every other CPU with a non-maskable interrupt (it arrives even while a CPU has
/// interrupts off, e.g. spinning on a lock), so nothing runs on after a crash. Waits a
/// bounded time for them to confirm. Returns `None` if another CPU is already stopping
/// everyone: then this one is about to be stopped too.
pub(crate) fn stop_other_cpus() -> Option<Stopped> {
    if STOPPING.swap(true, Ordering::AcqRel) {
        return None;
    }
    let others = ONLINE.load(Ordering::Acquire);
    if others > 0 && apic::send_nmi_to_others() {
        let mut spins: u32 = 0;
        while STOPPED.load(Ordering::Acquire) < others && spins < 100_000_000 {
            spins += 1;
            core::hint::spin_loop();
        }
    }
    Some(Stopped {
        stopped: STOPPED.load(Ordering::Acquire),
        others,
    })
}

/// Called for every NMI: `true` if it was the stop signal (the caller then halts).
pub(crate) fn stop_requested() -> bool {
    if STOPPING.load(Ordering::Acquire) {
        STOPPED.fetch_add(1, Ordering::Release);
        true
    } else {
        false
    }
}

/// Where every AP starts.
extern "C" fn ap_entry(cpu: &Cpu) -> ! {
    // Our page tables first: they map everything Limine's did that we still use (the kernel
    // image, the direct map with this stack and `cpu`), plus the heap and our stacks.
    paging::activate_kernel_space();
    let protections = arch::init_ap();
    let index = cpu.extra.load(Ordering::Relaxed) as u32;
    cpu_local::init(index, cpu.lapic_id);
    let info = CpuInfo {
        index,
        apic_id: cpu.lapic_id,
        protections,
    };
    ONLINE.fetch_add(1, Ordering::Release);
    let main = AP_MAIN.get().expect("set before any AP is woken up");
    main(info)
}
