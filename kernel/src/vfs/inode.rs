// SPDX-License-Identifier: GPL-2.0-only

//! Inodes: what every filesystem implements.
//!
//! The VFS checks permissions and arguments before it calls these methods (the target of a
//! `create` does not exist, a directory given to `rmdir` is a directory, the caller may
//! write…), so a filesystem only stores and fetches. Each method is atomic for its
//! filesystem: two `create`s of one name never both succeed.

use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::any::Any;

use ochra_frame::mm::frames::Frame;

use super::mode;
use super::{PageCache, Path};
use crate::errno::{EINVAL, ENOTDIR, EPERM, Errno};
use crate::file::File;

/// A point in time: seconds and nanoseconds since 1970 (UTC).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord)]
pub struct Timestamp {
    pub seconds: u64,
    pub nanoseconds: u32,
}

impl Timestamp {
    pub fn now() -> Self {
        Self::from_nanoseconds(crate::time::realtime())
    }

    pub fn from_nanoseconds(nanoseconds: u64) -> Self {
        let second = crate::time::NANOSECONDS_PER_SECOND;
        Self {
            seconds: nanoseconds / second,
            nanoseconds: (nanoseconds % second) as u32,
        }
    }
}

/// What `stat` reports about an inode (`struct stat`).
#[derive(Clone, Copy, Debug, Default)]
pub struct Metadata {
    /// Inode number (`st_ino`), unique within the filesystem.
    pub inode: u64,
    /// The filesystem's device number (`st_dev`).
    pub device: u64,
    /// Type and permission bits.
    pub mode: u32,
    pub user: u32,
    pub group: u32,
    pub links: u32,
    pub size: u64,
    /// For a device node: the device's number.
    pub rdev: u64,
    pub accessed: Timestamp,
    pub modified: Timestamp,
    /// Last change of the inode itself (metadata or contents).
    pub changed: Timestamp,
}

impl Metadata {
    pub fn file_type(&self) -> u32 {
        self.mode & mode::TYPE
    }

    pub fn is_directory(&self) -> bool {
        self.file_type() == mode::DIRECTORY
    }

    pub fn is_regular(&self) -> bool {
        self.file_type() == mode::REGULAR
    }

    pub fn is_symlink(&self) -> bool {
        self.file_type() == mode::SYMLINK
    }

    /// `struct stat` on x86-64: 144 bytes.
    pub fn to_stat(self) -> [u8; 144] {
        /// The block size `st_blksize` suggests for I/O.
        const BLOCK: u64 = 4096;
        let mut bytes = [0; 144];
        let mut put = |offset: usize, value: u64| {
            bytes[offset..offset + 8].copy_from_slice(&value.to_le_bytes());
        };
        put(0, self.device);
        put(8, self.inode);
        put(16, u64::from(self.links));
        put(40, self.rdev);
        put(48, self.size);
        put(56, BLOCK);
        put(64, self.size.div_ceil(512)); // st_blocks, in 512-byte units
        for (offset, time) in [
            (72, self.accessed),
            (88, self.modified),
            (104, self.changed),
        ] {
            put(offset, time.seconds);
            put(offset + 8, u64::from(time.nanoseconds));
        }
        bytes[24..28].copy_from_slice(&self.mode.to_le_bytes());
        bytes[28..32].copy_from_slice(&self.user.to_le_bytes());
        bytes[32..36].copy_from_slice(&self.group.to_le_bytes());
        bytes
    }

    /// The `d_type` of a directory entry for this type (`DT_*`: the type bits shifted down).
    pub fn directory_type(&self) -> u8 {
        (self.file_type() >> 12) as u8
    }
}

/// What a directory operation creates.
#[derive(Clone, Debug)]
pub enum NewNode {
    Regular,
    Directory,
    Symlink(String),
    /// A character device node with this device number.
    CharacterDevice(u64),
}

/// One entry from [`Inode::read_directory`].
#[derive(Clone, Debug)]
pub struct DirectoryEntry {
    /// Where a listing continues after this entry (`d_off`); greater than every earlier
    /// entry's and never 0, 1 or 2 (those are the start, `.` and `..`).
    pub cookie: u64,
    pub inode: u64,
    /// `DT_*` type.
    pub kind: u8,
    pub name: String,
}

/// A file, directory, link or device node of some filesystem.
pub trait Inode: Send + Sync {
    fn metadata(&self) -> Metadata;

    /// Changes the metadata (permissions, owner, times, size after a write…). The filesystem
    /// keeps the change; it may refuse fields it cannot store.
    fn set_metadata(&self, change: &mut dyn FnMut(&mut Metadata)) -> Result<(), Errno>;

    /// For downcasting between inodes of one filesystem (`rename`, `link`).
    fn as_any(&self) -> &dyn Any;

    /// The inode named `name` in this directory.
    fn lookup(&self, _name: &str) -> Result<Arc<dyn Inode>, Errno> {
        Err(ENOTDIR)
    }

    /// Whether the VFS may keep this directory's entries in its dentry cache. A filesystem
    /// whose entries come and go by themselves (a process in `/proc`, a pty in `/dev/pts`)
    /// says no, and every walk asks it again (Linux's `d_revalidate`).
    fn caches_lookups(&self) -> bool {
        true
    }

    /// Creates `name` in this directory, with the given mode bits (permissions; the type comes
    /// from `node`) and owner.
    fn create(
        &self,
        _name: &str,
        _node: NewNode,
        _permissions: u32,
        _owner: (u32, u32),
    ) -> Result<Arc<dyn Inode>, Errno> {
        Err(ENOTDIR)
    }

    /// Adds `name` in this directory for the existing (non-directory) `inode`.
    fn link(&self, _name: &str, _inode: &Arc<dyn Inode>) -> Result<(), Errno> {
        Err(EPERM)
    }

    /// Removes the entry `name` (not a directory) from this directory.
    fn unlink(&self, _name: &str) -> Result<(), Errno> {
        Err(ENOTDIR)
    }

    /// Removes the empty directory `name` (`ENOTEMPTY` otherwise).
    fn rmdir(&self, _name: &str) -> Result<(), Errno> {
        Err(ENOTDIR)
    }

    /// Moves `old` in this directory to `new` in `new_parent` (a directory of the same
    /// filesystem), replacing what is there (the VFS has checked that may be replaced).
    fn rename(&self, _old: &str, _new_parent: &Arc<dyn Inode>, _new: &str) -> Result<(), Errno> {
        Err(ENOTDIR)
    }

    /// The entries after the one with cookie `after` (0: from the start), without `.` and
    /// `..`, in cookie order.
    fn read_directory(&self, _after: u64) -> Result<Vec<DirectoryEntry>, Errno> {
        Err(ENOTDIR)
    }

    /// A symbolic link's target.
    fn readlink(&self) -> Result<String, Errno> {
        Err(EINVAL)
    }

    /// For filesystems that make their own open files (procfs): the file for an `open` of
    /// this inode at `path`. `None` (the default) lets the VFS open it as usual.
    fn open(&self, _path: &Path, _access_mode: u32) -> Option<Result<Arc<dyn File>, Errno>> {
        None
    }

    /// A regular file's cached pages.
    fn page_cache(&self) -> Option<&PageCache> {
        None
    }

    /// Fills `frame` with page `index` of the file, for the page cache. Without a backing
    /// store (tmpfs), a page that is not cached is a hole: zeros.
    fn read_page(&self, _index: u64, frame: &mut Frame) -> Result<(), Errno> {
        frame.zero();
        Ok(())
    }
}
