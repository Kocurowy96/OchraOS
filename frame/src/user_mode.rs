// SPDX-License-Identifier: GPL-2.0-only

//! User mode (ring 3): entering it, coming back from it, and touching user memory.
//!
//! The kernel crate decides what a syscall does and what happens to a program that faults;
//! it registers two handlers ([`set_syscall_handler`], [`set_fault_handler`]) and sees the
//! user's registers only through [`UserContext`], which cannot be turned into something that
//! would crash the kernel on the way back (e.g. a kernel code selector).
//!
//! User memory is reached only through [`copy_from_user`] and [`copy_to_user`]. They check
//! that the whole range lies in the user half, open SMAP for the copy alone (`stac`/`clac`:
//! with SMAP on, the kernel faults on any user page otherwise), and survive bad pointers: if
//! the copy faults (an unmapped or read-only page), the page fault handler finds the
//! faulting instruction in the fixup table and continues at the copy's error path, so the
//! syscall returns `EFAULT` instead of crashing the kernel.

use core::arch::asm;
use core::fmt;
use core::sync::atomic::Ordering;

use spin::Once;

use crate::arch::cpu::SMAP_ENABLED;
use crate::arch::exceptions::{self, ExceptionFrame};
use crate::arch::gdt::{USER_CODE, USER_DATA};
use crate::arch::interrupts;
use crate::arch::syscall::SYSCALL_VECTOR;
use crate::mm::user::USER_END;

/// The user's registers at a syscall or fault, as the kernel crate may see and change them.
#[repr(transparent)]
pub struct UserContext(ExceptionFrame);

impl UserContext {
    /// The syscall number (RAX).
    pub fn syscall_number(&self) -> u64 {
        self.0.rax
    }

    /// Syscall argument `index` (0–5: RDI, RSI, RDX, R10, R8, R9).
    pub fn argument(&self, index: usize) -> u64 {
        match index {
            0 => self.0.rdi,
            1 => self.0.rsi,
            2 => self.0.rdx,
            3 => self.0.r10,
            4 => self.0.r8,
            5 => self.0.r9,
            _ => panic!("syscalls have six arguments, not {}", index + 1),
        }
    }

    /// Sets the syscall result (RAX): a value, or `-errno` as `u64`.
    pub fn set_result(&mut self, value: u64) {
        self.0.rax = value;
    }

    pub fn instruction_pointer(&self) -> u64 {
        self.0.rip
    }

    pub fn stack_pointer(&self) -> u64 {
        self.0.rsp
    }

    /// Where the program continues. An address outside the user half is caught before
    /// returning (the program then faults instead).
    pub fn set_instruction_pointer(&mut self, address: u64) {
        self.0.rip = address;
    }

    pub fn set_stack_pointer(&mut self, address: u64) {
        self.0.rsp = address;
    }

    /// # Safety
    ///
    /// `frame` must describe user mode.
    unsafe fn from_frame(frame: &mut ExceptionFrame) -> &mut Self {
        // SAFETY: `UserContext` is a `repr(transparent)` wrapper of `ExceptionFrame`.
        unsafe { &mut *(frame as *mut ExceptionFrame).cast::<Self>() }
    }
}

/// A CPU exception raised by a user program.
#[derive(Clone, Copy, Debug)]
pub struct UserFault {
    pub vector: u8,
    pub error_code: u64,
    /// For a page fault: the address the program tried to access.
    pub address: Option<u64>,
}

impl fmt::Display for UserFault {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.write_str(exceptions::name(u64::from(self.vector)).0)?;
        if let Some(address) = self.address {
            write!(f, " at {address:#x}")?;
        }
        Ok(())
    }
}

/// What the kernel does for a syscall.
pub type SyscallHandler = fn(&mut UserContext);
/// What the kernel does when a user program faults. It must change the context (e.g. to run
/// a signal handler) or never return (end the task).
pub type FaultHandler = fn(&mut UserContext, UserFault);

static SYSCALL_HANDLER: Once<SyscallHandler> = Once::new();
static FAULT_HANDLER: Once<FaultHandler> = Once::new();

pub fn set_syscall_handler(handler: SyscallHandler) {
    SYSCALL_HANDLER.call_once(|| handler);
}

pub fn set_fault_handler(handler: FaultHandler) {
    FAULT_HANDLER.call_once(|| handler);
}

/// Handles a syscall, interrupt or exception that came from user mode, then checks that the
/// frame is fit to return to user mode. Interrupts are off on entry and on return.
pub(crate) fn handle(frame: &mut ExceptionFrame) {
    match frame.vector {
        SYSCALL_VECTOR => {
            let handler = *SYSCALL_HANDLER
                .get()
                .expect("no syscall handler registered");
            interrupts::enable();
            // SAFETY: the frame came from user mode.
            handler(unsafe { UserContext::from_frame(frame) });
            interrupts::disable();
        }
        vector if vector >= u64::from(interrupts::FIRST_VECTOR) => interrupts::dispatch(frame),
        vector => {
            let fault = UserFault {
                vector: vector as u8,
                error_code: frame.error_code,
                address: (vector == exceptions::PAGE_FAULT).then(exceptions::read_cr2),
            };
            report_fault(frame, fault);
        }
    }
    // `iretq` to an address in the kernel half would fault in ring 0; treat it as the
    // program's own general protection fault instead.
    while frame.rip >= USER_END {
        let fault = UserFault {
            vector: exceptions::GENERAL_PROTECTION as u8,
            error_code: 0,
            address: None,
        };
        report_fault(frame, fault);
    }
}

fn report_fault(frame: &mut ExceptionFrame, fault: UserFault) {
    let handler = *FAULT_HANDLER.get().expect("no fault handler registered");
    interrupts::enable();
    // SAFETY: the frame came from user mode.
    handler(unsafe { UserContext::from_frame(frame) }, fault);
    interrupts::disable();
}

/// Starts executing user code at `entry` with the stack pointer `stack`, in the current
/// task's address space. Never returns: the task continues in user mode.
pub fn enter(entry: u64, stack: u64) -> ! {
    interrupts::disable();
    let mut frame = ExceptionFrame::zeroed();
    frame.rip = entry;
    frame.cs = u64::from(USER_CODE);
    // IF (bit 9) on, plus bit 1, which is always set.
    frame.rflags = 1 << 9 | 1 << 1;
    frame.rsp = stack;
    frame.ss = u64::from(USER_DATA);
    // Check the address now, as `handle` does for every return to user mode.
    while frame.rip >= USER_END {
        let fault = UserFault {
            vector: exceptions::GENERAL_PROTECTION as u8,
            error_code: 0,
            address: None,
        };
        report_fault(&mut frame, fault);
    }
    // SAFETY: the frame describes user mode (user selectors, RIP in the user half), lives on
    // this task's kernel stack, and interrupts are off.
    unsafe { exceptions::return_to_user(&frame) }
}

/// A user pointer that is outside the user half or points to memory that is not mapped (or
/// not writable, for a write).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BadAddress;

/// Copies are done in pieces of this size, each with interrupts off (see `copy`).
const CHUNK: usize = 4096;

/// Copies `destination.len()` bytes from user address `source`.
pub fn copy_from_user(destination: &mut [u8], source: u64) -> Result<(), BadAddress> {
    check_range(source, destination.len())?;
    for (index, chunk) in destination.chunks_mut(CHUNK).enumerate() {
        let from = source + (index * CHUNK) as u64;
        // SAFETY: `from..from + len` is in the user half (checked above), so a fault there is
        // a page fault that the fixup table turns into an error; `chunk` is valid kernel
        // memory for writes.
        unsafe { copy(chunk.as_mut_ptr(), from as *const u8, chunk.len()) }?;
    }
    Ok(())
}

/// Copies `source` to user address `destination`.
pub fn copy_to_user(destination: u64, source: &[u8]) -> Result<(), BadAddress> {
    check_range(destination, source.len())?;
    for (index, chunk) in source.chunks(CHUNK).enumerate() {
        let to = destination + (index * CHUNK) as u64;
        // SAFETY: as in `copy_from_user`, with the roles swapped.
        unsafe { copy(to as *mut u8, chunk.as_ptr(), chunk.len()) }?;
    }
    Ok(())
}

fn check_range(address: u64, length: usize) -> Result<(), BadAddress> {
    match address.checked_add(length as u64) {
        Some(end) if end <= USER_END => Ok(()),
        _ => Err(BadAddress),
    }
}

/// Copies `length` bytes with `rep movsb`; a page fault on the way ends the copy with an
/// error. Interrupts are off meanwhile: an interrupt does not clear the AC flag that `stac`
/// sets, so its handler would run with SMAP open.
///
/// # Safety
///
/// One side must be valid kernel memory for the access; the other must lie in the user half.
unsafe fn copy(destination: *mut u8, source: *const u8, length: usize) -> Result<(), BadAddress> {
    let were_enabled = interrupts::disable();
    let failed: u32;
    if SMAP_ENABLED.load(Ordering::Relaxed) {
        // SAFETY: guaranteed by the caller; a fault at label 2 continues at label 3 (fixup
        // table), which closes SMAP again. `stac`/`clac` exist because SMAP is on.
        unsafe {
            asm!(
                "stac",
                "2:",
                "rep movsb",
                "clac",
                "xor {failed:e}, {failed:e}",
                "jmp 4f",
                "3:",
                "clac",
                "mov {failed:e}, 1",
                "4:",
                ".pushsection .ochra_fixup, \"a\"",
                ".balign 4",
                ".long 2b - .",
                ".long 3b - .",
                ".popsection",
                failed = out(reg) failed,
                inout("rcx") length => _,
                inout("rsi") source => _,
                inout("rdi") destination => _,
                options(nostack),
            );
        }
    } else {
        // SAFETY: as above, without SMAP to open.
        unsafe {
            asm!(
                "2:",
                "rep movsb",
                "xor {failed:e}, {failed:e}",
                "jmp 4f",
                "3:",
                "mov {failed:e}, 1",
                "4:",
                ".pushsection .ochra_fixup, \"a\"",
                ".balign 4",
                ".long 2b - .",
                ".long 3b - .",
                ".popsection",
                failed = out(reg) failed,
                inout("rcx") length => _,
                inout("rsi") source => _,
                inout("rdi") destination => _,
                options(nostack),
            );
        }
    }
    if were_enabled {
        interrupts::enable();
    }
    if failed == 0 { Ok(()) } else { Err(BadAddress) }
}

/// One entry of the fixup table: two offsets, each relative to its own field.
#[repr(C)]
struct Fixup {
    instruction: i32,
    continue_at: i32,
}

/// For a page fault in kernel mode: if it happened in a user memory copy, makes the frame
/// continue at the copy's error path and returns `true`.
pub(crate) fn fix_up(frame: &mut ExceptionFrame) -> bool {
    unsafe extern "C" {
        // SAFETY: defined by `linker.ld` around the `.ochra_fixup` section; only their
        // addresses are used.
        safe static __ochra_fixup_start: u8;
        safe static __ochra_fixup_end: u8;
    }
    let start = &raw const __ochra_fixup_start as usize;
    let end = &raw const __ochra_fixup_end as usize;
    let count = (end - start) / size_of::<Fixup>();
    // SAFETY: the section holds `count` `Fixup` entries (4-byte aligned, written by the
    // `asm!` blocks above) and is read-only data for the kernel's whole life.
    let table = unsafe { core::slice::from_raw_parts(start as *const Fixup, count) };
    for entry in table {
        let field = |offset: &i32| (offset as *const i32 as i64 + i64::from(*offset)) as u64;
        if field(&entry.instruction) == frame.rip {
            frame.rip = field(&entry.continue_at);
            return true;
        }
    }
    false
}
