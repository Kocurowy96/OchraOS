// SPDX-License-Identifier: GPL-2.0-only

//! The initramfs: a cpio archive ("newc" format, as in Linux) that Limine loads next to the
//! kernel. Until there is a VFS (stage 3) it is the only filesystem, read-only. It holds
//! regular files, directories, symbolic links (whose data is the target path) and character
//! device nodes (`/dev/null`…); a directory also exists implicitly when some entry lies
//! inside it.
//!
//! An archive is a list of entries: a 110-byte ASCII header (magic `070701` and 13 fields of
//! 8 hexadecimal digits), the path, then the data, each padded to 4 bytes. The entry
//! `TRAILER!!!` ends it. `cargo xtask build` writes it (`xtask/src/cpio.rs`).

use alloc::string::String;
use alloc::vec::Vec;

use ochra_frame::sync::IrqMutex;

use crate::errno::{ELOOP, ENAMETOOLONG, ENOENT, ENOTDIR, Errno};

/// The archive's bytes, set once at boot.
static ARCHIVE: IrqMutex<&'static [u8]> = IrqMutex::new(&[]);

/// Remembers the archive. Returns how many files it holds, or `None` if it is malformed
/// (then it is not used at all).
pub fn init(archive: &'static [u8]) -> Option<usize> {
    let files = Entries { rest: archive }
        .map(|entry| entry.map(|entry| entry.is_file()))
        .try_fold(0, |count, is_file| {
            is_file.map(|is_file| count + usize::from(is_file))
        })?;
    *ARCHIVE.lock() = archive;
    Some(files)
}

/// What a path names.
#[derive(Clone, Copy, Debug)]
pub enum Node {
    File(&'static [u8]),
    Directory,
    Symlink(&'static str),
    /// A character device, by major and minor number.
    CharacterDevice(u32, u32),
}

/// A path found in the archive: its canonical form (absolute, no `.`, `..` or links),
/// what it is, its mode (`st_mode`: type and permissions) and an inode number (the entry's
/// position; 1 for the root and for directories that only exist implicitly).
#[derive(Clone, Debug)]
pub struct Found {
    pub path: String,
    pub node: Node,
    pub mode: u32,
    pub inode: u64,
}

/// Most symbolic links followed for one path, as Linux (`MAXSYMLINKS`).
const MAX_LINKS: usize = 40;
/// Longest path, with its NUL (Linux's `PATH_MAX`).
pub const PATH_MAX: usize = 4096;

/// Looks up `path`, relative to the directory `cwd` (canonical) unless it is absolute,
/// following symbolic links on the way, and in the last component too if `follow_last`.
pub fn resolve(cwd: &str, path: &str, follow_last: bool) -> Result<Found, Errno> {
    if path.is_empty() {
        return Err(ENOENT);
    }
    if path.len() >= PATH_MAX {
        return Err(ENAMETOOLONG);
    }
    // Components still to walk, last first, and the canonical directory reached so far.
    let mut pending: Vec<String> = Vec::new();
    push_components(&mut pending, path);
    let mut current: Vec<String> = if path.starts_with('/') {
        Vec::new()
    } else {
        cwd.split('/')
            .filter(|part| !part.is_empty())
            .map(String::from)
            .collect()
    };
    let mut links = 0;
    let mut found = root();
    while let Some(component) = pending.pop() {
        if !matches!(found.node, Node::Directory) {
            return Err(ENOTDIR);
        }
        match component.as_str() {
            "." => continue,
            ".." => {
                current.pop();
                found = lookup_canonical(&current).ok_or(ENOENT)?;
                continue;
            }
            _ => {}
        }
        current.push(component);
        let next = lookup_canonical(&current).ok_or(ENOENT)?;
        let last = pending.is_empty();
        if let Node::Symlink(target) = next.node
            && (!last || follow_last)
        {
            links += 1;
            if links > MAX_LINKS {
                return Err(ELOOP);
            }
            current.pop();
            push_components(&mut pending, target);
            if target.starts_with('/') {
                current.clear();
            }
            found = lookup_canonical(&current).ok_or(ENOENT)?;
            continue;
        }
        found = next;
    }
    Ok(found)
}

/// Adds the components of `path` to `pending` so that its first one is popped next.
fn push_components(pending: &mut Vec<String>, path: &str) {
    let parts: Vec<&str> = path.split('/').filter(|part| !part.is_empty()).collect();
    pending.extend(parts.into_iter().rev().map(String::from));
}

/// The mode of the root and of implicit directories: `drwxr-xr-x`.
const IMPLICIT_DIRECTORY: u32 = DIRECTORY | 0o755;

fn root() -> Found {
    Found {
        path: String::from("/"),
        node: Node::Directory,
        mode: IMPLICIT_DIRECTORY,
        inode: 1,
    }
}

/// The entry at the canonical path made of `components` (no links followed).
fn lookup_canonical(components: &[String]) -> Option<Found> {
    if components.is_empty() {
        return Some(root());
    }
    let relative = components.join("/");
    let path = alloc::format!("/{relative}");
    let archive = *ARCHIVE.lock();
    let mut implicit = false;
    for (index, entry) in (Entries { rest: archive })
        .map_while(|entry| entry)
        .enumerate()
    {
        if entry.name == relative {
            let node = entry.node()?;
            return Some(Found {
                path,
                node,
                mode: entry.mode,
                inode: index as u64 + 2,
            });
        }
        implicit |= entry
            .name
            .strip_prefix(relative.as_str())
            .is_some_and(|rest| rest.starts_with('/'));
    }
    implicit.then_some(Found {
        path,
        node: Node::Directory,
        mode: IMPLICIT_DIRECTORY,
        inode: 1,
    })
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

/// File type bits of the mode (`st_mode`).
const TYPE_MASK: u32 = 0o170_000;
const REGULAR: u32 = 0o100_000;
const DIRECTORY: u32 = 0o040_000;
const SYMLINK: u32 = 0o120_000;
const CHARACTER_DEVICE: u32 = 0o020_000;

impl Entry {
    fn is_file(&self) -> bool {
        self.mode & TYPE_MASK == REGULAR
    }

    /// What the entry is; `None` for kinds the initramfs does not support (devices…).
    fn node(&self) -> Option<Node> {
        match self.mode & TYPE_MASK {
            REGULAR => Some(Node::File(self.data)),
            DIRECTORY => Some(Node::Directory),
            SYMLINK => core::str::from_utf8(self.data).ok().map(Node::Symlink),
            CHARACTER_DEVICE => Some(Node::CharacterDevice(self.rdev.0, self.rdev.1)),
            _ => None,
        }
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
