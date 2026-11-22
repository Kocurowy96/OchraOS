// SPDX-License-Identifier: GPL-2.0-only

//! CPU protection features, switched on at boot when the CPU has them.
//!
//! | feature | register bit | what it stops |
//! |---------|--------------|---------------|
//! | SMEP (Supervisor Mode Execution Prevention) | CR4 bit 20 | the kernel executing code from user pages — a classic exploit trick |
//! | SMAP (Supervisor Mode Access Prevention) | CR4 bit 21 | the kernel reading or writing user pages by accident; deliberate accesses (copying syscall arguments, stage 2) open a short window with `stac`/`clac` |
//! | UMIP (User Mode Instruction Prevention) | CR4 bit 11 | user programs reading kernel table addresses with `sgdt`, `sidt`, `sldt`, `smsw`, `str` |
//! | WP (Write Protect) | CR0 bit 16 | the kernel writing to read-only pages; without it W^X would not bind the kernel itself |
//! | NX (No-Execute) | EFER bit 11 | executing data pages; needed for W^X at all |
//!
//! Which of them a CPU has is read with `cpuid`. Older CPUs (e.g. before Intel Broadwell for
//! SMAP) lack some, so we switch on what is there and report the rest.

use core::arch::asm;
use core::arch::x86_64::__cpuid_count;
use core::fmt;

const CR0_WRITE_PROTECT: u64 = 1 << 16;
const CR4_UMIP: u64 = 1 << 11;
const CR4_SMEP: u64 = 1 << 20;
const CR4_SMAP: u64 = 1 << 21;

/// Whether SMAP is on, so `stac`/`clac` exist (they are invalid opcodes on CPUs without SMAP).
pub(crate) static SMAP_ENABLED: core::sync::atomic::AtomicBool =
    core::sync::atomic::AtomicBool::new(false);
const EFER_NO_EXECUTE: u64 = 1 << 11;
/// Model-specific register number of EFER (Extended Feature Enable Register).
const IA32_EFER: u32 = 0xC000_0080;

/// Which protections are on after [`enable_protections`].
#[derive(Clone, Copy, Debug)]
pub struct Protections {
    pub smep: bool,
    pub smap: bool,
    pub umip: bool,
    pub write_protect: bool,
    pub no_execute: bool,
}

impl fmt::Display for Protections {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        let state = |on: bool| if on { "on" } else { "off (not supported)" };
        write!(
            f,
            "SMEP {}, SMAP {}, UMIP {}, WP {}, NX {}",
            state(self.smep),
            state(self.smap),
            state(self.umip),
            if self.write_protect { "on" } else { "off" },
            if self.no_execute { "on" } else { "off" },
        )
    }
}

/// Switches on every protection this CPU supports and reports the result.
pub fn enable_protections() -> Protections {
    // CPUID leaf 7, sub-leaf 0: "structured extended feature flags".
    let features = __cpuid_count(7, 0);
    let has_smep = features.ebx & (1 << 7) != 0;
    let has_smap = features.ebx & (1 << 20) != 0;
    let has_umip = features.ecx & (1 << 2) != 0;

    let mut cr4 = read_cr4();
    if has_smep {
        cr4 |= CR4_SMEP;
    }
    if has_smap {
        cr4 |= CR4_SMAP;
        // All CPUs of a machine are the same model, so one flag serves them all.
        SMAP_ENABLED.store(true, core::sync::atomic::Ordering::Relaxed);
    }
    if has_umip {
        cr4 |= CR4_UMIP;
    }
    // SAFETY: we only add bits for features CPUID reports, and none of them affects the kernel:
    // all its pages are supervisor pages (Limine maps nothing for user mode), and it never
    // uses the instructions UMIP restricts in user mode.
    unsafe { write_cr4(cr4) };

    let mut cr0 = read_cr0();
    cr0 |= CR0_WRITE_PROTECT;
    // SAFETY: WP only makes the kernel obey the read-only bit of its own page mappings. Limine
    // maps the kernel's writable data writable, so correct code is unaffected.
    unsafe { write_cr0(cr0) };

    let cr4 = read_cr4();
    Protections {
        smep: cr4 & CR4_SMEP != 0,
        smap: cr4 & CR4_SMAP != 0,
        umip: cr4 & CR4_UMIP != 0,
        write_protect: read_cr0() & CR0_WRITE_PROTECT != 0,
        // Limine switches NX on (it maps the kernel's data non-executable); we only check.
        no_execute: read_msr(IA32_EFER) & EFER_NO_EXECUTE != 0,
    }
}

fn read_cr0() -> u64 {
    let value;
    // SAFETY: reading CR0 has no side effects and touches no memory.
    unsafe { asm!("mov {}, cr0", out(reg) value, options(nomem, nostack, preserves_flags)) };
    value
}

/// # Safety
///
/// The new value must keep the CPU in a state the kernel can run in (protected mode, paging).
unsafe fn write_cr0(value: u64) {
    // SAFETY: the caller guarantees the value is valid.
    unsafe { asm!("mov cr0, {}", in(reg) value, options(nostack, preserves_flags)) };
}

fn read_cr4() -> u64 {
    let value;
    // SAFETY: reading CR4 has no side effects and touches no memory.
    unsafe { asm!("mov {}, cr4", out(reg) value, options(nomem, nostack, preserves_flags)) };
    value
}

/// # Safety
///
/// The new value must keep paging working (PAE, …) and only set bits for features the CPU
/// supports (otherwise the write itself raises #GP).
unsafe fn write_cr4(value: u64) {
    // SAFETY: the caller guarantees the value is valid.
    unsafe { asm!("mov cr4, {}", in(reg) value, options(nostack, preserves_flags)) };
}

/// Reads a model-specific register.
pub(crate) fn read_msr(msr: u32) -> u64 {
    let (low, high): (u32, u32);
    // SAFETY: only called with MSRs that exist on every CPU we run on (EFER, APIC base) or
    // after CPUID confirmed them (x2APIC registers); reading them has no side effects.
    unsafe {
        asm!("rdmsr", in("ecx") msr, out("eax") low, out("edx") high,
             options(nomem, nostack, preserves_flags));
    }
    (u64::from(high) << 32) | u64::from(low)
}

/// Writes a model-specific register.
///
/// # Safety
///
/// The MSR must exist, and the value must not break anything the kernel relies on.
pub(crate) unsafe fn write_msr(msr: u32, value: u64) {
    let (low, high) = (value as u32, (value >> 32) as u32);
    // SAFETY: guaranteed by the caller.
    unsafe {
        asm!("wrmsr", in("ecx") msr, in("eax") low, in("edx") high,
             options(nostack, preserves_flags));
    }
}
