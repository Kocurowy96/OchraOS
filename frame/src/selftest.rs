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

// Small user programs for the ring 3 self-tests, as position-independent machine code in
// read-only data; the kernel copies them into a user page. (Real programs come from the
// initramfs from stage 2 on; these test the entry and exit paths without any of that.)
core::arch::global_asm!(
    ".pushsection .rodata.ochra_user_tests, \"a\"",
    // hello: write, EFAULT for a kernel and an unmapped pointer, ENOSYS, preserved
    // registers across syscalls and preemption; exit_group(0), or exit_group(1) on a mismatch.
    ".global ochra_user_hello_start",
    "ochra_user_hello_start:",
    "mov rbx, 0x0C4A0C4A",
    "mov r12, 0x1234567",
    // The message length, computed here: a difference of labels is not an immediate in
    // Intel syntax.
    "lea rsi, [rip + 5f]",
    "lea rdx, [rip + 6f]",
    "sub rdx, rsi",
    "mov r13, rdx",
    "mov eax, 1",
    "mov edi, 1",
    "syscall",
    "cmp rax, r13",
    "jne 7f",
    "mov eax, 1",
    "mov edi, 1",
    "mov rsi, 0xffff800000000000",
    "mov edx, 4",
    "syscall",
    "cmp rax, -14",
    "jne 7f",
    "mov eax, 1",
    "mov edi, 1",
    "mov esi, 0x1000",
    "mov edx, 4",
    "syscall",
    "cmp rax, -14",
    "jne 7f",
    "mov eax, 1000",
    "syscall",
    "cmp rax, -38",
    "jne 7f",
    // Spin a while, so timer interrupts (and task switches) hit user mode.
    "mov ecx, 5000000",
    "3:",
    "pause",
    "dec ecx",
    "jnz 3b",
    "cmp rbx, 0x0C4A0C4A",
    "jne 7f",
    "cmp r12, 0x1234567",
    "jne 7f",
    "mov eax, 231",
    "xor edi, edi",
    "syscall",
    "7:",
    "mov eax, 231",
    "mov edi, 1",
    "syscall",
    "5:",
    ".ascii \"hello from ring 3\\n\"",
    "6:",
    ".global ochra_user_hello_end",
    "ochra_user_hello_end:",
    // kernel: reads kernel memory, which is supervisor-only.
    ".global ochra_user_kernel_start",
    "ochra_user_kernel_start:",
    "mov rax, 0xffff800000000000",
    "mov rax, [rax]",
    "ud2",
    ".global ochra_user_kernel_end",
    "ochra_user_kernel_end:",
    // hlt: a privileged instruction.
    ".global ochra_user_hlt_start",
    "ochra_user_hlt_start:",
    "hlt",
    "ud2",
    ".global ochra_user_hlt_end",
    "ochra_user_hlt_end:",
    ".popsection",
);

/// The machine code of a user test program: `hello`, `kernel` or `hlt`.
pub fn user_program(name: &str) -> &'static [u8] {
    unsafe extern "C" {
        // SAFETY: defined by the `global_asm!` above; only their addresses are used.
        safe static ochra_user_hello_start: u8;
        safe static ochra_user_hello_end: u8;
        safe static ochra_user_kernel_start: u8;
        safe static ochra_user_kernel_end: u8;
        safe static ochra_user_hlt_start: u8;
        safe static ochra_user_hlt_end: u8;
    }
    let (start, end) = match name {
        "hello" => (
            &raw const ochra_user_hello_start,
            &raw const ochra_user_hello_end,
        ),
        "kernel" => (
            &raw const ochra_user_kernel_start,
            &raw const ochra_user_kernel_end,
        ),
        "hlt" => (
            &raw const ochra_user_hlt_start,
            &raw const ochra_user_hlt_end,
        ),
        _ => panic!("no user test program {name:?}"),
    };
    let length = end as usize - start as usize;
    // SAFETY: the labels delimit bytes in read-only data that live as long as the kernel.
    unsafe { core::slice::from_raw_parts(start, length) }
}
