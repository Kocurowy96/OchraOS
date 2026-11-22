// SPDX-License-Identifier: GPL-2.0-only

//! System calls, with Linux numbers and Linux behaviour (see `man 2 <name>`).
//!
//! `ochra_frame::user_mode` brings every `syscall` here with the user's registers; the number
//! picks the function, and the result goes back in RAX: a value, or `-errno` on failure.
//!
//! File descriptors do not exist yet (stage 2, P3): 0, 1 and 2 are the console.

use alloc::sync::Arc;
use alloc::vec;
use alloc::vec::Vec;

use log::{debug, info};
use ochra_frame::user_mode::{self, UserContext, UserFault, copy_from_user, copy_to_user};
use ochra_frame::{print, random, task};

use crate::errno::{EBADF, EFAULT, EINVAL, ENOSYS, ENOTTY, EPERM, Errno};
use crate::process::{self, Exit, Process};

/// Linux x86-64 syscall numbers (`arch/x86/entry/syscalls/syscall_64.tbl`).
mod number {
    pub const WRITE: u64 = 1;
    pub const MMAP: u64 = 9;
    pub const MPROTECT: u64 = 10;
    pub const MUNMAP: u64 = 11;
    pub const BRK: u64 = 12;
    pub const IOCTL: u64 = 16;
    pub const WRITEV: u64 = 20;
    pub const EXIT: u64 = 60;
    pub const ARCH_PRCTL: u64 = 158;
    pub const SET_TID_ADDRESS: u64 = 218;
    pub const EXIT_GROUP: u64 = 231;
    pub const GETRANDOM: u64 = 318;
}

/// Most bytes one `write` to the console copies; the caller sees a short write and retries
/// with the rest, as POSIX allows.
const MAX_WRITE: usize = 64 * 1024;

/// Registers the handlers with `ochra-frame`. Call once, before any user task runs.
pub fn init() {
    user_mode::set_syscall_handler(dispatch);
    user_mode::set_fault_handler(on_fault);
}

type SyscallResult = Result<u64, Errno>;

fn dispatch(context: &mut UserContext) {
    let number = context.syscall_number();
    let a = |index| context.argument(index);
    let result = match number {
        number::WRITE => write(a(0), a(1), a(2)),
        number::WRITEV => writev(a(0), a(1), a(2)),
        number::IOCTL => ioctl(a(0), a(1), a(2)),
        number::BRK => brk(a(0)),
        number::MMAP => mmap(a(0), a(1), a(2), a(3), a(4), a(5)),
        number::MUNMAP => munmap(a(0), a(1)),
        number::MPROTECT => mprotect(a(0), a(1), a(2)),
        number::ARCH_PRCTL => arch_prctl(a(0), a(1)),
        number::SET_TID_ADDRESS => set_tid_address(),
        number::GETRANDOM => getrandom(a(0), a(1), a(2)),
        number::EXIT | number::EXIT_GROUP => exit(a(0) as i32),
        _ => {
            debug!("syscall {number} is not implemented");
            Err(ENOSYS)
        }
    };
    context.set_result(match result {
        Ok(value) => value,
        Err(error) => (-error) as u64,
    });
}

/// The calling process. Every syscall comes from a user task; only the built-in ring 3
/// self-tests run without a process, and they make no syscalls that need one.
fn current() -> Result<Arc<Process>, Errno> {
    process::current().ok_or(ENOSYS)
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

fn grow_stack(address: u64) -> Result<(), user_mode::BadAddress> {
    let process = process::current().ok_or(user_mode::BadAddress)?;
    let grown = process.memory.lock().grow_stack(&process.space, address);
    grown.then_some(()).ok_or(user_mode::BadAddress)
}

/// Where bytes written to `fd` go.
fn output(fd: u64, bytes: &[u8]) -> Result<(), Errno> {
    if fd > 2 {
        return Err(EBADF);
    }
    if fd == 1
        && let Some(process) = process::current()
        && let Some(captured) = &process.captured_output
    {
        captured.lock().extend_from_slice(bytes);
        return Ok(());
    }
    // The console takes text; invalid UTF-8 shows as replacement characters.
    print!("{}", alloc::string::String::from_utf8_lossy(bytes));
    Ok(())
}

/// `write(fd, buf, count)`.
fn write(fd: u64, buffer: u64, count: u64) -> SyscallResult {
    if fd > 2 {
        return Err(EBADF);
    }
    let length = (count as usize).min(MAX_WRITE);
    let mut bytes = vec![0; length];
    read_user(&mut bytes, buffer)?;
    output(fd, &bytes)?;
    Ok(length as u64)
}

/// `writev(fd, iov, iovcnt)`: writes several buffers as one.
fn writev(fd: u64, vector: u64, count: u64) -> SyscallResult {
    /// Linux's `UIO_MAXIOV`.
    const MAX_PIECES: u64 = 1024;
    if fd > 2 {
        return Err(EBADF);
    }
    if count > MAX_PIECES {
        return Err(EINVAL);
    }
    // `struct iovec { void *iov_base; size_t iov_len; }`
    let mut table = vec![0; count as usize * 16];
    read_user(&mut table, vector)?;
    let pieces: Vec<(u64, u64)> = table
        .as_chunks::<16>()
        .0
        .iter()
        .map(|entry| {
            let (base, length) = entry.split_at(8);
            let word = |bytes: &[u8]| u64::from_le_bytes(bytes.try_into().expect("8 bytes"));
            (word(base), word(length))
        })
        .collect();
    if pieces.iter().any(|&(_, length)| length > i64::MAX as u64) {
        return Err(EINVAL);
    }
    let mut bytes = Vec::new();
    for (address, length) in pieces {
        let length = (length as usize).min(MAX_WRITE - bytes.len());
        let start = bytes.len();
        bytes.resize(start + length, 0);
        if read_user(&mut bytes[start..], address).is_err() {
            // What was gathered before the bad buffer is still written, as in Linux.
            bytes.truncate(start);
            if bytes.is_empty() {
                return Err(EFAULT);
            }
            break;
        }
        if bytes.len() == MAX_WRITE {
            break;
        }
    }
    output(fd, &bytes)?;
    Ok(bytes.len() as u64)
}

/// `ioctl(fd, request, argp)`: only what the C library asks the console about.
fn ioctl(fd: u64, request: u64, argument: u64) -> SyscallResult {
    /// Get the window size (`struct winsize`).
    const TIOCGWINSZ: u64 = 0x5413;
    if fd > 2 {
        return Err(EBADF);
    }
    // Captured output is not a terminal (like a pipe on Linux).
    let captured = process::current().is_some_and(|process| process.captured_output.is_some());
    if fd == 1 && captured {
        return Err(ENOTTY);
    }
    match request {
        TIOCGWINSZ => {
            // The serial console has no size; 24 rows of 80 columns, the classic terminal.
            let rows: u16 = 24;
            let columns: u16 = 80;
            let mut size = [0; 8];
            size[..2].copy_from_slice(&rows.to_le_bytes());
            size[2..4].copy_from_slice(&columns.to_le_bytes());
            write_user(argument, &size)?;
            Ok(0)
        }
        _ => {
            debug!("ioctl {request:#x} on the console is not implemented");
            Err(ENOTTY)
        }
    }
}

/// `brk(addr)`: returns the new program break, or the old one if it could not move.
fn brk(address: u64) -> SyscallResult {
    let process = current()?;
    let result = process.memory.lock().brk(&process.space, address);
    Ok(result)
}

fn mmap(
    address: u64,
    length: u64,
    protection: u64,
    flags: u64,
    _fd: u64,
    offset: u64,
) -> SyscallResult {
    let process = current()?;
    process
        .memory
        .lock()
        .mmap(&process.space, address, length, protection, flags, offset)
}

fn munmap(address: u64, length: u64) -> SyscallResult {
    let process = current()?;
    let result = process
        .memory
        .lock()
        .munmap(&process.space, address, length);
    result.map(|()| 0)
}

fn mprotect(address: u64, length: u64, protection: u64) -> SyscallResult {
    let process = current()?;
    let result = process
        .memory
        .lock()
        .mprotect(&process.space, address, length, protection);
    result.map(|()| 0)
}

/// `arch_prctl(code, addr)`: the thread pointer, which the C library sets up for
/// thread-local storage (TLS) and reads through the FS segment.
fn arch_prctl(code: u64, address: u64) -> SyscallResult {
    const ARCH_SET_FS: u64 = 0x1002;
    const ARCH_GET_FS: u64 = 0x1003;
    match code {
        ARCH_SET_FS => {
            task::set_fs_base(address).map_err(|_| EPERM)?;
            Ok(0)
        }
        ARCH_GET_FS => {
            let task = task::current().ok_or(EINVAL)?;
            write_user(address, &task.fs_base().to_le_bytes())?;
            Ok(0)
        }
        _ => {
            // The GS base and the CPUID and shadow stack controls come when needed.
            debug!("arch_prctl {code:#x} is not implemented");
            Err(EINVAL)
        }
    }
}

/// `set_tid_address(tidptr)`: returns the thread ID. The pointer matters only once a
/// process has several threads (the kernel clears it when the thread exits), so it is not
/// kept yet.
fn set_tid_address() -> SyscallResult {
    Ok(u64::from(current()?.id))
}

/// `getrandom(buf, buflen, flags)`: bytes from the kernel's ChaCha20 generator, which is
/// seeded at boot, so it never blocks.
fn getrandom(buffer: u64, length: u64, flags: u64) -> SyscallResult {
    const GRND_NONBLOCK: u64 = 1;
    const GRND_RANDOM: u64 = 2;
    const GRND_INSECURE: u64 = 4;
    if flags & !(GRND_NONBLOCK | GRND_RANDOM | GRND_INSECURE) != 0
        || flags & (GRND_RANDOM | GRND_INSECURE) == GRND_RANDOM | GRND_INSECURE
    {
        return Err(EINVAL);
    }
    let length = length.min(i32::MAX as u64);
    let mut done = 0;
    let mut chunk = [0; 256];
    while done < length {
        let size = (length - done).min(chunk.len() as u64) as usize;
        random::fill(&mut chunk[..size]);
        let written = write_user(buffer + done, &chunk[..size]);
        chunk.fill(0);
        if written.is_err() {
            // Linux reports the bytes already written, or EFAULT if there are none.
            return if done == 0 { Err(EFAULT) } else { Ok(done) };
        }
        done += size as u64;
    }
    Ok(done)
}

/// `exit(status)` / `exit_group(status)`. Tasks cannot end yet (P3), so the task records
/// how it ended and stays parked.
fn exit(status: i32) -> SyscallResult {
    let exit = Exit::Status(status as u8);
    match process::current() {
        Some(process) => {
            info!("process {} ({}) {exit}", process.id, process.name);
            process.set_exit(exit);
        }
        None => info!("user task {} {exit}", task_name()),
    }
    park()
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
    match process::current() {
        Some(process) => {
            info!("process {} ({}) {exit}: {fault}", process.id, process.name);
            process.set_exit(exit);
        }
        None => info!("user task {} killed: {fault}", task_name()),
    }
    park()
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

fn task_name() -> &'static str {
    task::current().map_or("?", |task| task.name())
}

/// Keeps the current task from running user code again.
fn park() -> ! {
    loop {
        ochra_frame::arch::interrupts::wait();
    }
}
