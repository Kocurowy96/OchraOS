// SPDX-License-Identifier: GPL-2.0-only

//! tmpfs: a filesystem that lives only in memory, as Linux's. The root, `/dev` and `/tmp`
//! are tmpfs instances.
//!
//! A regular file's contents are its page cache: the pages are never written anywhere, and
//! a page that was never written is a hole (zeros). Directories keep their entries in a map;
//! each entry gets a cookie, increasing in creation order, for `getdents64` to continue
//! where it stopped even while entries come and go.

use alloc::collections::BTreeMap;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::any::Any;
use core::sync::atomic::{AtomicU64, Ordering};

use ochra_frame::sync::IrqMutex;

use crate::errno::{EEXIST, EINVAL, ENOENT, ENOTDIR, ENOTEMPTY, EXDEV, Errno};
use crate::vfs::{
    DirectoryEntry, Inode, InodeRef, Metadata, NewNode, PageCache, Timestamp, anonymous_device,
    mode,
};

/// Linux's tmpfs reports a directory's size as 20 bytes per entry, `.` and `..` included
/// (`BOGO_DIRENT_SIZE`); `ls -l` shows it, so it is copied.
const DIRENT_SIZE: u64 = 20;

/// One tmpfs instance.
struct Filesystem {
    device: u64,
    next_inode: AtomicU64,
}

/// A new tmpfs; returns its root directory, with the given permission bits, owned by root.
pub fn new(permissions: u32) -> InodeRef {
    let filesystem = Arc::new(Filesystem {
        device: anonymous_device(),
        // The root is inode 1, as in Linux's tmpfs.
        next_inode: AtomicU64::new(1),
    });
    TmpInode::create_inode(
        &filesystem,
        NewNode::Directory,
        mode::DIRECTORY | permissions,
        (0, 0),
    )
}

struct TmpInode {
    filesystem: Arc<Filesystem>,
    metadata: IrqMutex<Metadata>,
    contents: Contents,
}

enum Contents {
    Regular(PageCache),
    Directory(IrqMutex<Directory>),
    Symlink(String),
    /// A device node: everything is in the metadata (`rdev`).
    Device,
}

#[derive(Default)]
struct Directory {
    /// Name to cookie and inode.
    entries: BTreeMap<String, (u64, InodeRef)>,
    /// Cookie to name, for listing in cookie order.
    names: BTreeMap<u64, String>,
    /// The next cookie; 0, 1 and 2 mean the start, `.` and `..`.
    next_cookie: u64,
}

impl Directory {
    fn insert(&mut self, name: &str, inode: InodeRef) {
        let cookie = self.next_cookie.max(3);
        self.next_cookie = cookie + 1;
        self.entries.insert(String::from(name), (cookie, inode));
        self.names.insert(cookie, String::from(name));
    }

    fn remove(&mut self, name: &str) -> Option<InodeRef> {
        let (cookie, inode) = self.entries.remove(name)?;
        self.names.remove(&cookie);
        Some(inode)
    }
}

impl TmpInode {
    fn create_inode(
        filesystem: &Arc<Filesystem>,
        node: NewNode,
        mode: u32,
        owner: (u32, u32),
    ) -> InodeRef {
        let now = Timestamp::now();
        let (contents, links, size, rdev) = match node {
            NewNode::Regular => (Contents::Regular(PageCache::new()), 1, 0, 0),
            NewNode::Directory => (
                Contents::Directory(IrqMutex::new(Directory::default())),
                2,
                2 * DIRENT_SIZE,
                0,
            ),
            NewNode::Symlink(target) => {
                let size = target.len() as u64;
                (Contents::Symlink(target), 1, size, 0)
            }
            NewNode::CharacterDevice(rdev) => (Contents::Device, 1, 0, rdev),
        };
        Arc::new(Self {
            filesystem: filesystem.clone(),
            metadata: IrqMutex::new(Metadata {
                inode: filesystem.next_inode.fetch_add(1, Ordering::Relaxed),
                device: filesystem.device,
                mode,
                user: owner.0,
                group: owner.1,
                links,
                size,
                rdev,
                accessed: now,
                modified: now,
                changed: now,
            }),
            contents,
        })
    }

    fn directory(&self) -> Result<&IrqMutex<Directory>, Errno> {
        match &self.contents {
            Contents::Directory(directory) => Ok(directory),
            _ => Err(ENOTDIR),
        }
    }

    fn change(&self, change: impl FnOnce(&mut Metadata)) {
        change(&mut self.metadata.lock());
    }

    /// Another inode of this filesystem, or `EXDEV`.
    fn same_filesystem<'a>(&self, inode: &'a InodeRef) -> Result<&'a TmpInode, Errno> {
        inode
            .as_any()
            .downcast_ref::<TmpInode>()
            .filter(|other| Arc::ptr_eq(&other.filesystem, &self.filesystem))
            .ok_or(EXDEV)
    }

    fn is_empty_directory(&self) -> bool {
        self.directory()
            .is_ok_and(|directory| directory.lock().entries.is_empty())
    }
}

/// Mode bits for a new inode of kind `node` with `permissions`.
fn mode_of(node: &NewNode, permissions: u32) -> u32 {
    let kind = match node {
        NewNode::Regular => mode::REGULAR,
        NewNode::Directory => mode::DIRECTORY,
        NewNode::Symlink(_) => mode::SYMLINK,
        NewNode::CharacterDevice(_) => mode::CHARACTER_DEVICE,
    };
    kind | (permissions & mode::CHANGEABLE)
}

impl Inode for TmpInode {
    fn metadata(&self) -> Metadata {
        *self.metadata.lock()
    }

    fn set_metadata(&self, change: &mut dyn FnMut(&mut Metadata)) -> Result<(), Errno> {
        let mut metadata = self.metadata.lock();
        let mut changed = *metadata;
        change(&mut changed);
        // What identifies the inode stays.
        changed.inode = metadata.inode;
        changed.device = metadata.device;
        changed.mode = (metadata.mode & mode::TYPE) | (changed.mode & mode::CHANGEABLE);
        *metadata = changed;
        Ok(())
    }

    fn as_any(&self) -> &dyn Any {
        self
    }

    fn lookup(&self, name: &str) -> Result<InodeRef, Errno> {
        let directory = self.directory()?.lock();
        let (_, inode) = directory.entries.get(name).ok_or(ENOENT)?;
        Ok(inode.clone())
    }

    fn create(
        &self,
        name: &str,
        node: NewNode,
        permissions: u32,
        owner: (u32, u32),
    ) -> Result<InodeRef, Errno> {
        let mut directory = self.directory()?.lock();
        if directory.entries.contains_key(name) {
            return Err(EEXIST);
        }
        let is_directory = matches!(node, NewNode::Directory);
        let mode = mode_of(&node, permissions);
        let inode = TmpInode::create_inode(&self.filesystem, node, mode, owner);
        directory.insert(name, inode.clone());
        drop(directory);
        self.change(|metadata| {
            metadata.size += DIRENT_SIZE;
            // The new directory's `..` is a link to this one.
            if is_directory {
                metadata.links += 1;
            }
        });
        Ok(inode)
    }

    fn link(&self, name: &str, inode: &InodeRef) -> Result<(), Errno> {
        let target = self.same_filesystem(inode)?;
        let mut directory = self.directory()?.lock();
        if directory.entries.contains_key(name) {
            return Err(EEXIST);
        }
        directory.insert(name, inode.clone());
        drop(directory);
        target.change(|metadata| metadata.links += 1);
        self.change(|metadata| metadata.size += DIRENT_SIZE);
        Ok(())
    }

    fn unlink(&self, name: &str) -> Result<(), Errno> {
        let inode = self.directory()?.lock().remove(name).ok_or(ENOENT)?;
        let _ =
            inode.set_metadata(&mut |metadata| metadata.links = metadata.links.saturating_sub(1));
        self.change(|metadata| metadata.size -= DIRENT_SIZE);
        Ok(())
    }

    fn rmdir(&self, name: &str) -> Result<(), Errno> {
        let mut directory = self.directory()?.lock();
        let (_, inode) = directory.entries.get(name).ok_or(ENOENT)?;
        let victim = self.same_filesystem(inode)?;
        if !victim.is_empty_directory() {
            return Err(ENOTEMPTY);
        }
        let inode = directory.remove(name).ok_or(ENOENT)?;
        drop(directory);
        let _ = inode.set_metadata(&mut |metadata| metadata.links = 0);
        self.change(|metadata| {
            metadata.size -= DIRENT_SIZE;
            metadata.links -= 1;
        });
        Ok(())
    }

    /// The VFS holds the locks of both directories and the global rename lock, so nothing
    /// else changes them meanwhile: the steps below need not be one atomic step.
    fn rename(&self, old: &str, new_parent: &InodeRef, new: &str) -> Result<(), Errno> {
        let new_directory = self.same_filesystem(new_parent)?;
        let source = self.lookup(old)?;
        let moves_directory = source.metadata().is_directory();
        let same_parent = core::ptr::eq(new_directory, self);

        // Make room at the new name.
        let mut entries = new_directory.directory()?.lock();
        let replaced = match entries.entries.get(new) {
            Some((_, target)) => {
                let target = self.same_filesystem(target)?;
                if target.metadata().is_directory() {
                    if !target.is_empty_directory() {
                        return Err(ENOTEMPTY);
                    }
                    if !moves_directory {
                        return Err(EINVAL);
                    }
                }
                entries.remove(new)
            }
            None => None,
        };
        entries.insert(new, source.clone());
        drop(entries);
        self.directory()?.lock().remove(old);

        let now = Timestamp::now();
        if let Some(replaced) = replaced {
            let replaced_directory = replaced.metadata().is_directory();
            let _ = replaced.set_metadata(&mut |metadata| {
                metadata.links = if replaced_directory {
                    0
                } else {
                    metadata.links.saturating_sub(1)
                };
                metadata.changed = now;
            });
            // The replaced directory's `..` goes; the moved one's comes below.
            new_directory.change(|metadata| {
                metadata.size -= DIRENT_SIZE;
                if replaced_directory {
                    metadata.links -= 1;
                }
            });
        }
        self.change(|metadata| metadata.size -= DIRENT_SIZE);
        new_directory.change(|metadata| metadata.size += DIRENT_SIZE);
        if moves_directory && !same_parent {
            self.change(|metadata| metadata.links -= 1);
            new_directory.change(|metadata| metadata.links += 1);
        }
        Ok(())
    }

    fn read_directory(&self, after: u64) -> Result<Vec<DirectoryEntry>, Errno> {
        let directory = self.directory()?.lock();
        let entries = directory
            .names
            .range(after + 1..)
            .map(|(&cookie, name)| {
                let (_, inode) = &directory.entries[name];
                let metadata = inode.metadata();
                DirectoryEntry {
                    cookie,
                    inode: metadata.inode,
                    kind: metadata.directory_type(),
                    name: name.clone(),
                }
            })
            .collect();
        Ok(entries)
    }

    fn readlink(&self) -> Result<String, Errno> {
        match &self.contents {
            Contents::Symlink(target) => Ok(target.clone()),
            _ => Err(EINVAL),
        }
    }

    fn page_cache(&self) -> Option<&PageCache> {
        match &self.contents {
            Contents::Regular(cache) => Some(cache),
            _ => None,
        }
    }
}
