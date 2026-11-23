// SPDX-License-Identifier: GPL-2.0-only

//! Syscalls that take paths: opening, `stat`, `access`, the current directory, links. Until
//! the VFS (stage 3) every path is in the read-only initramfs.
//!
//! The `…at` forms take a directory descriptor: a relative path starts there instead of in
//! the current directory (`AT_FDCWD`, -100, means the current directory).

use alloc::string::String;
use alloc::sync::Arc;

use super::{SyscallResult, current, read_string, write_user};
use crate::errno::{
    EACCES, EBADF, EEXIST, EINVAL, EISDIR, ELOOP, ENAMETOOLONG, ENOENT, ENOTDIR, ERANGE, EROFS,
    Errno,
};
use crate::file::{self, Descriptor, InitramfsFile, OpenFile, flags};
use crate::initramfs::{self, Found, Node, PATH_MAX};

/// "Relative to the current directory" for the `…at` syscalls.
const AT_FDCWD: i32 = -100;
/// `…at` flags.
const AT_SYMLINK_NOFOLLOW: u64 = 0x100;
const AT_EMPTY_PATH: u64 = 0x1000;

/// Reads a path argument. Initramfs names are UTF-8, so a path that is not cannot exist.
fn read_path(address: u64) -> Result<String, Errno> {
    let path = read_string(address, PATH_MAX - 1).map_err(|error| error.unwrap_or(ENAMETOOLONG))?;
    String::from_utf8(path).map_err(|_| ENOENT)
}

/// The directory a relative path starts from: the current one, or the directory `dirfd`.
fn base_directory(dirfd: i32) -> Result<String, Errno> {
    let process = current()?;
    if dirfd == AT_FDCWD {
        return Ok(process.cwd.lock().clone());
    }
    let fd = u64::try_from(dirfd).map_err(|_| EBADF)?;
    let file = process.files.lock().file(fd)?;
    file.file.directory().map(String::from).ok_or(ENOTDIR)
}

/// Looks up `path` from `dirfd`.
fn lookup(dirfd: i32, path: &str, follow: bool) -> Result<Found, Errno> {
    // An absolute path ignores `dirfd` entirely (even a bad one), as in Linux.
    let base = if path.starts_with('/') {
        String::from("/")
    } else {
        base_directory(dirfd)?
    };
    initramfs::resolve(&base, path, follow)
}

/// `open(pathname, flags, mode)`.
pub fn open(path: u64, open_flags: u64, mode: u64) -> SyscallResult {
    openat(AT_FDCWD, path, open_flags, mode)
}

/// `openat(dirfd, pathname, flags, mode)`. The initramfs is read-only: asking to write or
/// to create a file that is not there fails with `EROFS`. Device nodes open their device.
pub fn openat(dirfd: i32, path: u64, open_flags: u64, _mode: u64) -> SyscallResult {
    const ACCESS_MODE: u32 = 3;
    const O_CREAT: u32 = 0o100;
    const O_EXCL: u32 = 0o200;
    const O_TRUNC: u32 = 0o1000;
    const O_DIRECTORY: u32 = 0o200_000;
    const O_NOFOLLOW: u32 = 0o400_000;
    let open_flags = open_flags as u32;
    let path = read_path(path)?;
    let found = match lookup(dirfd, &path, open_flags & O_NOFOLLOW == 0) {
        Err(ENOENT) if open_flags & O_CREAT != 0 => return Err(EROFS),
        result => result?,
    };
    if open_flags & (O_CREAT | O_EXCL) == O_CREAT | O_EXCL {
        return Err(EEXIST);
    }
    let writes = open_flags & ACCESS_MODE != flags::O_RDONLY || open_flags & O_TRUNC != 0;
    let is_directory = matches!(found.node, Node::Directory);
    if open_flags & O_DIRECTORY != 0 && !is_directory {
        return Err(ENOTDIR);
    }
    let file: Arc<dyn file::File> = match found.node {
        // `O_NOFOLLOW` and the last component is a link.
        Node::Symlink(_) => return Err(ELOOP),
        Node::CharacterDevice(major, minor) => file::open_device(major, minor)?,
        Node::Directory if writes => return Err(EISDIR),
        _ if writes => return Err(EROFS),
        _ => Arc::new(InitramfsFile::new(found)),
    };
    let file = OpenFile::new(file, open_flags);
    let descriptor = Descriptor {
        file,
        close_on_exec: open_flags & flags::O_CLOEXEC != 0,
    };
    current()?.files.lock().insert(descriptor, 0)
}

/// `lseek(fd, offset, whence)`.
pub fn lseek(fd: u64, offset: u64, whence: u64) -> SyscallResult {
    let file = current()?.files.lock().file(fd)?;
    let whence = u32::try_from(whence).map_err(|_| EINVAL)?;
    file.file.seek(offset as i64, whence)
}

/// `pread64(fd, buf, count, offset)`.
pub fn pread64(fd: u64, buffer: u64, count: u64, offset: u64) -> SyscallResult {
    if (offset as i64) < 0 {
        return Err(EINVAL);
    }
    let file = current()?.files.lock().file(fd)?;
    let mut bytes = alloc::vec![0; (count as usize).min(64 * 1024)];
    let read = file.file.read_at(&mut bytes, offset)?;
    write_user(buffer, &bytes[..read])?;
    Ok(read as u64)
}

/// `fstat(fd, statbuf)`.
pub fn fstat(fd: u64, buffer: u64) -> SyscallResult {
    let file = current()?.files.lock().file(fd)?;
    write_user(buffer, &file.file.stat().to_bytes())?;
    Ok(0)
}

/// `stat(pathname, statbuf)`.
pub fn stat(path: u64, buffer: u64) -> SyscallResult {
    newfstatat(AT_FDCWD, path, buffer, 0)
}

/// `lstat(pathname, statbuf)`: a link itself, not what it points to.
pub fn lstat(path: u64, buffer: u64) -> SyscallResult {
    newfstatat(AT_FDCWD, path, buffer, AT_SYMLINK_NOFOLLOW)
}

/// `newfstatat(dirfd, pathname, statbuf, flags)`; with `AT_EMPTY_PATH` and an empty path,
/// `dirfd` itself (as `fstat`).
pub fn newfstatat(dirfd: i32, path: u64, buffer: u64, at_flags: u64) -> SyscallResult {
    if at_flags & !(AT_SYMLINK_NOFOLLOW | AT_EMPTY_PATH) != 0 {
        return Err(EINVAL);
    }
    let path = read_path(path)?;
    if path.is_empty() && at_flags & AT_EMPTY_PATH != 0 {
        if dirfd == AT_FDCWD {
            let cwd = current()?.cwd.lock().clone();
            let found = initramfs::resolve("/", &cwd, true)?;
            write_user(buffer, &file::initramfs_stat(&found).to_bytes())?;
            return Ok(0);
        }
        return fstat(u64::try_from(dirfd).map_err(|_| EBADF)?, buffer);
    }
    let found = lookup(dirfd, &path, at_flags & AT_SYMLINK_NOFOLLOW == 0)?;
    write_user(buffer, &file::initramfs_stat(&found).to_bytes())?;
    Ok(0)
}

/// `access(pathname, mode)`.
pub fn access(path: u64, mode: u64) -> SyscallResult {
    faccessat2(AT_FDCWD, path, mode, 0)
}

/// `faccessat2(dirfd, pathname, mode, flags)` (and `faccessat`, without flags). Every
/// process is root for now: reading is always allowed, running needs an execute bit, and
/// writing is refused because the filesystem is read-only.
pub fn faccessat2(dirfd: i32, path: u64, mode: u64, at_flags: u64) -> SyscallResult {
    const X_OK: u64 = 1;
    const W_OK: u64 = 2;
    const R_OK: u64 = 4;
    /// Check with the effective IDs; the same as the real ones here.
    const AT_EACCESS: u64 = 0x200;
    if mode & !(X_OK | W_OK | R_OK) != 0
        || at_flags & !(AT_EACCESS | AT_SYMLINK_NOFOLLOW | AT_EMPTY_PATH) != 0
    {
        return Err(EINVAL);
    }
    let path = read_path(path)?;
    let found = lookup(dirfd, &path, at_flags & AT_SYMLINK_NOFOLLOW == 0)?;
    if mode & W_OK != 0 {
        return Err(EROFS);
    }
    let executable = found.mode & 0o111 != 0;
    if mode & X_OK != 0 && !executable {
        return Err(EACCES);
    }
    Ok(0)
}

/// `getcwd(buf, size)`: the path with its NUL; returns its length (with the NUL).
pub fn getcwd(buffer: u64, size: u64) -> SyscallResult {
    let mut path = current()?.cwd.lock().clone().into_bytes();
    path.push(0);
    if (path.len() as u64) > size {
        return Err(ERANGE);
    }
    write_user(buffer, &path)?;
    Ok(path.len() as u64)
}

/// `chdir(path)`.
pub fn chdir(path: u64) -> SyscallResult {
    let path = read_path(path)?;
    let found = lookup(AT_FDCWD, &path, true)?;
    if !matches!(found.node, Node::Directory) {
        return Err(ENOTDIR);
    }
    *current()?.cwd.lock() = found.path;
    Ok(0)
}

/// `fchdir(fd)`.
pub fn fchdir(fd: u64) -> SyscallResult {
    let process = current()?;
    let file = process.files.lock().file(fd)?;
    let path = file.file.directory().ok_or(ENOTDIR)?;
    *process.cwd.lock() = String::from(path);
    Ok(0)
}

/// `readlink(pathname, buf, bufsiz)`.
pub fn readlink(path: u64, buffer: u64, size: u64) -> SyscallResult {
    readlinkat(AT_FDCWD, path, buffer, size)
}

/// `readlinkat(dirfd, pathname, buf, bufsiz)`: the link's target, cut to `bufsiz`, without
/// a NUL.
pub fn readlinkat(dirfd: i32, path: u64, buffer: u64, size: u64) -> SyscallResult {
    if size as i64 <= 0 {
        return Err(EINVAL);
    }
    let path = read_path(path)?;
    let found = lookup(dirfd, &path, false)?;
    let Node::Symlink(target) = found.node else {
        return Err(EINVAL);
    };
    let count = target.len().min(size as usize);
    write_user(buffer, &target.as_bytes()[..count])?;
    Ok(count as u64)
}
