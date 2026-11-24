// SPDX-License-Identifier: GPL-2.0-only

//! Filesystems: the implementations of the VFS's [`Inode`](crate::vfs::Inode).

pub mod procfs;
pub mod synthetic;
pub mod sysfs;
pub mod tmpfs;
