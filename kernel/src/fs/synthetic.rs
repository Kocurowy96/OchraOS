// SPDX-License-Identifier: GPL-2.0-only

//! Filesystems the kernel makes up as they are read (procfs, sysfs, devpts): nothing is
//! stored. A file's contents are generated when it is opened (as Linux's `seq_file` does on
//! the first read), a link's target when it is read, and a directory's entries when they are
//! listed or looked up. Everything is read-only.

use alloc::boxed::Box;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::any::Any;

use crate::errno::{EINVAL, ENOENT, EPERM, Errno};
use crate::file::{File, flags};
use crate::sched::Mutex;
use crate::vfs::{DirectoryEntry, Inode, InodeRef, Metadata, Path, Timestamp, mode};

/// Makes a file's contents.
pub type Contents = Box<dyn Fn() -> Result<Vec<u8>, Errno> + Send + Sync>;
/// Makes a link's target.
pub type Target = Box<dyn Fn() -> Result<String, Errno> + Send + Sync>;

/// One entry of a directory listing; `cookie` is where `getdents64` continues after it
/// (increasing, never 0, 1 or 2).
pub struct Entry {
    pub cookie: u64,
    pub name: String,
    pub inode: InodeRef,
}

/// What a synthetic directory holds.
pub trait Listing: Send + Sync {
    /// Every entry, in cookie order.
    fn entries(&self) -> Result<Vec<Entry>, Errno>;

    /// The entry `name`; by default found in [`entries`](Self::entries).
    fn lookup(&self, name: &str) -> Result<InodeRef, Errno> {
        self.entries()?
            .into_iter()
            .find(|entry| entry.name == name)
            .map(|entry| entry.inode)
            .ok_or(ENOENT)
    }
}

/// A fixed list of entries made on demand: each name with a function making its inode.
pub type Maker = fn() -> InodeRef;

impl Listing for &'static [(&'static str, Maker)] {
    fn entries(&self) -> Result<Vec<Entry>, Errno> {
        Ok(self
            .iter()
            .enumerate()
            .map(|(index, &(name, make))| Entry {
                cookie: index as u64 + 3,
                name: String::from(name),
                inode: make(),
            })
            .collect())
    }

    fn lookup(&self, name: &str) -> Result<InodeRef, Errno> {
        self.iter()
            .find(|(entry, _)| *entry == name)
            .map(|(_, make)| make())
            .ok_or(ENOENT)
    }
}

enum Kind {
    File(Contents),
    Symlink(Target),
    Directory(Box<dyn Listing>),
    /// A character device node: all in the metadata (`rdev`).
    Device,
}

/// An inode of a synthetic filesystem.
pub struct Node {
    metadata: Metadata,
    kind: Kind,
}

/// Who owns a node, and its permission bits.
#[derive(Clone, Copy)]
pub struct Attributes {
    /// The filesystem's device number.
    pub device: u64,
    pub inode: u64,
    pub permissions: u32,
    pub owner: (u32, u32),
}

fn node(attributes: Attributes, file_type: u32, rdev: u64, kind: Kind) -> InodeRef {
    let now = Timestamp::now();
    let links = if file_type == mode::DIRECTORY { 2 } else { 1 };
    Arc::new(Node {
        metadata: Metadata {
            inode: attributes.inode,
            device: attributes.device,
            mode: file_type | attributes.permissions,
            user: attributes.owner.0,
            group: attributes.owner.1,
            links,
            // As in Linux's procfs: generated files report size 0.
            size: 0,
            rdev,
            accessed: now,
            modified: now,
            changed: now,
        },
        kind,
    })
}

pub fn file(attributes: Attributes, contents: Contents) -> InodeRef {
    node(attributes, mode::REGULAR, 0, Kind::File(contents))
}

pub fn symlink(attributes: Attributes, target: Target) -> InodeRef {
    node(attributes, mode::SYMLINK, 0, Kind::Symlink(target))
}

pub fn directory(attributes: Attributes, listing: Box<dyn Listing>) -> InodeRef {
    node(attributes, mode::DIRECTORY, 0, Kind::Directory(listing))
}

pub fn character_device(attributes: Attributes, rdev: u64) -> InodeRef {
    node(attributes, mode::CHARACTER_DEVICE, rdev, Kind::Device)
}

impl Node {
    fn listing(&self) -> Result<&dyn Listing, Errno> {
        match &self.kind {
            Kind::Directory(listing) => Ok(listing.as_ref()),
            _ => Err(crate::errno::ENOTDIR),
        }
    }
}

impl Inode for Node {
    fn metadata(&self) -> Metadata {
        self.metadata
    }

    /// Nothing is stored, so nothing can be changed.
    fn set_metadata(&self, _change: &mut dyn FnMut(&mut Metadata)) -> Result<(), Errno> {
        Err(EPERM)
    }

    fn as_any(&self) -> &dyn Any {
        self
    }

    fn lookup(&self, name: &str) -> Result<InodeRef, Errno> {
        self.listing()?.lookup(name)
    }

    /// Entries appear and disappear with processes and pseudo-terminals: never cached.
    fn caches_lookups(&self) -> bool {
        false
    }

    fn read_directory(&self, after: u64) -> Result<Vec<DirectoryEntry>, Errno> {
        Ok(self
            .listing()?
            .entries()?
            .into_iter()
            .filter(|entry| entry.cookie > after)
            .map(|entry| {
                let metadata = entry.inode.metadata();
                DirectoryEntry {
                    cookie: entry.cookie,
                    inode: metadata.inode,
                    kind: metadata.directory_type(),
                    name: entry.name,
                }
            })
            .collect())
    }

    fn readlink(&self) -> Result<String, Errno> {
        match &self.kind {
            Kind::Symlink(target) => target(),
            _ => Err(EINVAL),
        }
    }

    fn open(&self, path: &Path, _access_mode: u32) -> Option<Result<Arc<dyn File>, Errno>> {
        let Kind::File(contents) = &self.kind else {
            return None;
        };
        Some(contents().map(|contents| {
            Arc::new(GeneratedFile {
                path: path.clone(),
                contents,
                position: Mutex::new(0),
            }) as Arc<dyn File>
        }))
    }
}

/// An open synthetic file: the contents made at `open`, read like a regular file.
struct GeneratedFile {
    path: Path,
    contents: Vec<u8>,
    position: Mutex<u64>,
}

impl GeneratedFile {
    fn read_from(&self, buffer: &mut [u8], offset: u64) -> usize {
        let start = usize::try_from(offset)
            .unwrap_or(usize::MAX)
            .min(self.contents.len());
        let count = buffer.len().min(self.contents.len() - start);
        buffer[..count].copy_from_slice(&self.contents[start..start + count]);
        count
    }
}

impl File for GeneratedFile {
    fn read(&self, buffer: &mut [u8], _nonblocking: bool) -> Result<usize, Errno> {
        let mut position = self.position.lock();
        let count = self.read_from(buffer, *position);
        *position += count as u64;
        Ok(count)
    }

    fn read_at(&self, buffer: &mut [u8], offset: u64) -> Result<usize, Errno> {
        Ok(self.read_from(buffer, offset))
    }

    /// Generated files have no way to take writes (Linux: `EINVAL` without a `write`
    /// operation).
    fn write(&self, _data: &[u8], _nonblocking: bool) -> Result<usize, Errno> {
        Err(EINVAL)
    }

    fn access_mode(&self) -> u32 {
        flags::O_RDONLY
    }

    fn stat(&self) -> Metadata {
        self.path.metadata()
    }

    fn path(&self) -> Option<Path> {
        Some(self.path.clone())
    }

    fn seek(&self, offset: i64, whence: u32) -> Result<u64, Errno> {
        const SEEK_SET: u32 = 0;
        const SEEK_CUR: u32 = 1;
        const SEEK_END: u32 = 2;
        let mut position = self.position.lock();
        let base = match whence {
            SEEK_SET => 0,
            SEEK_CUR => *position,
            SEEK_END => self.contents.len() as u64,
            _ => return Err(EINVAL),
        };
        let new = base
            .checked_add_signed(offset)
            .filter(|&new| new <= i64::MAX as u64)
            .ok_or(EINVAL)?;
        *position = new;
        Ok(new)
    }
}
