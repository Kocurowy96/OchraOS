// SPDX-License-Identifier: GPL-2.0-only

//! The initramfs: a cpio archive ("newc" format, as in Linux) that Limine loads next to the
//! kernel. At boot it is unpacked into the root tmpfs (`vfs::init`), as Linux does with its
//! initramfs. It holds regular files, directories, symbolic links (whose data is the target
//! path) and character device nodes; the parent directories of an entry are created when
//! the archive does not list them.
//!
//! An archive is a list of entries: a 110-byte ASCII header (magic `070701` and 13 fields of
//! 8 hexadecimal digits), the path, then the data, each padded to 4 bytes. The entry
//! `TRAILER!!!` ends it. `cargo xtask build` writes it (`xtask/src/cpio.rs`).

use alloc::string::String;

use ochra_frame::sync::IrqMutex;

use crate::errno::{EEXIST, EINVAL, Errno};
use crate::file::device;
use crate::vfs::{Credentials, NewNode, Path, Walk, mode};

/// The archive's bytes, kept for the self-tests (which list its programs).
static ARCHIVE: IrqMutex<&'static [u8]> = IrqMutex::new(&[]);

/// Unpacks `archive` into the directory `root`; returns how many entries it held. Stops at
/// the first malformed entry (`EINVAL`), keeping what was unpacked before it.
pub fn unpack(archive: &'static [u8], root: &Path) -> Result<usize, Errno> {
    *ARCHIVE.lock() = archive;
    let credentials = Credentials::root();
    let mut count = 0;
    for entry in (Entries { rest: archive }) {
        let entry = entry.ok_or(EINVAL)?;
        let name = entry.name.trim_start_matches('/');
        if name.is_empty() || name == "." {
            continue;
        }
        let (parent, base) = match name.rsplit_once('/') {
            Some((parent, base)) => (make_directories(root, parent, &credentials)?, base),
            None => (root.clone(), name),
        };
        let permissions = entry.mode & mode::CHANGEABLE;
        let node = match entry.mode & mode::TYPE {
            mode::REGULAR => NewNode::Regular,
            mode::DIRECTORY => NewNode::Directory,
            mode::SYMLINK => {
                let target = core::str::from_utf8(entry.data).map_err(|_| EINVAL)?;
                NewNode::Symlink(String::from(target))
            }
            mode::CHARACTER_DEVICE => {
                NewNode::CharacterDevice(device(entry.rdev.0.into(), entry.rdev.1.into()))
            }
            // Other kinds (FIFOs, sockets, block devices) are not in OchraOS's archives.
            _ => return Err(EINVAL),
        };
        let is_directory = matches!(node, NewNode::Directory);
        let created = match parent.create(base, node, permissions, &credentials) {
            Ok(created) => created,
            // A directory created earlier for an entry inside it: give it its own mode.
            Err(EEXIST) if is_directory => {
                let existing = parent.walk(base, Walk::NoFollow, &credentials)?;
                existing.inode().set_metadata(&mut |metadata| {
                    metadata.mode = (metadata.mode & mode::TYPE) | permissions;
                })?;
                count += 1;
                continue;
            }
            Err(error) => return Err(error),
        };
        if entry.mode & mode::TYPE == mode::REGULAR {
            write_contents(&created, entry.data)?;
        }
        count += 1;
    }
    Ok(count)
}

/// `mkdir -p`: the directory `path` under `root`, created (mode 755) where missing.
fn make_directories(root: &Path, path: &str, credentials: &Credentials) -> Result<Path, Errno> {
    let mut current = root.clone();
    for name in path.split('/').filter(|name| !name.is_empty()) {
        current = match current.walk(name, Walk::Follow, credentials) {
            Ok(found) => found,
            Err(crate::errno::ENOENT) => {
                current.create(name, NewNode::Directory, 0o755, credentials)?
            }
            Err(error) => return Err(error),
        };
    }
    Ok(current)
}

/// Writes a file's contents through its page cache.
fn write_contents(file: &Path, data: &[u8]) -> Result<(), Errno> {
    let inode = file.inode();
    let cache = inode.page_cache().ok_or(EINVAL)?;
    let written = cache.write(inode.as_ref(), 0, data)?;
    if written < data.len() {
        return Err(crate::errno::ENOSPC);
    }
    inode.set_metadata(&mut |metadata| metadata.size = data.len() as u64)
}

/// Every regular file: its path without the leading `/`, and its contents.
#[cfg_attr(
    not(feature = "selftest"),
    expect(dead_code, reason = "only the user-program tests list files")
)]
pub fn files() -> impl Iterator<Item = (&'static str, &'static [u8])> {
    let archive = *ARCHIVE.lock();
    Entries { rest: archive }
        .map_while(|entry| entry)
        .filter(Entry::is_file)
        .map(|entry| (entry.name, entry.data))
}

struct Entry {
    mode: u32,
    /// Major and minor number, for a device node.
    rdev: (u32, u32),
    name: &'static str,
    data: &'static [u8],
}

impl Entry {
    fn is_file(&self) -> bool {
        self.mode & mode::TYPE == mode::REGULAR
    }
}

/// Walks the entries; yields `None` once for a malformed entry, then stops.
struct Entries {
    rest: &'static [u8],
}

impl Iterator for Entries {
    type Item = Option<Entry>;

    fn next(&mut self) -> Option<Self::Item> {
        const HEADER: usize = 110;
        if self.rest.is_empty() {
            return None;
        }
        let entry = parse(self.rest, HEADER);
        match entry {
            Some((entry, _)) if entry.name == "TRAILER!!!" => {
                self.rest = &[];
                None
            }
            Some((entry, length)) => {
                self.rest = &self.rest[length..];
                Some(Some(entry))
            }
            None => {
                self.rest = &[];
                Some(None)
            }
        }
    }
}

/// Parses the entry at the start of `bytes`; returns it with its length (padding included).
fn parse(bytes: &'static [u8], header: usize) -> Option<(Entry, usize)> {
    if bytes.get(..6)? != b"070701" {
        return None;
    }
    let field = |index: usize| -> Option<u32> {
        let start = 6 + index * 8;
        let digits = core::str::from_utf8(bytes.get(start..start + 8)?).ok()?;
        u32::from_str_radix(digits, 16).ok()
    };
    let mode = field(1)?;
    let rdev = (field(9)?, field(10)?);
    let size = field(6)? as usize;
    let name_size = field(11)? as usize;
    // The name ends with a NUL, which is counted in `name_size`.
    let name_bytes = bytes.get(header..header.checked_add(name_size)?.checked_sub(1)?)?;
    let name = core::str::from_utf8(name_bytes).ok()?;
    let data_start = (header + name_size).next_multiple_of(4);
    let data = bytes.get(data_start..data_start.checked_add(size)?)?;
    let length = (data_start + size).next_multiple_of(4).min(bytes.len());
    Some((
        Entry {
            mode,
            rdev,
            name,
            data,
        },
        length,
    ))
}
