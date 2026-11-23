// SPDX-License-Identifier: GPL-2.0-only

//! Syscalls that make and remove directory entries: `mkdir`, `rmdir`, `unlink`, `rename`,
//! `link`, `symlink`, `mknod`. The checks (permissions, the sticky bit, a directory moved
//! into itself…) are the VFS's (`vfs/path.rs`); here are the arguments and the rules about
//! trailing slashes.

use core::sync::atomic::Ordering;

use super::paths::{
    AT_EMPTY_PATH, AT_FDCWD, AT_SYMLINK_FOLLOW, credentials, lookup, lookup_parent, read_path,
};
use super::{SyscallResult, current};
use crate::errno::{EEXIST, EINVAL, EISDIR, ENOENT, ENOTDIR, EPERM};
use crate::vfs::{self, NewNode, Parent, Walk, mode};

/// The umask of the calling process.
fn umask() -> Result<u32, crate::errno::Errno> {
    Ok(current()?.umask.load(Ordering::Relaxed))
}

/// A name with a trailing slash must be a directory.
fn check_trailing_slash(parent: &Parent) -> Result<(), crate::errno::Errno> {
    if parent.trailing_slash {
        let found = parent
            .directory
            .walk(&parent.name, Walk::NoFollow, &credentials()?)?;
        if !found.metadata().is_directory() {
            return Err(ENOTDIR);
        }
    }
    Ok(())
}

/// `mkdir(pathname, mode)`.
pub fn mkdir(path: u64, permissions: u64) -> SyscallResult {
    mkdirat(AT_FDCWD, path, permissions)
}

/// `mkdirat(dirfd, pathname, mode)`: the permission bits and the sticky bit are kept,
/// less the umask.
pub fn mkdirat(dirfd: i32, path: u64, permissions: u64) -> SyscallResult {
    let path = read_path(path)?;
    let parent = lookup_parent(dirfd, &path)?;
    let permissions = permissions as u32 & (0o777 | mode::STICKY) & !umask()?;
    parent.directory.create(
        &parent.name,
        NewNode::Directory,
        permissions,
        &credentials()?,
    )?;
    Ok(0)
}

/// `rmdir(pathname)`.
pub fn rmdir(path: u64) -> SyscallResult {
    let path = read_path(path)?;
    remove_directory(AT_FDCWD, &path)
}

fn remove_directory(dirfd: i32, path: &str) -> SyscallResult {
    let parent = lookup_parent(dirfd, path)?;
    // `/` has no parent to be removed from.
    if parent.name == "." && path.trim_matches('/').is_empty() {
        return Err(crate::errno::EBUSY);
    }
    parent.directory.rmdir(&parent.name, &credentials()?)?;
    Ok(0)
}

/// `unlink(pathname)`.
pub fn unlink(path: u64) -> SyscallResult {
    unlinkat(AT_FDCWD, path, 0)
}

/// `unlinkat(dirfd, pathname, flags)`: `rmdir` with `AT_REMOVEDIR`.
pub fn unlinkat(dirfd: i32, path: u64, at_flags: u64) -> SyscallResult {
    const AT_REMOVEDIR: u64 = 0x200;
    if at_flags & !AT_REMOVEDIR != 0 {
        return Err(EINVAL);
    }
    let path = read_path(path)?;
    if at_flags & AT_REMOVEDIR != 0 {
        return remove_directory(dirfd, &path);
    }
    let parent = lookup_parent(dirfd, &path)?;
    if parent.trailing_slash {
        // `unlink("dir/")` is `EISDIR`, `unlink("file/")` is `ENOTDIR`.
        check_trailing_slash(&parent)?;
        return Err(EISDIR);
    }
    parent.directory.unlink(&parent.name, &credentials()?)?;
    Ok(0)
}

/// `rename(oldpath, newpath)`.
pub fn rename(old: u64, new: u64) -> SyscallResult {
    renameat2(AT_FDCWD, old, AT_FDCWD, new, 0)
}

/// `renameat2(olddirfd, oldpath, newdirfd, newpath, flags)` (and `renameat`, without
/// flags). `RENAME_NOREPLACE` is supported; exchanging two names (`RENAME_EXCHANGE`) and
/// whiteouts are not.
pub fn renameat2(
    old_dirfd: i32,
    old: u64,
    new_dirfd: i32,
    new: u64,
    rename_flags: u64,
) -> SyscallResult {
    const RENAME_NOREPLACE: u64 = 1;
    if rename_flags & !RENAME_NOREPLACE != 0 {
        return Err(EINVAL);
    }
    let old = read_path(old)?;
    let new = read_path(new)?;
    let source = lookup_parent(old_dirfd, &old)?;
    let target = lookup_parent(new_dirfd, &new)?;
    check_trailing_slash(&source)?;
    if target.trailing_slash {
        // The target may not exist yet: then the source decides.
        let moved = source
            .directory
            .walk(&source.name, Walk::NoFollow, &credentials()?)?;
        if !moved.metadata().is_directory() {
            return Err(ENOTDIR);
        }
    }
    vfs::rename(
        &source.directory,
        &source.name,
        &target.directory,
        &target.name,
        rename_flags & RENAME_NOREPLACE != 0,
        &credentials()?,
    )?;
    Ok(0)
}

/// `link(oldpath, newpath)`.
pub fn link(old: u64, new: u64) -> SyscallResult {
    linkat(AT_FDCWD, old, AT_FDCWD, new, 0)
}

/// `linkat(olddirfd, oldpath, newdirfd, newpath, flags)`: a link is not followed unless
/// `AT_SYMLINK_FOLLOW` (a hard link to a symbolic link is made).
pub fn linkat(old_dirfd: i32, old: u64, new_dirfd: i32, new: u64, at_flags: u64) -> SyscallResult {
    if at_flags & !(AT_SYMLINK_FOLLOW | AT_EMPTY_PATH) != 0 {
        return Err(EINVAL);
    }
    let old = read_path(old)?;
    let new = read_path(new)?;
    let target = if old.is_empty() && at_flags & AT_EMPTY_PATH != 0 {
        // Linux allows this only with `CAP_DAC_READ_SEARCH`: root.
        if !credentials()?.is_root() {
            return Err(ENOENT);
        }
        let fd = u64::try_from(old_dirfd).map_err(|_| crate::errno::EBADF)?;
        let file = current()?.files.lock().file(fd)?;
        file.file.path().ok_or(EINVAL)?
    } else {
        lookup(old_dirfd, &old, at_flags & AT_SYMLINK_FOLLOW != 0)?
    };
    let parent = lookup_parent(new_dirfd, &new)?;
    if parent.trailing_slash {
        return Err(if target.metadata().is_directory() {
            EPERM
        } else {
            ENOENT
        });
    }
    parent
        .directory
        .link(&parent.name, &target, &credentials()?)?;
    Ok(0)
}

/// `symlink(target, linkpath)`.
pub fn symlink(target: u64, path: u64) -> SyscallResult {
    symlinkat(target, AT_FDCWD, path)
}

/// `symlinkat(target, newdirfd, linkpath)`: links always have permissions 777.
pub fn symlinkat(target: u64, dirfd: i32, path: u64) -> SyscallResult {
    let target = read_path(target)?;
    if target.is_empty() {
        return Err(ENOENT);
    }
    let path = read_path(path)?;
    let parent = lookup_parent(dirfd, &path)?;
    if parent.trailing_slash {
        // A link cannot be a directory: `EEXIST` if the name is taken, else `ENOENT`.
        let exists = parent
            .directory
            .walk(&parent.name, Walk::NoFollow, &credentials()?)
            .is_ok();
        return Err(if exists { EEXIST } else { ENOENT });
    }
    parent.directory.create(
        &parent.name,
        NewNode::Symlink(target),
        0o777,
        &credentials()?,
    )?;
    Ok(0)
}

/// `mknod(pathname, mode, dev)`.
pub fn mknod(path: u64, file_mode: u64, device: u64) -> SyscallResult {
    mknodat(AT_FDCWD, path, file_mode, device)
}

/// `mknodat(dirfd, pathname, mode, dev)`: a regular file (type 0 too) or, for root, a
/// character device node. FIFOs, sockets and block devices cannot be made yet.
pub fn mknodat(dirfd: i32, path: u64, file_mode: u64, device: u64) -> SyscallResult {
    let file_mode = file_mode as u32;
    let path = read_path(path)?;
    let parent = lookup_parent(dirfd, &path)?;
    let credentials = credentials()?;
    let node = match file_mode & mode::TYPE {
        0 | mode::REGULAR => NewNode::Regular,
        mode::CHARACTER_DEVICE if credentials.is_root() => NewNode::CharacterDevice(device),
        mode::CHARACTER_DEVICE | mode::BLOCK_DEVICE => return Err(EPERM),
        mode::DIRECTORY | mode::SYMLINK => return Err(EINVAL),
        _ => return Err(crate::errno::EOPNOTSUPP),
    };
    let permissions = file_mode & mode::CHANGEABLE & !umask()?;
    parent
        .directory
        .create(&parent.name, node, permissions, &credentials)?;
    Ok(0)
}
