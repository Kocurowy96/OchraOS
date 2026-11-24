// SPDX-License-Identifier: GPL-2.0-only

//! The virtual filesystem: one interface over every filesystem, as `docs/vfs.md` describes.
//!
//! - [`Inode`] (`inode`): a file, directory, link or device node of some filesystem;
//! - [`PageCache`] (`page_cache`): the cached pages of a regular file's contents;
//! - dentries, mounts and [`Path`] (`path`): names, mounts, and looking paths up;
//! - [`may_access`] (`permission`): the one place that decides about access;
//! - [`VfsFile`] (`file`): an open file or directory.
//!
//! At boot (`init`) a tmpfs becomes the root, the initramfs is unpacked into it, and tmpfs
//! instances are mounted at `/dev` (with the device nodes) and `/tmp`.

mod file;
mod inode;
mod page_cache;
mod path;
mod permission;

use alloc::sync::Arc;

pub use file::{DeviceNode, VfsFile, touch_accessed, truncate};
pub use inode::{DirectoryEntry, Inode, Metadata, NewNode, Timestamp};
pub use page_cache::PageCache;
pub use path::{PATH_MAX, Parent, Path, Walk, mount, mounts, rename, root};
pub use permission::{Access, Credentials, may_access, may_delete, may_own};

use crate::fs::tmpfs;

/// File type bits of `st_mode` (`include/uapi/linux/stat.h`).
pub mod mode {
    pub const TYPE: u32 = 0o170_000;
    pub const SYMLINK: u32 = 0o120_000;
    pub const REGULAR: u32 = 0o100_000;
    pub const BLOCK_DEVICE: u32 = 0o060_000;
    pub const DIRECTORY: u32 = 0o040_000;
    pub const CHARACTER_DEVICE: u32 = 0o020_000;
    pub const FIFO: u32 = 0o010_000;
    pub const SET_UID: u32 = 0o4000;
    pub const SET_GID: u32 = 0o2000;
    /// The sticky bit: in a directory, only owners may delete entries.
    pub const STICKY: u32 = 0o1000;
    /// Permission bits and the three above: what `chmod` may change.
    pub const CHANGEABLE: u32 = 0o7777;
}

/// The device numbers (`st_dev`) of the in-memory filesystems: the "unnamed" major 0, as
/// Linux gives tmpfs and similar, with a minor number per instance.
pub fn anonymous_device() -> u64 {
    use core::sync::atomic::{AtomicU64, Ordering};
    static NEXT: AtomicU64 = AtomicU64::new(1);
    NEXT.fetch_add(1, Ordering::Relaxed)
}

/// Mounts the root tmpfs, unpacks `initramfs` into it, then mounts `/dev` (with the device
/// nodes) and `/tmp`. Call once at boot, before any process starts.
pub fn init(initramfs: &'static [u8]) {
    path::set_root(tmpfs::new(0o755), "tmpfs");
    match crate::initramfs::unpack(initramfs, &root()) {
        Ok(count) => log::info!(
            "vfs: root is a tmpfs, {count} initramfs entries ({} KiB) unpacked into it",
            initramfs.len() / 1024
        ),
        Err(error) => log::error!("vfs: unpacking the initramfs failed (errno {error})"),
    }
    let credentials = Credentials::root();
    let mount_at = |path: &str, inode: InodeRef, kind: &'static str| {
        let at = match root().walk(path, Walk::Follow, &credentials) {
            Ok(at) => at,
            Err(_) => {
                let parent = root()
                    .walk_parent(path, &credentials)
                    .expect("mount points are below existing directories");
                parent
                    .directory
                    .create(&parent.name, NewNode::Directory, 0o755, &credentials)
                    .expect("the root tmpfs takes new directories")
            }
        };
        mount(&at, inode, kind).expect("filesystems mount on directories");
    };
    mount_at("dev", tmpfs::new(0o755), "tmpfs");
    let dev = root()
        .walk("dev", Walk::Follow, &credentials)
        .expect("/dev was just mounted");
    crate::file::create_device_nodes(&dev);
    mount_at("dev/pts", crate::tty::devpts_root(), "devpts");
    mount_at("tmp", tmpfs::new(0o1777), "tmpfs");
    // Until `mount(2)` exists (P4), the kernel mounts these itself (docs/decisions.md).
    mount_at("proc", crate::fs::procfs::root(), "proc");
    mount_at("sys", crate::fs::sysfs::root(), "sysfs");
}

/// An inode behind an `Arc`.
pub type InodeRef = Arc<dyn Inode>;
