// SPDX-License-Identifier: GPL-2.0-only

//! Memory syscalls; the work is done by `crate::memory`.

use super::{SyscallResult, current};

/// `brk(addr)`: returns the new program break, or the old one if it could not move.
pub fn brk(address: u64) -> SyscallResult {
    let process = current()?;
    let space = process.space()?;
    let result = process.memory.lock().brk(&space, address);
    Ok(result)
}

/// `mmap(addr, length, prot, flags, fd, offset)`; the file descriptor is not used yet (only
/// anonymous memory).
pub fn mmap(address: u64, length: u64, protection: u64, flags: u64, offset: u64) -> SyscallResult {
    let process = current()?;
    let space = process.space()?;
    process
        .memory
        .lock()
        .mmap(&space, address, length, protection, flags, offset)
}

pub fn munmap(address: u64, length: u64) -> SyscallResult {
    let process = current()?;
    let space = process.space()?;
    let result = process.memory.lock().munmap(&space, address, length);
    result.map(|()| 0)
}

pub fn mprotect(address: u64, length: u64, protection: u64) -> SyscallResult {
    let process = current()?;
    let space = process.space()?;
    let result = process
        .memory
        .lock()
        .mprotect(&space, address, length, protection);
    result.map(|()| 0)
}
