// SPDX-License-Identifier: GPL-2.0-only

//! File descriptor syscalls: reading, writing, closing, duplicating, pipes.

use alloc::vec;
use alloc::vec::Vec;

use log::debug;

use super::{SyscallResult, current, read_user, write_user};
use crate::errno::{EFAULT, EINVAL, ENOTTY, Errno};
use crate::file::{self, Descriptor, MAX_DESCRIPTORS, OpenFile, flags};

/// Most bytes one `read` or `write` moves; the caller sees a short count and goes on with
/// the rest, as POSIX allows.
const MAX_TRANSFER: usize = 64 * 1024;
/// Linux's `UIO_MAXIOV`: most pieces in one `readv`/`writev`.
const MAX_PIECES: u64 = 1024;

fn open_file(fd: u64) -> Result<alloc::sync::Arc<OpenFile>, Errno> {
    match current() {
        Ok(process) => process.files.lock().file(fd),
        // The built-in ring 3 self-tests have no process; they write to the console.
        Err(_) if fd == 1 || fd == 2 => Ok(OpenFile::new(
            alloc::sync::Arc::new(crate::console::Console),
            0,
        )),
        Err(_) => Err(crate::errno::EBADF),
    }
}

/// `read(fd, buf, count)`.
pub fn read(fd: u64, buffer: u64, count: u64) -> SyscallResult {
    let file = open_file(fd)?;
    let mut bytes = vec![0; (count as usize).min(MAX_TRANSFER)];
    let read = file.file.read(&mut bytes, file.nonblocking())?;
    write_user(buffer, &bytes[..read])?;
    Ok(read as u64)
}

/// `write(fd, buf, count)`. Copied in pieces of `MAX_TRANSFER`, but all of it is written
/// unless the file takes less (a blocking pipe takes everything, as in Linux).
pub fn write(fd: u64, buffer: u64, count: u64) -> SyscallResult {
    let file = open_file(fd)?;
    let mut done: u64 = 0;
    loop {
        let piece = ((count - done) as usize).min(MAX_TRANSFER);
        let mut bytes = vec![0; piece];
        let result = read_user(&mut bytes, buffer + done)
            .and_then(|()| file.file.write(&bytes, file.nonblocking()));
        match result {
            Ok(written) => {
                done += written as u64;
                if written < piece || done == count {
                    return Ok(done);
                }
            }
            // An error after some bytes went through shows on the next call.
            Err(_) if done > 0 => return Ok(done),
            Err(error) => return Err(error),
        }
    }
}

/// Reads a `struct iovec` array: `(base, length)` pairs.
fn read_vector(vector: u64, count: u64) -> Result<Vec<(u64, u64)>, Errno> {
    if count > MAX_PIECES {
        return Err(EINVAL);
    }
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
    Ok(pieces)
}

/// `readv(fd, iov, iovcnt)`: one read, spread over several buffers.
pub fn readv(fd: u64, vector: u64, count: u64) -> SyscallResult {
    let file = open_file(fd)?;
    let pieces = read_vector(vector, count)?;
    let total: u64 = pieces.iter().map(|&(_, length)| length).sum();
    let mut bytes = vec![0; (total as usize).min(MAX_TRANSFER)];
    let read = file.file.read(&mut bytes, file.nonblocking())?;
    let mut done = 0;
    for (address, length) in pieces {
        if done == read {
            break;
        }
        let part = (length as usize).min(read - done);
        write_user(address, &bytes[done..done + part])?;
        done += part;
    }
    Ok(read as u64)
}

/// `writev(fd, iov, iovcnt)`: writes several buffers as one.
pub fn writev(fd: u64, vector: u64, count: u64) -> SyscallResult {
    let file = open_file(fd)?;
    let pieces = read_vector(vector, count)?;
    let mut bytes = Vec::new();
    for (address, length) in pieces {
        let length = (length as usize).min(MAX_TRANSFER - bytes.len());
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
        if bytes.len() == MAX_TRANSFER {
            break;
        }
    }
    let written = file.file.write(&bytes, file.nonblocking())?;
    Ok(written as u64)
}

/// `close(fd)`.
pub fn close(fd: u64) -> SyscallResult {
    let process = current()?;
    let descriptor = process.files.lock().remove(fd)?;
    // Dropped outside the table's lock: closing a pipe end wakes the other side.
    drop(descriptor);
    Ok(0)
}

/// `ioctl(fd, request, argp)`: only what the C library asks a terminal about.
pub fn ioctl(fd: u64, request: u64, argument: u64) -> SyscallResult {
    /// Get the window size (`struct winsize`).
    const TIOCGWINSZ: u64 = 0x5413;
    let file = open_file(fd)?;
    if !file.file.is_terminal() {
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

/// `pipe2(pipefd, flags)` (and `pipe`, with no flags): writes the read end's and the write
/// end's descriptor numbers as two `int`s.
pub fn pipe2(descriptors: u64, pipe_flags: u64) -> SyscallResult {
    let allowed = u64::from(flags::O_CLOEXEC | flags::O_NONBLOCK);
    if pipe_flags & !allowed != 0 {
        return Err(EINVAL);
    }
    let status = pipe_flags as u32;
    let close_on_exec = status & flags::O_CLOEXEC != 0;
    let (reader, writer) = file::pipe();
    let process = current()?;
    let (read_fd, write_fd) = {
        let mut table = process.files.lock();
        let read_fd = table.insert(
            Descriptor {
                file: OpenFile::new(reader, status),
                close_on_exec,
            },
            0,
        )?;
        let write_fd = match table.insert(
            Descriptor {
                file: OpenFile::new(writer, status),
                close_on_exec,
            },
            0,
        ) {
            Ok(fd) => fd,
            Err(error) => {
                let _ = table.remove(read_fd);
                return Err(error);
            }
        };
        (read_fd, write_fd)
    };
    let mut numbers = [0; 8];
    numbers[..4].copy_from_slice(&(read_fd as u32).to_le_bytes());
    numbers[4..].copy_from_slice(&(write_fd as u32).to_le_bytes());
    if let Err(error) = write_user(descriptors, &numbers) {
        // Linux leaves no descriptors behind when it cannot report them.
        let mut table = process.files.lock();
        let ends = (table.remove(read_fd), table.remove(write_fd));
        drop(table);
        drop(ends);
        return Err(error);
    }
    Ok(0)
}

/// Duplicates `fd` to the lowest free number at least `minimum`.
fn duplicate(fd: u64, minimum: usize, close_on_exec: bool) -> SyscallResult {
    let process = current()?;
    let mut table = process.files.lock();
    let file = table.file(fd)?;
    table.insert(
        Descriptor {
            file,
            close_on_exec,
        },
        minimum,
    )
}

/// `dup(oldfd)`.
pub fn dup(fd: u64) -> SyscallResult {
    duplicate(fd, 0, false)
}

/// `dup2(oldfd, newfd)`: like `dup3` without flags, but `oldfd == newfd` is fine.
pub fn dup2(old: u64, new: u64) -> SyscallResult {
    if old == new {
        current()?.files.lock().get(old)?;
        return Ok(new);
    }
    dup3(old, new, 0)
}

/// `dup3(oldfd, newfd, flags)`: makes `newfd` refer to `oldfd`'s file, closing what
/// `newfd` referred to.
pub fn dup3(old: u64, new: u64, dup_flags: u64) -> SyscallResult {
    if old == new || dup_flags & !u64::from(flags::O_CLOEXEC) != 0 {
        return Err(EINVAL);
    }
    let process = current()?;
    let replaced = {
        let mut table = process.files.lock();
        let file = table.file(old)?;
        let descriptor = Descriptor {
            file,
            close_on_exec: dup_flags != 0,
        };
        table.insert_at(new, descriptor)?
    };
    // Closed outside the lock, as in `close`.
    drop(replaced);
    Ok(new)
}

/// `fcntl(fd, cmd, arg)`: duplicating, the close-on-exec flag and the status flags.
pub fn fcntl(fd: u64, command: u64, argument: u64) -> SyscallResult {
    const F_DUPFD: u64 = 0;
    const F_GETFD: u64 = 1;
    const F_SETFD: u64 = 2;
    const F_GETFL: u64 = 3;
    const F_SETFL: u64 = 4;
    const F_DUPFD_CLOEXEC: u64 = 1030;
    /// The only descriptor flag: close on exec.
    const FD_CLOEXEC: u64 = 1;
    let process = current()?;
    match command {
        F_DUPFD | F_DUPFD_CLOEXEC => {
            let minimum = usize::try_from(argument)
                .ok()
                .filter(|&minimum| minimum < MAX_DESCRIPTORS)
                .ok_or(EINVAL)?;
            drop(process);
            duplicate(fd, minimum, command == F_DUPFD_CLOEXEC)
        }
        F_GETFD => {
            let table = process.files.lock();
            Ok(u64::from(table.get(fd)?.close_on_exec))
        }
        F_SETFD => {
            let mut table = process.files.lock();
            table.get_mut(fd)?.close_on_exec = argument & FD_CLOEXEC != 0;
            Ok(0)
        }
        F_GETFL => {
            let file = process.files.lock().file(fd)?;
            Ok(u64::from(file.status_flags()))
        }
        F_SETFL => {
            let file = process.files.lock().file(fd)?;
            file.set_status_flags(argument as u32);
            Ok(0)
        }
        _ => {
            process.files.lock().get(fd)?;
            debug!("fcntl command {command} is not implemented");
            Err(EINVAL)
        }
    }
}
