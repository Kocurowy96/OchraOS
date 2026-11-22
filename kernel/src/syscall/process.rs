// SPDX-License-Identifier: GPL-2.0-only

//! Process syscalls: `fork`, `execve`, `exit`, `wait4`, IDs, and per-thread settings.

use alloc::string::String;
use alloc::vec::Vec;

use log::{debug, info};
use ochra_frame::user_mode::UserContext;
use ochra_frame::{random, task};

use super::{SyscallResult, current, read_string, read_u64, write_user};
use crate::errno::{E2BIG, EFAULT, EINVAL, ENAMETOOLONG, ENOENT, ENOSYS, EPERM, Errno};
use crate::memory::STACK_LIMIT;
use crate::process::{self, Exit, Selector};

/// `fork()` (and `vfork()`, which may behave as `fork`: the parent just does not wait).
pub fn fork(context: &UserContext) -> SyscallResult {
    let process = current()?;
    process::fork(&process, context).map(u64::from)
}

/// `clone(flags, stack, …)`: only the form that is a plain `fork` (no shared memory, no new
/// stack, `SIGCHLD` at exit). Threads come in stage 4.
pub fn clone(context: &UserContext, flags: u64, stack: u64) -> SyscallResult {
    const SIGCHLD: u64 = 17;
    if flags != SIGCHLD || stack != 0 {
        debug!("clone with flags {flags:#x} is not implemented");
        return Err(ENOSYS);
    }
    fork(context)
}

/// Longest path `execve` takes, with its NUL (Linux's `PATH_MAX`).
const PATH_MAX: usize = 4096;
/// Longest single argument or environment string (Linux's `MAX_ARG_STRLEN`).
const MAX_ARGUMENT: usize = 32 * 4096;

/// `execve(pathname, argv, envp)`.
pub fn execve(
    context: &mut UserContext,
    path: u64,
    arguments: u64,
    environment: u64,
) -> SyscallResult {
    let path = read_string(path, PATH_MAX - 1).map_err(|error| error.unwrap_or(ENAMETOOLONG))?;
    // Until the VFS, every path is UTF-8 (the initramfs's are).
    let path = String::from_utf8(path).map_err(|_| ENOENT)?;
    let mut total = 0;
    let arguments = read_strings(arguments, &mut total)?;
    let environment = read_strings(environment, &mut total)?;
    let arguments: Vec<&[u8]> = arguments.iter().map(Vec::as_slice).collect();
    let environment: Vec<&[u8]> = environment.iter().map(Vec::as_slice).collect();
    let process = current()?;
    process::exec(&process, context, &path, &arguments, &environment)?;
    Ok(0)
}

/// Reads a NULL-terminated array of string pointers (a NULL array is empty, as in Linux),
/// adding the space it needs on the new stack to `total`.
fn read_strings(array: u64, total: &mut usize) -> Result<Vec<Vec<u8>>, Errno> {
    let mut strings = Vec::new();
    if array == 0 {
        return Ok(strings);
    }
    loop {
        let pointer = read_u64(array + strings.len() as u64 * 8)?;
        if pointer == 0 {
            return Ok(strings);
        }
        let text =
            read_string(pointer, MAX_ARGUMENT - 1).map_err(|error| error.unwrap_or(E2BIG))?;
        // The string, its NUL and its pointer; Linux allows a quarter of the stack limit.
        *total += text.len() + 1 + 8;
        if *total > STACK_LIMIT as usize / 4 {
            return Err(E2BIG);
        }
        strings.push(text);
    }
}

/// `exit(status)` / `exit_group(status)` (the same while processes have one thread).
pub fn exit(status: i32) -> SyscallResult {
    let exit = Exit::Status(status as u8);
    match process::current() {
        Some(process) => process::exit(&process, exit),
        None => {
            let name = task::current().map_or("?", |task| task.name());
            info!("user task {name} {exit}");
            crate::sched::exit_current()
        }
    }
}

/// `wait4(pid, wstatus, options, rusage)`.
pub fn wait4(pid: i64, status: u64, options: u64, usage: u64) -> SyscallResult {
    const WNOHANG: u64 = 1;
    // Accepted, but there are no stopped or continued children yet (signals, P4).
    const WUNTRACED: u64 = 2;
    const WCONTINUED: u64 = 8;
    // Thread-related, meaningless while processes have one thread.
    const WNOTHREAD: u64 = 0x2000_0000;
    const WALL: u64 = 0x4000_0000;
    const WCLONE: u64 = 0x8000_0000;
    /// `struct rusage`: two `timeval`s and 14 `long`s.
    const RUSAGE_SIZE: usize = 144;
    if options & !(WNOHANG | WUNTRACED | WCONTINUED | WNOTHREAD | WALL | WCLONE) != 0 {
        return Err(EINVAL);
    }
    let process = current()?;
    let selector = match pid {
        -1 => Selector::Any,
        0 => Selector::Group(process::group(&process)),
        pid if pid < -1 => Selector::Group(pid.unsigned_abs() as u32),
        pid => Selector::Process(pid as u32),
    };
    let Some((child, exit)) = process::wait(&process, selector, options & WNOHANG != 0)? else {
        return Ok(0);
    };
    if status != 0 {
        write_user(status, &exit.wait_status().to_le_bytes())?;
    }
    if usage != 0 {
        // No resource accounting yet: all zero.
        write_user(usage, &[0; RUSAGE_SIZE])?;
    }
    Ok(u64::from(child))
}

/// `getpid()`; also `gettid()` and `set_tid_address()` while each process has one thread
/// (its thread ID is the process ID). `set_tid_address`'s pointer only matters for threads.
pub fn getpid() -> SyscallResult {
    Ok(u64::from(current()?.id))
}

/// `getppid()`: 0 for a process the kernel started (init too, as in Linux).
pub fn getppid() -> SyscallResult {
    Ok(u64::from(current()?.parent()))
}

/// `getpgrp()`.
pub fn getpgrp() -> SyscallResult {
    let process = current()?;
    Ok(u64::from(process::group(&process)))
}

/// `sched_yield()`: lets the next task run.
pub fn sched_yield() -> SyscallResult {
    crate::sched::schedule();
    Ok(0)
}

/// `arch_prctl(code, addr)`: the thread pointer, which the C library sets up for
/// thread-local storage (TLS) and reads through the FS segment.
pub fn arch_prctl(code: u64, address: u64) -> SyscallResult {
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

/// `getrandom(buf, buflen, flags)`: bytes from the kernel's ChaCha20 generator, which is
/// seeded at boot, so it never blocks.
pub fn getrandom(buffer: u64, length: u64, flags: u64) -> SyscallResult {
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
