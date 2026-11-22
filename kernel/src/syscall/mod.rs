// SPDX-License-Identifier: GPL-2.0-only

//! System calls, with Linux numbers and Linux behaviour (see `man 2 <name>`).
//!
//! `ochra_frame::user_mode` brings every `syscall` here with the user's registers; the number
//! picks the function, and the result goes back in RAX: a value, or `-errno` on failure.
//! The functions are grouped by topic: files, memory and processes.

mod files;
mod memory;
mod process;

use alloc::sync::Arc;
use alloc::vec::Vec;

use log::{debug, info};
use ochra_frame::task;
use ochra_frame::user_mode::{self, UserContext, UserFault, copy_from_user, copy_to_user};

use crate::errno::{EFAULT, ENOSYS, Errno};
use crate::process::{Exit, Process};

/// Linux x86-64 syscall numbers (`arch/x86/entry/syscalls/syscall_64.tbl`).
mod number {
    pub const READ: u64 = 0;
    pub const WRITE: u64 = 1;
    pub const CLOSE: u64 = 3;
    pub const MMAP: u64 = 9;
    pub const MPROTECT: u64 = 10;
    pub const MUNMAP: u64 = 11;
    pub const BRK: u64 = 12;
    pub const IOCTL: u64 = 16;
    pub const READV: u64 = 19;
    pub const WRITEV: u64 = 20;
    pub const PIPE: u64 = 22;
    pub const SCHED_YIELD: u64 = 24;
    pub const DUP: u64 = 32;
    pub const DUP2: u64 = 33;
    pub const GETPID: u64 = 39;
    pub const CLONE: u64 = 56;
    pub const FORK: u64 = 57;
    pub const VFORK: u64 = 58;
    pub const EXECVE: u64 = 59;
    pub const EXIT: u64 = 60;
    pub const WAIT4: u64 = 61;
    pub const FCNTL: u64 = 72;
    pub const GETPPID: u64 = 110;
    pub const GETPGRP: u64 = 111;
    pub const ARCH_PRCTL: u64 = 158;
    pub const GETTID: u64 = 186;
    pub const SET_TID_ADDRESS: u64 = 218;
    pub const EXIT_GROUP: u64 = 231;
    pub const DUP3: u64 = 292;
    pub const PIPE2: u64 = 293;
    pub const GETRANDOM: u64 = 318;
}

/// Registers the handlers with `ochra-frame`. Call once, before any user task runs.
pub fn init() {
    user_mode::set_syscall_handler(dispatch);
    user_mode::set_fault_handler(on_fault);
}

type SyscallResult = Result<u64, Errno>;

fn dispatch(context: &mut UserContext) {
    let number = context.syscall_number();
    let a = |index| context.argument(index);
    let (a0, a1, a2, a3, a5) = (a(0), a(1), a(2), a(3), a(5));
    let result = match number {
        number::READ => files::read(a0, a1, a2),
        number::WRITE => files::write(a0, a1, a2),
        number::READV => files::readv(a0, a1, a2),
        number::WRITEV => files::writev(a0, a1, a2),
        number::CLOSE => files::close(a0),
        number::IOCTL => files::ioctl(a0, a1, a2),
        number::PIPE => files::pipe2(a0, 0),
        number::PIPE2 => files::pipe2(a0, a1),
        number::DUP => files::dup(a0),
        number::DUP2 => files::dup2(a0, a1),
        number::DUP3 => files::dup3(a0, a1, a2),
        number::FCNTL => files::fcntl(a0, a1, a2),
        number::BRK => memory::brk(a0),
        number::MMAP => memory::mmap(a0, a1, a2, a3, a5),
        number::MUNMAP => memory::munmap(a0, a1),
        number::MPROTECT => memory::mprotect(a0, a1, a2),
        number::FORK | number::VFORK => process::fork(context),
        number::CLONE => process::clone(context, a0, a1),
        number::EXECVE => process::execve(context, a0, a1, a2),
        number::EXIT | number::EXIT_GROUP => process::exit(a0 as i32),
        number::WAIT4 => process::wait4(a0 as i64, a1, a2, a3),
        number::GETPID => process::getpid(),
        number::GETTID => process::getpid(),
        number::GETPPID => process::getppid(),
        number::GETPGRP => process::getpgrp(),
        number::SCHED_YIELD => process::sched_yield(),
        number::ARCH_PRCTL => process::arch_prctl(a0, a1),
        number::SET_TID_ADDRESS => process::getpid(),
        number::GETRANDOM => process::getrandom(a0, a1, a2),
        _ => {
            debug!("syscall {number} is not implemented");
            Err(ENOSYS)
        }
    };
    // `execve` replaced the registers with the new program's; nothing to return there.
    if number == number::EXECVE && result.is_ok() {
        return;
    }
    context.set_result(match result {
        Ok(value) => value,
        Err(error) => (-error) as u64,
    });
}

/// The calling process. Every syscall comes from a user task; only the built-in ring 3
/// self-tests run without a process, and they make no syscalls that need one.
fn current() -> Result<Arc<Process>, Errno> {
    crate::process::current().ok_or(ENOSYS)
}

/// `copy_from_user`, but a stack page that is not there yet is added first (the stack
/// grows on demand, also when the kernel is the one touching it).
fn read_user(buffer: &mut [u8], address: u64) -> Result<(), Errno> {
    copy_from_user(buffer, address)
        .or_else(|_| {
            grow_stack(address)?;
            copy_from_user(buffer, address)
        })
        .map_err(|_| EFAULT)
}

fn write_user(address: u64, data: &[u8]) -> Result<(), Errno> {
    copy_to_user(address, data)
        .or_else(|_| {
            grow_stack(address)?;
            copy_to_user(address, data)
        })
        .map_err(|_| EFAULT)
}

fn read_u64(address: u64) -> Result<u64, Errno> {
    let mut bytes = [0; 8];
    read_user(&mut bytes, address)?;
    Ok(u64::from_le_bytes(bytes))
}

/// Reads a NUL-terminated string of at most `limit` bytes (without the NUL); `Err(None)`
/// if it is longer, `Err(Some(errno))` for a bad pointer.
fn read_string(address: u64, limit: usize) -> Result<Vec<u8>, Option<Errno>> {
    let mut text = Vec::new();
    let mut cursor = address;
    loop {
        // Up to the end of the page, so a string at the end of memory is still readable.
        let in_page = (4096 - (cursor % 4096)) as usize;
        let mut chunk = alloc::vec![0; in_page.min(limit + 1 - text.len())];
        read_user(&mut chunk, cursor).map_err(Some)?;
        if let Some(end) = chunk.iter().position(|&byte| byte == 0) {
            text.extend_from_slice(&chunk[..end]);
            return Ok(text);
        }
        text.extend_from_slice(&chunk);
        if text.len() > limit {
            return Err(None);
        }
        cursor += chunk.len() as u64;
    }
}

fn grow_stack(address: u64) -> Result<(), user_mode::BadAddress> {
    let process = crate::process::current().ok_or(user_mode::BadAddress)?;
    let space = process.space().map_err(|_| user_mode::BadAddress)?;
    let grown = process.memory.lock().grow_stack(&space, address);
    grown.then_some(()).ok_or(user_mode::BadAddress)
}

/// A user program raised an exception. A page fault just below the stack grows the stack
/// (the program then retries); anything else kills the program with the signal Linux would
/// send. (Signal handlers come in P4.)
fn on_fault(_context: &mut UserContext, fault: UserFault) {
    const PAGE_FAULT: u8 = 14;
    /// Page fault error code bit 0: the page was present (a protection fault).
    const PRESENT: u64 = 1;
    if fault.vector == PAGE_FAULT
        && fault.error_code & PRESENT == 0
        && let Some(address) = fault.address
        && grow_stack(address).is_ok()
    {
        return;
    }
    let exit = Exit::Signal(signal_for(fault.vector));
    match crate::process::current() {
        Some(process) => {
            info!("process {} ({}): {fault}", process.id, process.name());
            crate::process::exit(&process, exit)
        }
        None => {
            let name = task::current().map_or("?", |task| task.name());
            info!("user task {name} killed: {fault}");
            crate::sched::exit_current()
        }
    }
}

/// The signal Linux sends for a CPU exception in user mode.
fn signal_for(vector: u8) -> u8 {
    const SIGILL: u8 = 4;
    const SIGTRAP: u8 = 5;
    const SIGBUS: u8 = 7;
    const SIGFPE: u8 = 8;
    const SIGSEGV: u8 = 11;
    match vector {
        0 | 16 | 19 => SIGFPE, // divide error, x87 and SIMD floating point
        1 | 3 => SIGTRAP,      // debug, breakpoint
        6 => SIGILL,           // invalid opcode
        17 => SIGBUS,          // alignment check
        _ => SIGSEGV,          // page fault, general protection and the rest
    }
}
