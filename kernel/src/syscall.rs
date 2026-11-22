// SPDX-License-Identifier: GPL-2.0-only

//! System calls, with Linux numbers and Linux behaviour (see `man 2 <name>`).
//!
//! `ochra_frame::user_mode` brings every `syscall` here with the user's registers; the number
//! picks the function, and the result goes back in RAX: a value, or `-errno` on failure.

use alloc::vec;

use log::{debug, info};
use ochra_frame::user_mode::{self, UserContext, UserFault, copy_from_user};
use ochra_frame::{print, task};

/// Linux error numbers (`include/uapi/asm-generic/errno-base.h`, `errno.h`).
mod errno {
    pub const EBADF: i64 = 9;
    pub const EFAULT: i64 = 14;
    pub const ENOSYS: i64 = 38;
}

/// Linux x86-64 syscall numbers (`arch/x86/entry/syscalls/syscall_64.tbl`).
mod number {
    pub const WRITE: u64 = 1;
    pub const EXIT: u64 = 60;
    pub const EXIT_GROUP: u64 = 231;
}

/// Most bytes one `write` to the console copies; the caller sees a short write and retries
/// with the rest, as POSIX allows.
const MAX_WRITE: usize = 64 * 1024;

/// Registers the handlers with `ochra-frame`. Call once, before any user task runs.
pub fn init() {
    user_mode::set_syscall_handler(dispatch);
    user_mode::set_fault_handler(on_fault);
}

type SyscallResult = Result<u64, i64>;

fn dispatch(context: &mut UserContext) {
    let number = context.syscall_number();
    let argument = |index| context.argument(index);
    let result = match number {
        number::WRITE => write(argument(0), argument(1), argument(2)),
        number::EXIT | number::EXIT_GROUP => exit(argument(0) as i32),
        _ => {
            debug!("syscall {number} is not implemented");
            Err(errno::ENOSYS)
        }
    };
    context.set_result(match result {
        Ok(value) => value,
        Err(error) => (-error) as u64,
    });
}

/// `write(fd, buf, count)`: only the console (fds 1 and 2) for now.
fn write(fd: u64, buffer: u64, count: u64) -> SyscallResult {
    if fd != 1 && fd != 2 {
        return Err(errno::EBADF);
    }
    let length = (count as usize).min(MAX_WRITE);
    let mut bytes = vec![0; length];
    copy_from_user(&mut bytes, buffer).map_err(|_| errno::EFAULT)?;
    // The console takes text; invalid UTF-8 shows as replacement characters.
    print!("{}", alloc::string::String::from_utf8_lossy(&bytes));
    Ok(length as u64)
}

/// `exit(status)` / `exit_group(status)`. Tasks cannot end yet (that comes with processes),
/// so the task only reports and stays parked.
fn exit(status: i32) -> SyscallResult {
    info!("user task {} exited with status {status}", task_name());
    park()
}

/// A user program raised an exception: it is killed. (Signals come later.)
fn on_fault(_context: &mut UserContext, fault: UserFault) {
    info!("user task {} killed: {fault}", task_name());
    park()
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
