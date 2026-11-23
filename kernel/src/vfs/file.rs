// SPDX-License-Identifier: GPL-2.0-only

//! Open files of the VFS: [`VfsFile`] (a regular file or a directory, with its own position)
//! and [`DeviceNode`] (a device opened through its node in the tree).

use alloc::sync::Arc;
use alloc::vec::Vec;

use super::{Metadata, Path, Timestamp};
use crate::errno::{EBADF, EFBIG, EINVAL, EISDIR, Errno};
use crate::file::{File, flags};
use crate::sched::Mutex;

/// The largest file size (Linux's `MAX_LFS_FILESIZE` on 64-bit).
const MAX_FILE_SIZE: u64 = i64::MAX as u64;

/// `lseek`'s `whence`.
const SEEK_SET: u32 = 0;
const SEEK_CUR: u32 = 1;
const SEEK_END: u32 = 2;

/// A regular file or a directory, opened. Each `open` makes a new one with its own position;
/// `dup` and `fork` share it (through the `OpenFile`).
pub struct VfsFile {
    path: Path,
    access_mode: u32,
    append: bool,
    /// For a regular file, the byte offset; for a directory, the cookie where a listing
    /// continues (0 at the start, 1 after `.`, 2 after `..`).
    position: Mutex<u64>,
}

impl VfsFile {
    pub fn new(path: Path, access_mode: u32, append: bool) -> Self {
        Self {
            path,
            access_mode,
            append,
            position: Mutex::new(0),
        }
    }

    fn readable(&self) -> bool {
        self.access_mode != flags::O_WRONLY
    }

    fn writable(&self) -> bool {
        self.access_mode != flags::O_RDONLY
    }

    /// Reads at `offset`, for `read` and `pread64`.
    fn read_from(&self, buffer: &mut [u8], offset: u64) -> Result<usize, Errno> {
        if !self.readable() {
            return Err(EBADF);
        }
        let metadata = self.path.metadata();
        if metadata.is_directory() {
            return Err(EISDIR);
        }
        let inode = self.path.inode();
        let cache = inode.page_cache().ok_or(EINVAL)?;
        let count = cache.read(inode.as_ref(), offset, buffer, metadata.size)?;
        touch_accessed(&self.path);
        Ok(count)
    }

    /// Writes at `offset` (at the end with `O_APPEND`); returns the count and where the
    /// write ended.
    fn write_to(&self, data: &[u8], offset: u64) -> Result<(usize, u64), Errno> {
        if !self.writable() {
            return Err(EBADF);
        }
        let inode = self.path.inode();
        let cache = inode.page_cache().ok_or(EINVAL)?;
        let _writes = cache.lock_writes();
        let offset = if self.append {
            inode.metadata().size
        } else {
            offset
        };
        let room = MAX_FILE_SIZE.saturating_sub(offset);
        if room == 0 && !data.is_empty() {
            return Err(EFBIG);
        }
        let data = &data[..data.len().min(room as usize)];
        let count = cache.write(inode.as_ref(), offset, data)?;
        let end = offset + count as u64;
        let now = Timestamp::now();
        inode.set_metadata(&mut |metadata| {
            metadata.size = metadata.size.max(end);
            metadata.modified = now;
            metadata.changed = now;
        })?;
        Ok((count, end))
    }
}

/// Updates the access time as Linux's `relatime` does: only if it is not newer than the last
/// modification or change, or is a day old. That keeps "was this read since it changed?"
/// working (mail readers ask it) without a write for every read.
pub fn touch_accessed(path: &Path) {
    const DAY: u64 = 24 * 60 * 60;
    let metadata = path.metadata();
    let now = Timestamp::now();
    let stale = metadata.accessed <= metadata.modified
        || metadata.accessed <= metadata.changed
        || metadata.accessed.seconds + DAY <= now.seconds;
    if stale {
        let _ = path
            .inode()
            .set_metadata(&mut |metadata| metadata.accessed = now);
    }
}

/// Size of a `linux_dirent64` record with a name of `length` bytes: the fixed fields (19
/// bytes), the name and its NUL, rounded up to 8.
fn record_size(length: usize) -> usize {
    (19 + length + 1).next_multiple_of(8)
}

/// Appends one `linux_dirent64` record (`man 2 getdents64`): `d_ino`, `d_off` (the cookie
/// where the next call continues), `d_reclen`, `d_type`, the name with a NUL.
fn put_record(records: &mut Vec<u8>, inode: u64, cookie: u64, kind: u8, name: &str) {
    let size = record_size(name.len());
    let start = records.len();
    records.extend_from_slice(&inode.to_le_bytes());
    records.extend_from_slice(&cookie.to_le_bytes());
    records.extend_from_slice(&(size as u16).to_le_bytes());
    records.push(kind);
    records.extend_from_slice(name.as_bytes());
    records.resize(start + size, 0);
}

/// `DT_DIR`.
const DIRECTORY_TYPE: u8 = 4;

impl File for VfsFile {
    fn read(&self, buffer: &mut [u8], _nonblocking: bool) -> Result<usize, Errno> {
        let mut position = self.position.lock();
        let count = self.read_from(buffer, *position)?;
        *position += count as u64;
        Ok(count)
    }

    fn read_at(&self, buffer: &mut [u8], offset: u64) -> Result<usize, Errno> {
        self.read_from(buffer, offset)
    }

    fn write(&self, data: &[u8], _nonblocking: bool) -> Result<usize, Errno> {
        let mut position = self.position.lock();
        let (count, end) = self.write_to(data, *position)?;
        *position = end;
        Ok(count)
    }

    fn write_at(&self, data: &[u8], offset: u64) -> Result<usize, Errno> {
        // With `O_APPEND`, Linux appends even here (`man 2 pwrite`, BUGS).
        self.write_to(data, offset).map(|(count, _)| count)
    }

    fn access_mode(&self) -> u32 {
        self.access_mode
    }

    fn append(&self) -> bool {
        self.append
    }

    fn stat(&self) -> Metadata {
        self.path.metadata()
    }

    fn path(&self) -> Option<Path> {
        Some(self.path.clone())
    }

    /// The position may go past the end of a file (reads there give nothing), not below 0.
    /// In a directory it is a cookie: only `SEEK_SET` and `SEEK_CUR` make sense, as tmpfs.
    fn seek(&self, offset: i64, whence: u32) -> Result<u64, Errno> {
        let mut position = self.position.lock();
        let directory = self.path.metadata().is_directory();
        let base = match whence {
            SEEK_SET => 0,
            SEEK_CUR => *position,
            SEEK_END if !directory => self.path.metadata().size,
            _ => return Err(EINVAL),
        };
        let new = base
            .checked_add_signed(offset)
            .filter(|&new| new <= MAX_FILE_SIZE)
            .ok_or(EINVAL)?;
        *position = new;
        Ok(new)
    }

    fn directory(&self) -> Option<Path> {
        self.path
            .metadata()
            .is_directory()
            .then(|| self.path.clone())
    }

    /// `getdents64`: as many records as fit in `size` bytes, from the position on; `.` and
    /// `..` first. `EINVAL` if not even one fits.
    fn read_directory(&self, size: usize) -> Result<Vec<u8>, Errno> {
        let metadata = self.path.metadata();
        if !metadata.is_directory() {
            return Err(crate::errno::ENOTDIR);
        }
        let mut position = self.position.lock();
        let mut records = Vec::new();
        let fits = |records: &Vec<u8>, name: &str| {
            let room = records.len() + record_size(name.len()) <= size;
            if !room && records.is_empty() {
                Err(EINVAL)
            } else {
                Ok(room)
            }
        };
        if *position == 0 {
            if !fits(&records, ".")? {
                return Ok(records);
            }
            put_record(&mut records, metadata.inode, 1, DIRECTORY_TYPE, ".");
            *position = 1;
        }
        if *position == 1 {
            if !fits(&records, "..")? {
                return Ok(records);
            }
            let parent = self.path.parent().metadata().inode;
            put_record(&mut records, parent, 2, DIRECTORY_TYPE, "..");
            *position = 2;
        }
        for entry in self.path.inode().read_directory(*position)? {
            if !fits(&records, &entry.name)? {
                break;
            }
            put_record(
                &mut records,
                entry.inode,
                entry.cookie,
                entry.kind,
                &entry.name,
            );
            *position = entry.cookie;
        }
        drop(position);
        touch_accessed(&self.path);
        Ok(records)
    }

    fn truncate(&self, size: u64) -> Result<(), Errno> {
        if !self.writable() {
            return Err(EINVAL);
        }
        truncate(&self.path, size)
    }
}

/// Sets the size of the regular file at `path` (`truncate`, `ftruncate`, `O_TRUNC`); the
/// caller has checked the permission.
pub fn truncate(path: &Path, size: u64) -> Result<(), Errno> {
    let metadata = path.metadata();
    if metadata.is_directory() {
        return Err(EISDIR);
    }
    let inode = path.inode();
    let cache = inode.page_cache().ok_or(EINVAL)?;
    if size > MAX_FILE_SIZE {
        return Err(EFBIG);
    }
    let _writes = cache.lock_writes();
    cache.truncate(size);
    let now = Timestamp::now();
    inode.set_metadata(&mut |metadata| {
        metadata.size = size;
        metadata.modified = now;
        metadata.changed = now;
    })
}

/// A device opened through its node: reads and writes go to the device, `fstat` and
/// `fchmod` to the node.
pub struct DeviceNode {
    path: Path,
    device: Arc<dyn File>,
    access_mode: u32,
}

impl DeviceNode {
    pub fn new(path: Path, device: Arc<dyn File>, access_mode: u32) -> Self {
        Self {
            path,
            device,
            access_mode,
        }
    }
}

impl File for DeviceNode {
    fn read(&self, buffer: &mut [u8], nonblocking: bool) -> Result<usize, Errno> {
        if self.access_mode == flags::O_WRONLY {
            return Err(EBADF);
        }
        self.device.read(buffer, nonblocking)
    }

    fn write(&self, data: &[u8], nonblocking: bool) -> Result<usize, Errno> {
        if self.access_mode == flags::O_RDONLY {
            return Err(EBADF);
        }
        self.device.write(data, nonblocking)
    }

    fn access_mode(&self) -> u32 {
        self.access_mode
    }

    fn is_terminal(&self) -> bool {
        self.device.is_terminal()
    }

    fn stat(&self) -> Metadata {
        self.path.metadata()
    }

    fn path(&self) -> Option<Path> {
        Some(self.path.clone())
    }

    fn seek(&self, offset: i64, whence: u32) -> Result<u64, Errno> {
        self.device.seek(offset, whence)
    }
}
