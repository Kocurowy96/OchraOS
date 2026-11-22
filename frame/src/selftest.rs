// SPDX-License-Identifier: GPL-2.0-only

//! Crash triggers for the kernel's self-tests (`test=<name>` on the command line, see
//! `kernel/src/selftest.rs`). Each one breaks a protection on purpose; the exception handler
//! must then print the matching report and stop.
//!
//! Only built with the `selftest` feature, which `lib.rs` refuses in release builds.

use core::arch::asm;

use crate::mm::paging;

/// Page fault: write to an address nothing is mapped at.
pub fn page_fault() -> ! {
    // SAFETY: nothing is mapped in the low 4 GiB (Limine maps nothing there for protocol
    // revision 3, and neither do our page tables), so the write faults instead of changing
    // memory, and the page fault handler never returns.
    unsafe { core::ptr::write_volatile(0xdead_beef as *mut u8, 42) };
    unreachable!("the write to 0xdeadbeef did not fault");
}

/// Double fault, the way a kernel stack overflow causes one: with RSP pointing to unmapped
/// memory, the `push` faults, and the CPU cannot push the page fault frame onto that same
/// stack either. Only the separate IST stack saves the day.
pub fn double_fault() -> ! {
    // SAFETY: control never comes back here: the double fault handler halts. The broken
    // stack pointer is never used by Rust code.
    unsafe { asm!("mov rsp, 0xdead0000", "push rax", options(noreturn)) };
}

/// W^X: write into the kernel's own code, which is mapped read-only.
pub fn write_to_code() -> ! {
    let code = write_to_code as *const () as *mut u8;
    // SAFETY: the page is mapped without write permission, so the write faults and the handler
    // halts; no code is changed.
    unsafe { code.write_volatile(0xCC) };
    unreachable!("writing to kernel code did not fault");
}

/// NX: execute a byte of kernel data, which is mapped no-execute.
pub fn execute_data() -> ! {
    /// A `ret` instruction, stored in writable data.
    static RET: [u8; 1] = [0xC3];
    let data = &raw const RET;
    // SAFETY: the fetch from a no-execute page faults before the instruction runs, and the
    // handler halts. If NX were off it would run `ret` and come back, which `unreachable!`
    // then reports.
    unsafe { asm!("call {}", in(reg) data, clobber_abi("C")) };
    unreachable!("executing kernel data did not fault");
}

/// Guard page: write just below the double fault stack.
pub fn stack_guard_page() -> ! {
    let below = (paging::double_fault_stack_bottom() - 8) as *mut u64;
    // SAFETY: the page below every kernel stack is left unmapped, so the write faults and the
    // handler halts.
    unsafe { below.write_volatile(0) };
    unreachable!("the stack guard page is mapped");
}

/// SMEP: the kernel calls code in a user page.
pub fn smep() -> ! {
    let code = paging::map_user_test_page();
    // SAFETY: with SMEP the instruction fetch from a user page faults and the handler halts.
    // Without SMEP the page holds a single `ret`, so the call returns and `unreachable!`
    // reports the missing protection.
    unsafe { asm!("call {}", in(reg) code, clobber_abi("C")) };
    unreachable!("SMEP is off: the kernel executed a user page");
}

/// SMAP: the kernel reads a user page.
pub fn smap() -> ! {
    let data = paging::map_user_test_page() as *const u8;
    // SAFETY: the page is mapped and readable; with SMAP the read faults and the handler
    // halts, without SMAP it just reads the `ret` byte.
    let byte = unsafe { data.read_volatile() };
    unreachable!("SMAP is off: the kernel read {byte:#x} from a user page");
}
