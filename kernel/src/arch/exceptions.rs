// SPDX-License-Identifier: GPL-2.0-only

//! CPU exception handlers.
//!
//! How an exception reaches Rust code:
//!
//! 1. The CPU looks up the vector in the IDT, switches stacks if the gate says so (IST), and
//!    pushes SS, RSP, RFLAGS, CS and RIP of the interrupted code. For some exceptions it also
//!    pushes an error code.
//! 2. It jumps to the entry stub for that vector (generated below). The stub pushes a dummy
//!    error code when the CPU did not push one — so every frame has the same layout — and
//!    the vector number, then jumps to `common_entry`.
//! 3. `common_entry` saves all general-purpose registers and calls `handle_exception` with a
//!    pointer to everything pushed so far: an [`ExceptionFrame`].
//! 4. If the handler returns, `common_entry` restores the registers and `iretq` resumes the
//!    interrupted code.
//!
//! The stubs are "naked" functions: Rust emits no prologue or epilogue for them, only our
//! assembly. That is needed because on entry the stack holds the CPU's frame instead of a
//! normal function call.
//!
//! Almost every exception in the kernel is a bug, so the handler prints a readable report
//! and stops the machine. The exception is the breakpoint (`int3`), which just logs and
//! returns.

use core::arch::{asm, naked_asm};
use core::fmt::Write;

use log::info;

use crate::arch::halt_forever;
use crate::color::{BOLD, RED, RESET};
use crate::serial;

/// The stack contents when `handle_exception` is called, lowest address first:
/// the registers saved by `common_entry`, then what the stub and the CPU pushed.
#[repr(C)]
#[derive(Debug)]
pub struct ExceptionFrame {
    r15: u64,
    r14: u64,
    r13: u64,
    r12: u64,
    r11: u64,
    r10: u64,
    r9: u64,
    r8: u64,
    rbp: u64,
    rdi: u64,
    rsi: u64,
    rdx: u64,
    rcx: u64,
    rbx: u64,
    rax: u64,
    /// Pushed by the stub.
    vector: u64,
    /// Pushed by the CPU, or 0 pushed by the stub.
    error_code: u64,
    // Pushed by the CPU.
    rip: u64,
    cs: u64,
    rflags: u64,
    rsp: u64,
    ss: u64,
}

/// Defines one entry stub. `error_code` marks the vectors where the CPU pushes an error
/// code itself; for the others the stub pushes a 0 in its place.
macro_rules! entry_stub {
    ($name:ident, $vector:literal) => {
        #[unsafe(naked)]
        extern "C" fn $name() {
            naked_asm!(
                "push 0",
                "push {vector}",
                "jmp {common}",
                vector = const $vector,
                common = sym common_entry,
            )
        }
    };
    ($name:ident, $vector:literal, error_code) => {
        #[unsafe(naked)]
        extern "C" fn $name() {
            naked_asm!(
                "push {vector}",
                "jmp {common}",
                vector = const $vector,
                common = sym common_entry,
            )
        }
    };
}

entry_stub!(divide_error, 0);
entry_stub!(debug, 1);
entry_stub!(non_maskable_interrupt, 2);
entry_stub!(breakpoint, 3);
entry_stub!(overflow, 4);
entry_stub!(bound_range_exceeded, 5);
entry_stub!(invalid_opcode, 6);
entry_stub!(device_not_available, 7);
entry_stub!(double_fault, 8, error_code);
entry_stub!(coprocessor_segment_overrun, 9);
entry_stub!(invalid_tss, 10, error_code);
entry_stub!(segment_not_present, 11, error_code);
entry_stub!(stack_segment_fault, 12, error_code);
entry_stub!(general_protection, 13, error_code);
entry_stub!(page_fault, 14, error_code);
entry_stub!(reserved_15, 15);
entry_stub!(x87_floating_point, 16);
entry_stub!(alignment_check, 17, error_code);
entry_stub!(machine_check, 18);
entry_stub!(simd_floating_point, 19);
entry_stub!(virtualization, 20);
entry_stub!(control_protection, 21, error_code);
entry_stub!(reserved_22, 22);
entry_stub!(reserved_23, 23);
entry_stub!(reserved_24, 24);
entry_stub!(reserved_25, 25);
entry_stub!(reserved_26, 26);
entry_stub!(reserved_27, 27);
entry_stub!(hypervisor_injection, 28);
entry_stub!(vmm_communication, 29, error_code);
entry_stub!(security, 30, error_code);
entry_stub!(reserved_31, 31);

/// Entry stubs for vectors 0–31, in order; `idt::init` puts them into the IDT.
pub static ENTRY_POINTS: [extern "C" fn(); 32] = [
    divide_error,
    debug,
    non_maskable_interrupt,
    breakpoint,
    overflow,
    bound_range_exceeded,
    invalid_opcode,
    device_not_available,
    double_fault,
    coprocessor_segment_overrun,
    invalid_tss,
    segment_not_present,
    stack_segment_fault,
    general_protection,
    page_fault,
    reserved_15,
    x87_floating_point,
    alignment_check,
    machine_check,
    simd_floating_point,
    virtualization,
    control_protection,
    reserved_22,
    reserved_23,
    reserved_24,
    reserved_25,
    reserved_26,
    reserved_27,
    hypervisor_injection,
    vmm_communication,
    security,
    reserved_31,
];

/// Saves the registers, calls the Rust handler, restores the registers and returns to the
/// interrupted code.
///
/// Stack alignment: the CPU aligns RSP to 16 bytes before pushing its frame. The frame
/// (5 qwords), error code, vector and 15 registers make 22 qwords = 176 bytes, a multiple of
/// 16, so RSP is aligned at the `call` as the System V ABI requires.
#[unsafe(naked)]
extern "C" fn common_entry() {
    naked_asm!(
        "push rax",
        "push rbx",
        "push rcx",
        "push rdx",
        "push rsi",
        "push rdi",
        "push rbp",
        "push r8",
        "push r9",
        "push r10",
        "push r11",
        "push r12",
        "push r13",
        "push r14",
        "push r15",
        // First argument (RDI) = pointer to the frame we just built.
        "mov rdi, rsp",
        // The ABI expects the direction flag clear; the interrupted code may have set it.
        "cld",
        "call {handler}",
        "pop r15",
        "pop r14",
        "pop r13",
        "pop r12",
        "pop r11",
        "pop r10",
        "pop r9",
        "pop r8",
        "pop rbp",
        "pop rdi",
        "pop rsi",
        "pop rdx",
        "pop rcx",
        "pop rbx",
        "pop rax",
        // Drop the vector and the error code.
        "add rsp, 16",
        "iretq",
        handler = sym handle_exception,
    )
}

const BREAKPOINT: u64 = 3;
const DOUBLE_FAULT: u64 = 8;
const PAGE_FAULT: u64 = 14;

extern "C" fn handle_exception(frame: &ExceptionFrame) {
    if frame.vector == BREAKPOINT {
        // `int3` is only ever placed on purpose, never inside the logger, so taking the
        // COM1 lock here cannot deadlock. RIP already points past the 1-byte instruction.
        info!("breakpoint (int3) at {:#x}, continuing", frame.rip - 1);
        return;
    }
    report(frame);
    halt_forever();
}

/// Prints a readable description of a fatal exception.
fn report(frame: &ExceptionFrame) {
    // Not `println!`: the exception may have hit while the COM1 lock was held.
    let mut out = serial::emergency_writer();
    let (name, mnemonic) = name(frame.vector);
    // Writing to the UART cannot fail; the `fmt::Result`s carry no information.
    let _ = writeln!(
        out,
        "\n{RED}{BOLD}CPU EXCEPTION: {name} (#{mnemonic}, vector {}){RESET}",
        frame.vector
    );

    match frame.vector {
        PAGE_FAULT => {
            let _ = writeln!(out, "  {}", PageFaultCause(frame.error_code));
            let _ = writeln!(out, "  address  {:#018x}", read_cr2());
        }
        DOUBLE_FAULT => {
            let _ = writeln!(
                out,
                "  an exception happened while the CPU was delivering another one;\n  \
                 most often the kernel stack overflowed"
            );
        }
        _ => {}
    }

    let f = frame;
    let _ = writeln!(out, "  error    {:#x}", f.error_code);
    let _ = writeln!(
        out,
        "  rip {:#018x}  cs {:#06x}  rflags {:#010x}",
        f.rip, f.cs, f.rflags
    );
    let _ = writeln!(out, "  rsp {:#018x}  ss {:#06x}", f.rsp, f.ss);
    let _ = writeln!(
        out,
        "  rax {:#018x}  rbx {:#018x}  rcx {:#018x}",
        f.rax, f.rbx, f.rcx
    );
    let _ = writeln!(
        out,
        "  rdx {:#018x}  rsi {:#018x}  rdi {:#018x}",
        f.rdx, f.rsi, f.rdi
    );
    let _ = writeln!(
        out,
        "  rbp {:#018x}  r8  {:#018x}  r9  {:#018x}",
        f.rbp, f.r8, f.r9
    );
    let _ = writeln!(
        out,
        "  r10 {:#018x}  r11 {:#018x}  r12 {:#018x}",
        f.r10, f.r11, f.r12
    );
    let _ = writeln!(
        out,
        "  r13 {:#018x}  r14 {:#018x}  r15 {:#018x}",
        f.r13, f.r14, f.r15
    );
    let _ = writeln!(out, "{RED}system halted{RESET}");
}

/// Human-readable name and mnemonic of an exception vector (Intel SDM vol. 3, table 6-1).
fn name(vector: u64) -> (&'static str, &'static str) {
    match vector {
        0 => ("Divide error", "DE"),
        1 => ("Debug", "DB"),
        2 => ("Non-maskable interrupt", "NMI"),
        3 => ("Breakpoint", "BP"),
        4 => ("Overflow", "OF"),
        5 => ("BOUND range exceeded", "BR"),
        6 => ("Invalid opcode", "UD"),
        7 => ("Device not available", "NM"),
        8 => ("Double fault", "DF"),
        9 => ("Coprocessor segment overrun", "MF"),
        10 => ("Invalid TSS", "TS"),
        11 => ("Segment not present", "NP"),
        12 => ("Stack-segment fault", "SS"),
        13 => ("General protection fault", "GP"),
        14 => ("Page fault", "PF"),
        16 => ("x87 floating-point error", "MF"),
        17 => ("Alignment check", "AC"),
        18 => ("Machine check", "MC"),
        19 => ("SIMD floating-point exception", "XM"),
        20 => ("Virtualization exception", "VE"),
        21 => ("Control protection exception", "CP"),
        28 => ("Hypervisor injection exception", "HV"),
        29 => ("VMM communication exception", "VC"),
        30 => ("Security exception", "SX"),
        _ => ("Reserved", "?"),
    }
}

/// Decodes the page fault error code (Intel SDM vol. 3, "Interrupt 14").
struct PageFaultCause(u64);

impl core::fmt::Display for PageFaultCause {
    fn fmt(&self, f: &mut core::fmt::Formatter) -> core::fmt::Result {
        let bit = |n: u32| self.0 & (1 << n) != 0;
        let access = if bit(4) {
            "instruction fetch from"
        } else if bit(1) {
            "write to"
        } else {
            "read from"
        };
        let page = if bit(0) {
            "a page without the needed permission"
        } else {
            "a non-present page"
        };
        let mode = if bit(2) { "user" } else { "kernel" };
        write!(f, "{access} {page}, in {mode} mode")?;
        if bit(3) {
            write!(f, " (reserved bit set in a page table entry)")?;
        }
        if bit(5) {
            write!(f, " (protection key)")?;
        }
        if bit(6) {
            write!(f, " (shadow stack)")?;
        }
        Ok(())
    }
}

/// CR2 holds the address that caused the last page fault.
fn read_cr2() -> u64 {
    let value;
    // SAFETY: reading CR2 has no side effects and touches no memory.
    unsafe { asm!("mov {}, cr2", out(reg) value, options(nomem, nostack, preserves_flags)) };
    value
}

/// Triggers a breakpoint exception, to check that the IDT works.
pub fn breakpoint_self_test() {
    // SAFETY: `int3` only raises vector 3, whose handler logs and returns.
    unsafe { asm!("int3", options(nomem, nostack)) };
}
