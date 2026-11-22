// SPDX-License-Identifier: GPL-2.0-only

//! The `syscall` instruction: how user programs enter the kernel.
//!
//! `syscall` jumps to the address in the `LSTAR` MSR, in ring 0, with the code and stack
//! selectors from `STAR`. It does not switch stacks and does not save much: the user's RIP
//! goes to RCX and RFLAGS to R11, and the flags listed in `SFMASK` are cleared (interrupts
//! among them, so the entry cannot be interrupted before it is on a kernel stack).
//!
//! The entry below swaps GS to the per-CPU data (`swapgs`), moves to the current task's
//! kernel stack, and pushes the same frame an interrupt from user mode would have pushed.
//! From there it shares the path of interrupts and exceptions, including the way back
//! (always `iretq`, never `sysret`, see `exceptions`).
//!
//! Linux numbering applies: RAX holds the syscall number, RDI, RSI, RDX, R10, R8, R9 the
//! arguments, and RAX the result (`-errno` on failure).

use core::arch::naked_asm;

use super::cpu::{read_msr, write_msr};
use super::exceptions;
use super::gdt::{KERNEL_CODE, USER_CODE, USER_DATA, USER32_CODE};
use crate::cpu_local::{KERNEL_STACK_TOP_OFFSET, USER_RSP_SCRATCH_OFFSET};

const IA32_EFER: u32 = 0xC000_0080;
/// EFER bit 0: `syscall`/`sysret` enabled.
const EFER_SCE: u64 = 1 << 0;
const IA32_STAR: u32 = 0xC000_0081;
const IA32_LSTAR: u32 = 0xC000_0082;
const IA32_FMASK: u32 = 0xC000_0084;

/// Flags cleared on entry: TF (8), IF (9), DF (10), IOPL (12–13), NT (14), AC (18). As in
/// Linux: no single-stepping, no interrupts until we are on the kernel stack, string
/// instructions forward, and SMAP active (AC set would switch it off).
const ENTRY_FLAGS_CLEARED: u64 = 1 << 8 | 1 << 9 | 1 << 10 | 0b11 << 12 | 1 << 14 | 1 << 18;

/// The "vector" the syscall entry pushes into the frame, so the common handler can tell a
/// syscall from an interrupt (real vectors are 0–255).
pub(crate) const SYSCALL_VECTOR: u64 = 0x100;

/// Sets up `syscall` on this CPU. Call on every CPU, after its GDT is loaded.
pub(crate) fn init() {
    // STAR: bits 32–47 = kernel CS (SS is CS + 8); bits 48–63 = the base for `sysret`
    // (user CS = base + 16, user SS = base + 8), which is why USER32_CODE comes right before
    // USER_DATA and USER_CODE in the GDT. We return with `iretq`, but keep it right.
    assert_eq!(USER_DATA, ((USER32_CODE & !3) + 8) | 3);
    assert_eq!(USER_CODE, ((USER32_CODE & !3) + 16) | 3);
    let star = u64::from(USER32_CODE) << 48 | u64::from(KERNEL_CODE) << 32;
    // SAFETY: STAR, LSTAR and FMASK only affect the `syscall` instruction; LSTAR points to
    // the entry below, which is valid kernel code; enabling SCE allows `syscall` at all.
    unsafe {
        write_msr(IA32_STAR, star);
        write_msr(IA32_LSTAR, syscall_entry as *const () as u64);
        write_msr(IA32_FMASK, ENTRY_FLAGS_CLEARED);
        write_msr(IA32_EFER, read_msr(IA32_EFER) | EFER_SCE);
    }
}

/// Where `syscall` lands. Interrupts are off (FMASK), RSP is still the user's.
#[unsafe(naked)]
extern "C" fn syscall_entry() {
    naked_asm!(
        "swapgs",
        "mov gs:[{scratch}], rsp",
        "mov rsp, gs:[{stack}]",
        // The frame of an interrupt from user mode: SS, RSP, RFLAGS, CS, RIP, then the error
        // code and vector the entry stubs push.
        "push {user_data}",
        "push qword ptr gs:[{scratch}]",
        "push r11",
        "push {user_code}",
        "push rcx",
        "push 0",
        "push {vector}",
        "jmp {body}",
        scratch = const USER_RSP_SCRATCH_OFFSET,
        stack = const KERNEL_STACK_TOP_OFFSET,
        user_data = const USER_DATA as u64,
        user_code = const USER_CODE as u64,
        vector = const SYSCALL_VECTOR,
        body = sym exceptions::common_body,
    )
}
