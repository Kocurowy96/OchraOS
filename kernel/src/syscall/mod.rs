// SPDX-License-Identifier: GPL-2.0-only

//! System calls, with Linux numbers and Linux behaviour (see `man 2 <name>`).
//!
//! `ochra_frame::user_mode` brings every `syscall` here with the user's registers; the number
//! picks the function, and the result goes back in RAX: a value, or `-errno` on failure.
//! The functions are grouped by topic: files, memory and processes.

mod files;
mod memory;
mod names;
mod paths;
mod process;
mod signal;
mod system;

use alloc::sync::Arc;
use alloc::vec::Vec;

use log::{debug, info};
use ochra_frame::task;
use ochra_frame::user_mode::{self, UserContext, UserFault, copy_from_user, copy_to_user};

use crate::errno::{EFAULT, ENOSYS, ERESTARTNOHAND, ERESTARTNOINTR, ERESTARTSYS, Errno};
use crate::process::Process;

/// Linux x86-64 syscall numbers (`arch/x86/entry/syscalls/syscall_64.tbl`).
mod number {
    pub const READ: u64 = 0;
    pub const WRITE: u64 = 1;
    pub const OPEN: u64 = 2;
    pub const CLOSE: u64 = 3;
    pub const STAT: u64 = 4;
    pub const FSTAT: u64 = 5;
    pub const LSTAT: u64 = 6;
    pub const LSEEK: u64 = 8;
    pub const MMAP: u64 = 9;
    pub const MPROTECT: u64 = 10;
    pub const MUNMAP: u64 = 11;
    pub const BRK: u64 = 12;
    pub const RT_SIGACTION: u64 = 13;
    pub const RT_SIGPROCMASK: u64 = 14;
    pub const RT_SIGRETURN: u64 = 15;
    pub const IOCTL: u64 = 16;
    pub const PREAD64: u64 = 17;
    pub const PWRITE64: u64 = 18;
    pub const READV: u64 = 19;
    pub const WRITEV: u64 = 20;
    pub const ACCESS: u64 = 21;
    pub const PIPE: u64 = 22;
    pub const SCHED_YIELD: u64 = 24;
    pub const DUP: u64 = 32;
    pub const DUP2: u64 = 33;
    pub const PAUSE: u64 = 34;
    pub const NANOSLEEP: u64 = 35;
    pub const GETPID: u64 = 39;
    pub const CLONE: u64 = 56;
    pub const FORK: u64 = 57;
    pub const VFORK: u64 = 58;
    pub const EXECVE: u64 = 59;
    pub const EXIT: u64 = 60;
    pub const WAIT4: u64 = 61;
    pub const KILL: u64 = 62;
    pub const UNAME: u64 = 63;
    pub const FCNTL: u64 = 72;
    pub const FSYNC: u64 = 74;
    pub const FDATASYNC: u64 = 75;
    pub const TRUNCATE: u64 = 76;
    pub const FTRUNCATE: u64 = 77;
    pub const GETCWD: u64 = 79;
    pub const CHDIR: u64 = 80;
    pub const FCHDIR: u64 = 81;
    pub const RENAME: u64 = 82;
    pub const MKDIR: u64 = 83;
    pub const RMDIR: u64 = 84;
    pub const CREAT: u64 = 85;
    pub const LINK: u64 = 86;
    pub const UNLINK: u64 = 87;
    pub const SYMLINK: u64 = 88;
    pub const READLINK: u64 = 89;
    pub const CHMOD: u64 = 90;
    pub const FCHMOD: u64 = 91;
    pub const CHOWN: u64 = 92;
    pub const FCHOWN: u64 = 93;
    pub const LCHOWN: u64 = 94;
    pub const UMASK: u64 = 95;
    pub const GETTIMEOFDAY: u64 = 96;
    pub const GETUID: u64 = 102;
    pub const GETGID: u64 = 104;
    pub const GETEUID: u64 = 107;
    pub const GETEGID: u64 = 108;
    pub const GETPPID: u64 = 110;
    pub const GETPGRP: u64 = 111;
    pub const RT_SIGPENDING: u64 = 127;
    pub const RT_SIGSUSPEND: u64 = 130;
    pub const MKNOD: u64 = 133;
    pub const ARCH_PRCTL: u64 = 158;
    pub const SYNC: u64 = 162;
    pub const GETTID: u64 = 186;
    pub const TKILL: u64 = 200;
    pub const TIME: u64 = 201;
    pub const GETDENTS64: u64 = 217;
    pub const SET_TID_ADDRESS: u64 = 218;
    pub const CLOCK_GETTIME: u64 = 228;
    pub const CLOCK_GETRES: u64 = 229;
    pub const CLOCK_NANOSLEEP: u64 = 230;
    pub const EXIT_GROUP: u64 = 231;
    pub const TGKILL: u64 = 234;
    pub const OPENAT: u64 = 257;
    pub const MKDIRAT: u64 = 258;
    pub const MKNODAT: u64 = 259;
    pub const FCHOWNAT: u64 = 260;
    pub const NEWFSTATAT: u64 = 262;
    pub const UNLINKAT: u64 = 263;
    pub const RENAMEAT: u64 = 264;
    pub const LINKAT: u64 = 265;
    pub const SYMLINKAT: u64 = 266;
    pub const READLINKAT: u64 = 267;
    pub const FCHMODAT: u64 = 268;
    pub const FACCESSAT: u64 = 269;
    pub const UTIMENSAT: u64 = 280;
    pub const DUP3: u64 = 292;
    pub const PIPE2: u64 = 293;
    pub const SYNCFS: u64 = 306;
    pub const RENAMEAT2: u64 = 316;
    pub const GETRANDOM: u64 = 318;
    pub const FACCESSAT2: u64 = 439;
}

/// Registers the handlers with `ochra-frame`. Call once, before any user task runs.
pub fn init() {
    user_mode::set_syscall_handler(dispatch);
    user_mode::set_fault_handler(on_fault);
    user_mode::set_return_handler(crate::signal::on_return);
}

type SyscallResult = Result<u64, Errno>;

fn dispatch(context: &mut UserContext) {
    let number = context.syscall_number();
    let a = |index| context.argument(index);
    let (a0, a1, a2, a3, a4, a5) = (a(0), a(1), a(2), a(3), a(4), a(5));
    // A directory descriptor argument is an `int`.
    let dirfd = a0 as i32;
    let result = match number {
        number::READ => files::read(a0, a1, a2),
        number::WRITE => files::write(a0, a1, a2),
        number::READV => files::readv(a0, a1, a2),
        number::WRITEV => files::writev(a0, a1, a2),
        number::CLOSE => files::close(a0),
        number::OPEN => paths::open(a0, a1, a2),
        number::OPENAT => paths::openat(dirfd, a1, a2, a3),
        number::CREAT => paths::creat(a0, a1),
        number::LSEEK => files::lseek(a0, a1, a2),
        number::PREAD64 => files::pread64(a0, a1, a2, a3),
        number::PWRITE64 => files::pwrite64(a0, a1, a2, a3),
        number::FSTAT => files::fstat(a0, a1),
        number::FTRUNCATE => files::ftruncate(a0, a1),
        number::FSYNC | number::FDATASYNC | number::SYNCFS => files::fsync(a0),
        number::SYNC => Ok(0),
        number::GETDENTS64 => files::getdents64(a0, a1, a2),
        number::STAT => paths::stat(a0, a1),
        number::LSTAT => paths::lstat(a0, a1),
        number::NEWFSTATAT => paths::newfstatat(dirfd, a1, a2, a3),
        number::ACCESS => paths::access(a0, a1),
        number::FACCESSAT => paths::faccessat2(dirfd, a1, a2, 0),
        number::FACCESSAT2 => paths::faccessat2(dirfd, a1, a2, a3),
        number::GETCWD => paths::getcwd(a0, a1),
        number::CHDIR => paths::chdir(a0),
        number::FCHDIR => paths::fchdir(a0),
        number::READLINK => paths::readlink(a0, a1, a2),
        number::READLINKAT => paths::readlinkat(dirfd, a1, a2, a3),
        number::CHMOD => paths::chmod(a0, a1),
        number::FCHMOD => paths::fchmod(a0, a1),
        number::FCHMODAT => paths::fchmodat(dirfd, a1, a2),
        number::CHOWN => paths::chown(a0, a1, a2),
        number::FCHOWN => paths::fchown(a0, a1, a2),
        number::LCHOWN => paths::lchown(a0, a1, a2),
        number::FCHOWNAT => paths::fchownat(dirfd, a1, a2, a3, a4),
        number::UMASK => paths::umask(a0),
        number::UTIMENSAT => paths::utimensat(dirfd, a1, a2, a3),
        number::TRUNCATE => paths::truncate(a0, a1),
        number::MKDIR => names::mkdir(a0, a1),
        number::MKDIRAT => names::mkdirat(dirfd, a1, a2),
        number::RMDIR => names::rmdir(a0),
        number::UNLINK => names::unlink(a0),
        number::UNLINKAT => names::unlinkat(dirfd, a1, a2),
        number::RENAME => names::rename(a0, a1),
        number::RENAMEAT => names::renameat2(dirfd, a1, a2 as i32, a3, 0),
        number::RENAMEAT2 => names::renameat2(dirfd, a1, a2 as i32, a3, a4),
        number::LINK => names::link(a0, a1),
        number::LINKAT => names::linkat(dirfd, a1, a2 as i32, a3, a4),
        number::SYMLINK => names::symlink(a0, a1),
        number::SYMLINKAT => names::symlinkat(a0, a1 as i32, a2),
        number::MKNOD => names::mknod(a0, a1, a2),
        number::MKNODAT => names::mknodat(dirfd, a1, a2, a3),
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
        number::GETUID | number::GETGID | number::GETEUID | number::GETEGID => {
            process::identity(number)
        }
        number::GETPGRP => process::getpgrp(),
        number::SCHED_YIELD => process::sched_yield(),
        number::ARCH_PRCTL => process::arch_prctl(a0, a1),
        number::SET_TID_ADDRESS => process::getpid(),
        number::GETRANDOM => process::getrandom(a0, a1, a2),
        number::RT_SIGACTION => signal::rt_sigaction(a0, a1, a2, a3),
        number::RT_SIGPROCMASK => signal::rt_sigprocmask(a0, a1, a2, a3),
        number::RT_SIGRETURN => signal::rt_sigreturn(context),
        number::RT_SIGPENDING => signal::rt_sigpending(a0, a1),
        number::RT_SIGSUSPEND => signal::rt_sigsuspend(a0, a1),
        number::PAUSE => signal::pause(),
        number::KILL => signal::kill(a0 as i32, a1),
        number::UNAME => system::uname(a0),
        number::CLOCK_GETTIME => system::clock_gettime(a0, a1),
        number::CLOCK_GETRES => system::clock_getres(a0, a1),
        number::CLOCK_NANOSLEEP => system::clock_nanosleep(a0, a1, a2, a3),
        number::NANOSLEEP => system::nanosleep(a0, a1),
        number::GETTIMEOFDAY => system::gettimeofday(a0, a1),
        number::TIME => system::time(a0),
        number::TKILL => signal::tkill(a0 as i32, a1),
        number::TGKILL => signal::tgkill(a0 as i32, a1 as i32, a2),
        _ => {
            debug!("syscall {number} is not implemented");
            Err(ENOSYS)
        }
    };
    // `execve` replaced the registers with the new program's; nothing to return there.
    if number == number::EXECVE && result.is_ok() {
        return;
    }
    if let Err(ERESTARTSYS | ERESTARTNOINTR | ERESTARTNOHAND) = result
        && let Ok(process) = current()
    {
        // A signal interrupted the syscall; `signal::on_return` decides what the program sees.
        process.signals.lock().restart = Some(number);
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
pub(crate) fn read_user(buffer: &mut [u8], address: u64) -> Result<(), Errno> {
    copy_from_user(buffer, address)
        .or_else(|_| {
            grow_stack(address)?;
            copy_from_user(buffer, address)
        })
        .map_err(|_| EFAULT)
}

pub(crate) fn write_user(address: u64, data: &[u8]) -> Result<(), Errno> {
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
/// (the program then retries); anything else sends the program the signal Linux would send,
/// which `signal::on_return` delivers: a handler runs, or the program ends.
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
    let (signal, info) = crate::signal::for_fault(&fault);
    match crate::process::current() {
        Some(process) => {
            info!("process {} ({}): {fault}", process.id, process.name());
            crate::signal::force(&process, signal, info);
        }
        None => {
            let name = task::current().map_or("?", |task| task.name());
            info!("user task {name} killed: {fault}");
            crate::sched::exit_current()
        }
    }
}
