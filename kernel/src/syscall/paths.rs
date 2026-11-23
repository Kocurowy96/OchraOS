// SPDX-License-Identifier: GPL-2.0-only

//! Syscalls that take paths: opening, `stat`, `access`, the current directory, links, and
//! changing metadata (`chmod`, `chown`, `utimensat`, `truncate`). Directory entries are made
//! and removed in `names.rs`.
//!
//! The `…at` forms take a directory descriptor: a relative path starts there instead of in
//! the current directory (`AT_FDCWD`, -100, means the current directory). Every access
//! decision is `vfs::may_access`'s.

use alloc::string::String;
use alloc::sync::Arc;

use super::{SyscallResult, current, read_string, read_user, write_user};
use crate::errno::{
    EBADF, EEXIST, EFAULT, EINVAL, EISDIR, ELOOP, ENAMETOOLONG, ENOENT, ENOTDIR, EPERM, ERANGE,
    Errno,
};
use crate::file::{self, Descriptor, File, OpenFile, flags};
use crate::vfs::{
    self, Access, Credentials, DeviceNode, NewNode, PATH_MAX, Parent, Path, Timestamp, VfsFile,
    Walk, may_access, may_own, mode,
};

/// "Relative to the current directory" for the `…at` syscalls.
pub const AT_FDCWD: i32 = -100;
/// `…at` flags.
pub const AT_SYMLINK_NOFOLLOW: u64 = 0x100;
pub const AT_SYMLINK_FOLLOW: u64 = 0x400;
pub const AT_EMPTY_PATH: u64 = 0x1000;

/// Reads a path argument. Names in the VFS are UTF-8 strings for now, so a path that is not
/// cannot exist.
pub fn read_path(address: u64) -> Result<String, Errno> {
    let path = read_string(address, PATH_MAX - 1).map_err(|error| error.unwrap_or(ENAMETOOLONG))?;
    String::from_utf8(path).map_err(|_| ENOENT)
}

/// The calling process's credentials.
pub fn credentials() -> Result<Credentials, Errno> {
    Ok(current()?.credentials.lock().clone())
}

/// The directory a relative path starts from: the current one, or the directory `dirfd`.
/// An absolute path ignores `dirfd` entirely (even a bad one), as in Linux.
pub fn base_directory(dirfd: i32, path: &str) -> Result<Path, Errno> {
    if path.starts_with('/') {
        return Ok(vfs::root());
    }
    let process = current()?;
    if dirfd == AT_FDCWD {
        return Ok(process.cwd.lock().clone());
    }
    let fd = u64::try_from(dirfd).map_err(|_| EBADF)?;
    let file = process.files.lock().file(fd)?;
    file.file.directory().ok_or(ENOTDIR)
}

/// Looks up `path` from `dirfd`.
pub fn lookup(dirfd: i32, path: &str, follow: bool) -> Result<Path, Errno> {
    let walk = if follow { Walk::Follow } else { Walk::NoFollow };
    base_directory(dirfd, path)?.walk(path, walk, &credentials()?)
}

/// Looks up the directory holding the last component of `path`, from `dirfd`.
pub fn lookup_parent(dirfd: i32, path: &str) -> Result<Parent, Errno> {
    base_directory(dirfd, path)?.walk_parent(path, &credentials()?)
}

/// The place a descriptor's file was opened at (`fchmod`, `fchown`, `futimens`).
fn descriptor_path(fd: u64) -> Result<Path, Errno> {
    let file = current()?.files.lock().file(fd)?;
    // Pipes and the console of init are in no filesystem here.
    file.file.path().ok_or(EINVAL)
}

/// Looks up `path` from `dirfd`, or with `AT_EMPTY_PATH` and an empty path, `dirfd`
/// itself (the current directory for `AT_FDCWD`).
fn lookup_or_descriptor(
    dirfd: i32,
    path: &str,
    at_flags: u64,
    follow: bool,
) -> Result<Path, Errno> {
    if path.is_empty() && at_flags & AT_EMPTY_PATH != 0 {
        if dirfd == AT_FDCWD {
            return Ok(current()?.cwd.lock().clone());
        }
        return descriptor_path(u64::try_from(dirfd).map_err(|_| EBADF)?);
    }
    lookup(dirfd, path, follow)
}

/// `open(pathname, flags, mode)`.
pub fn open(path: u64, open_flags: u64, mode: u64) -> SyscallResult {
    openat(AT_FDCWD, path, open_flags, mode)
}

/// `creat(pathname, mode)`: `open` with `O_CREAT | O_WRONLY | O_TRUNC`.
pub fn creat(path: u64, mode: u64) -> SyscallResult {
    openat(
        AT_FDCWD,
        path,
        u64::from(O_CREAT | flags::O_WRONLY | O_TRUNC),
        mode,
    )
}

const ACCESS_MODE: u32 = 3;
const O_CREAT: u32 = 0o100;
const O_EXCL: u32 = 0o200;
const O_TRUNC: u32 = 0o1000;
const O_DIRECTORY: u32 = 0o200_000;
const O_NOFOLLOW: u32 = 0o400_000;

/// `openat(dirfd, pathname, flags, mode)`.
pub fn openat(dirfd: i32, path: u64, open_flags: u64, permissions: u64) -> SyscallResult {
    let open_flags = open_flags as u32;
    let access_mode = open_flags & ACCESS_MODE;
    if access_mode == ACCESS_MODE {
        return Err(EINVAL);
    }
    let path = read_path(path)?;
    let process = current()?;
    let credentials = process.credentials.lock().clone();
    let (found, created) = if open_flags & O_CREAT != 0 {
        let umask = process.umask.load(core::sync::atomic::Ordering::Relaxed);
        let permissions = permissions as u32 & mode::CHANGEABLE & !umask;
        open_or_create(dirfd, &path, open_flags, permissions, &credentials)?
    } else {
        let follow = open_flags & O_NOFOLLOW == 0;
        (lookup(dirfd, &path, follow)?, false)
    };
    let metadata = found.metadata();
    if metadata.is_symlink() {
        // `O_NOFOLLOW` and the last component is a link.
        return Err(ELOOP);
    }
    if open_flags & O_DIRECTORY != 0 && !metadata.is_directory() {
        return Err(ENOTDIR);
    }
    let writes = access_mode != flags::O_RDONLY;
    if metadata.is_directory() && (writes || open_flags & O_TRUNC != 0) {
        return Err(EISDIR);
    }
    // Whoever creates a file may open it as asked, whatever its permission bits.
    if !created {
        let mut wanted = Access::NONE;
        if access_mode != flags::O_WRONLY {
            wanted = wanted.union(Access::READ);
        }
        if writes || open_flags & O_TRUNC != 0 {
            wanted = wanted.union(Access::WRITE);
        }
        may_access(&metadata, &credentials, wanted)?;
    }
    let file: Arc<dyn File> = match metadata.file_type() {
        mode::CHARACTER_DEVICE => {
            let device = file::open_device(metadata.rdev)?;
            Arc::new(DeviceNode::new(found, device, access_mode))
        }
        mode::REGULAR | mode::DIRECTORY => {
            if metadata.is_regular() && open_flags & O_TRUNC != 0 && metadata.size != 0 {
                vfs::truncate(&found, 0)?;
            }
            let append = open_flags & flags::O_APPEND != 0;
            Arc::new(VfsFile::new(found, access_mode, append))
        }
        // FIFOs and sockets cannot be made yet.
        _ => return Err(crate::errno::ENXIO),
    };
    let descriptor = Descriptor {
        file: OpenFile::new(file, open_flags),
        close_on_exec: open_flags & flags::O_CLOEXEC != 0,
    };
    process.files.lock().insert(descriptor, 0)
}

/// `O_CREAT`: the file at `path`, created if it is not there. Returns it and whether it was
/// created now.
fn open_or_create(
    dirfd: i32,
    path: &str,
    open_flags: u32,
    permissions: u32,
    credentials: &Credentials,
) -> Result<(Path, bool), Errno> {
    let parent = lookup_parent(dirfd, path)?;
    if parent.trailing_slash || parent.name == "." || parent.name == ".." {
        return Err(EISDIR);
    }
    let directory = &parent.directory;
    // Twice: someone else may create the name between our look and our create.
    for _ in 0..2 {
        match directory.walk(&parent.name, Walk::NoFollow, credentials) {
            Ok(_) if open_flags & O_EXCL != 0 => return Err(EEXIST),
            Ok(existing) if existing.metadata().is_symlink() && open_flags & O_NOFOLLOW == 0 => {
                // An existing link is followed (a dangling one is not created through).
                return Ok((
                    directory.walk(&parent.name, Walk::Follow, credentials)?,
                    false,
                ));
            }
            Ok(existing) => return Ok((existing, false)),
            Err(ENOENT) => {}
            Err(error) => return Err(error),
        }
        match directory.create(&parent.name, NewNode::Regular, permissions, credentials) {
            Ok(created) => return Ok((created, true)),
            Err(EEXIST) if open_flags & O_EXCL == 0 => continue,
            Err(error) => return Err(error),
        }
    }
    Err(EEXIST)
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
    if path.is_empty() && at_flags & AT_EMPTY_PATH != 0 && dirfd != AT_FDCWD {
        return super::files::fstat(u64::try_from(dirfd).map_err(|_| EBADF)?, buffer);
    }
    let found = lookup_or_descriptor(dirfd, &path, at_flags, at_flags & AT_SYMLINK_NOFOLLOW == 0)?;
    write_user(buffer, &found.metadata().to_stat())?;
    Ok(0)
}

/// `access(pathname, mode)`.
pub fn access(path: u64, mode: u64) -> SyscallResult {
    faccessat2(AT_FDCWD, path, mode, 0)
}

/// `faccessat2(dirfd, pathname, mode, flags)` (and `faccessat`, without flags): whether the
/// caller may read, write or execute the file (`F_OK`, 0: whether it exists). The real IDs
/// are used unless `AT_EACCESS`; they are the same as the effective ones for now.
pub fn faccessat2(dirfd: i32, path: u64, mode: u64, at_flags: u64) -> SyscallResult {
    /// Check with the effective IDs.
    const AT_EACCESS: u64 = 0x200;
    if mode & !7 != 0 || at_flags & !(AT_EACCESS | AT_SYMLINK_NOFOLLOW | AT_EMPTY_PATH) != 0 {
        return Err(EINVAL);
    }
    let path = read_path(path)?;
    let found = lookup_or_descriptor(dirfd, &path, at_flags, at_flags & AT_SYMLINK_NOFOLLOW == 0)?;
    may_access(
        &found.metadata(),
        &credentials()?,
        Access::from_bits(mode as u32),
    )?;
    Ok(0)
}

/// `getcwd(buf, size)`: the path with its NUL; returns its length (with the NUL).
pub fn getcwd(buffer: u64, size: u64) -> SyscallResult {
    let cwd = current()?.cwd.lock().clone();
    let mut path = cwd.absolute_name()?.into_bytes();
    path.push(0);
    if (path.len() as u64) > size {
        return Err(ERANGE);
    }
    write_user(buffer, &path)?;
    Ok(path.len() as u64)
}

/// Makes `path` the current directory: a directory the caller may search.
fn change_directory(path: Path) -> SyscallResult {
    let metadata = path.metadata();
    if !metadata.is_directory() {
        return Err(ENOTDIR);
    }
    let process = current()?;
    may_access(&metadata, &process.credentials.lock(), Access::EXECUTE)?;
    *process.cwd.lock() = path;
    Ok(0)
}

/// `chdir(path)`.
pub fn chdir(path: u64) -> SyscallResult {
    let path = read_path(path)?;
    change_directory(lookup(AT_FDCWD, &path, true)?)
}

/// `fchdir(fd)`.
pub fn fchdir(fd: u64) -> SyscallResult {
    let file = current()?.files.lock().file(fd)?;
    change_directory(file.file.directory().ok_or(ENOTDIR)?)
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
    if !found.metadata().is_symlink() {
        return Err(EINVAL);
    }
    let target = found.inode().readlink()?;
    let count = target.len().min(size as usize);
    write_user(buffer, &target.as_bytes()[..count])?;
    vfs::touch_accessed(&found);
    Ok(count as u64)
}

/// Sets the permission bits of the file at `path`: its owner or root only.
fn change_mode(path: &Path, permissions: u64) -> SyscallResult {
    may_own(&path.metadata(), &credentials()?)?;
    let now = Timestamp::now();
    path.inode().set_metadata(&mut |metadata| {
        metadata.mode = (metadata.mode & mode::TYPE) | (permissions as u32 & mode::CHANGEABLE);
        metadata.changed = now;
    })?;
    Ok(0)
}

/// `chmod(pathname, mode)`.
pub fn chmod(path: u64, permissions: u64) -> SyscallResult {
    fchmodat(AT_FDCWD, path, permissions)
}

/// `fchmod(fd, mode)`.
pub fn fchmod(fd: u64, permissions: u64) -> SyscallResult {
    change_mode(&descriptor_path(fd)?, permissions)
}

/// `fchmodat(dirfd, pathname, mode)`: the kernel's form has no flags (the C library handles
/// `AT_SYMLINK_NOFOLLOW` itself).
pub fn fchmodat(dirfd: i32, path: u64, permissions: u64) -> SyscallResult {
    let path = read_path(path)?;
    change_mode(&lookup(dirfd, &path, true)?, permissions)
}

/// Changes the owner and group of the file at `path`; -1 keeps one. Only root may give a
/// file away; an owner may set its group to one of its own groups (`man 2 chown`).
fn change_owner(path: &Path, user: u64, group: u64) -> SyscallResult {
    let user = (user as u32 != u32::MAX).then_some(user as u32);
    let group = (group as u32 != u32::MAX).then_some(group as u32);
    let metadata = path.metadata();
    let credentials = credentials()?;
    if !credentials.is_root() {
        let owns = credentials.effective_user == metadata.user;
        let keeps_user = user.is_none_or(|user| user == metadata.user);
        let own_group = group.is_none_or(|group| {
            group == metadata.group
                || group == credentials.effective_group
                || credentials.groups.contains(&group)
        });
        if !owns || !keeps_user || !own_group {
            return Err(EPERM);
        }
    }
    if user.is_none() && group.is_none() {
        return Ok(0);
    }
    let now = Timestamp::now();
    path.inode().set_metadata(&mut |metadata| {
        metadata.user = user.unwrap_or(metadata.user);
        metadata.group = group.unwrap_or(metadata.group);
        // A new owner must not inherit the old one's set-ID powers: Linux clears set-user-ID,
        // and set-group-ID if the group may execute (otherwise it marks mandatory locking).
        if !metadata.is_directory() {
            metadata.mode &= !mode::SET_UID;
            if metadata.mode & 0o010 != 0 {
                metadata.mode &= !mode::SET_GID;
            }
        }
        metadata.changed = now;
    })?;
    Ok(0)
}

/// `chown(pathname, owner, group)`.
pub fn chown(path: u64, user: u64, group: u64) -> SyscallResult {
    fchownat(AT_FDCWD, path, user, group, 0)
}

/// `lchown(pathname, owner, group)`: a link itself.
pub fn lchown(path: u64, user: u64, group: u64) -> SyscallResult {
    fchownat(AT_FDCWD, path, user, group, AT_SYMLINK_NOFOLLOW)
}

/// `fchown(fd, owner, group)`.
pub fn fchown(fd: u64, user: u64, group: u64) -> SyscallResult {
    change_owner(&descriptor_path(fd)?, user, group)
}

/// `fchownat(dirfd, pathname, owner, group, flags)`.
pub fn fchownat(dirfd: i32, path: u64, user: u64, group: u64, at_flags: u64) -> SyscallResult {
    if at_flags & !(AT_SYMLINK_NOFOLLOW | AT_EMPTY_PATH) != 0 {
        return Err(EINVAL);
    }
    let path = read_path(path)?;
    let found = lookup_or_descriptor(dirfd, &path, at_flags, at_flags & AT_SYMLINK_NOFOLLOW == 0)?;
    change_owner(&found, user, group)
}

/// `umask(mask)`: sets the mask for new files; returns the old one.
pub fn umask(mask: u64) -> SyscallResult {
    let old = current()?
        .umask
        .swap(mask as u32 & 0o777, core::sync::atomic::Ordering::Relaxed);
    Ok(u64::from(old))
}

/// `utimensat(dirfd, pathname, times, flags)`: sets the access and modification times from
/// two `struct timespec` (now for both if `times` is NULL). A NULL path means `dirfd`
/// itself (how the C library makes `futimens`).
pub fn utimensat(dirfd: i32, path: u64, times: u64, at_flags: u64) -> SyscallResult {
    const UTIME_NOW: u64 = (1 << 30) - 1;
    const UTIME_OMIT: u64 = (1 << 30) - 2;
    if at_flags & !(AT_SYMLINK_NOFOLLOW | AT_EMPTY_PATH) != 0 {
        return Err(EINVAL);
    }
    // `None` keeps a time; `Some(None)` sets it to now.
    let mut wanted: [Option<Option<Timestamp>>; 2] = [Some(None), Some(None)];
    if times != 0 {
        let mut bytes = [0; 32];
        read_user(&mut bytes, times)?;
        for (index, slot) in wanted.iter_mut().enumerate() {
            let word =
                |at: usize| u64::from_le_bytes(bytes[at..at + 8].try_into().expect("8 bytes"));
            let (seconds, nanoseconds) = (word(index * 16), word(index * 16 + 8));
            *slot = match nanoseconds {
                UTIME_NOW => Some(None),
                UTIME_OMIT => None,
                0..=999_999_999 if (seconds as i64) >= 0 => Some(Some(Timestamp {
                    seconds,
                    nanoseconds: nanoseconds as u32,
                })),
                _ => return Err(EINVAL),
            };
        }
    }
    let found = if path == 0 {
        if dirfd == AT_FDCWD {
            // Linux's answer for a NULL path without a descriptor.
            return Err(EFAULT);
        }
        descriptor_path(u64::try_from(dirfd).map_err(|_| EBADF)?)?
    } else {
        let path = read_path(path)?;
        lookup_or_descriptor(dirfd, &path, at_flags, at_flags & AT_SYMLINK_NOFOLLOW == 0)?
    };
    if wanted == [None, None] {
        return Ok(0);
    }
    // Setting a time to "now" needs only write access; any other time, ownership.
    let metadata = found.metadata();
    let credentials = credentials()?;
    let explicit = wanted.iter().any(|time| matches!(time, Some(Some(_))));
    if explicit || may_access(&metadata, &credentials, Access::WRITE).is_err() {
        may_own(&metadata, &credentials)?;
    }
    let now = Timestamp::now();
    found.inode().set_metadata(&mut |metadata| {
        if let Some(time) = wanted[0] {
            metadata.accessed = time.unwrap_or(now);
        }
        if let Some(time) = wanted[1] {
            metadata.modified = time.unwrap_or(now);
        }
        metadata.changed = now;
    })?;
    Ok(0)
}

/// `truncate(path, length)`: needs write access to the file.
pub fn truncate(path: u64, length: u64) -> SyscallResult {
    if (length as i64) < 0 {
        return Err(EINVAL);
    }
    let path = read_path(path)?;
    let found = lookup(AT_FDCWD, &path, true)?;
    let metadata = found.metadata();
    if metadata.is_directory() {
        return Err(EISDIR);
    }
    if !metadata.is_regular() {
        return Err(EINVAL);
    }
    may_access(&metadata, &credentials()?, Access::WRITE)?;
    vfs::truncate(&found, length)?;
    Ok(0)
}
