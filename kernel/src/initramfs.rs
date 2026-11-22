// SPDX-License-Identifier: GPL-2.0-only

//! The initramfs: a cpio archive ("newc" format, as in Linux) that Limine loads next to the
//! kernel. Until there is a VFS (stage 3) it is the only filesystem, read-only, and files are
//! looked up by their full path.
//!
//! An archive is a list of entries: a 110-byte ASCII header (magic `070701` and 13 fields of
//! 8 hexadecimal digits), the path, then the data, each padded to 4 bytes. The entry
//! `TRAILER!!!` ends it. `cargo xtask build` writes it (`xtask/src/cpio.rs`).

use ochra_frame::sync::IrqMutex;

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

/// The contents of the regular file at `path` (absolute, e.g. `/init`).
pub fn lookup(path: &str) -> Option<&'static [u8]> {
    let relative = path.strip_prefix('/')?;
    files()
        .find(|(name, _)| *name == relative)
        .map(|(_, data)| data)
}

/// Every regular file: its path without the leading `/`, and its contents.
pub fn files() -> impl Iterator<Item = (&'static str, &'static [u8])> {
    let archive = *ARCHIVE.lock();
    Entries { rest: archive }
        .map_while(|entry| entry)
        .filter(Entry::is_file)
        .map(|entry| (entry.name, entry.data))
}

struct Entry {
    mode: u32,
    name: &'static str,
    data: &'static [u8],
}

impl Entry {
    fn is_file(&self) -> bool {
        const TYPE_MASK: u32 = 0o170_000;
        const REGULAR: u32 = 0o100_000;
        self.mode & TYPE_MASK == REGULAR
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
    let size = field(6)? as usize;
    let name_size = field(11)? as usize;
    // The name ends with a NUL, which is counted in `name_size`.
    let name_bytes = bytes.get(header..header.checked_add(name_size)?.checked_sub(1)?)?;
    let name = core::str::from_utf8(name_bytes).ok()?;
    let data_start = (header + name_size).next_multiple_of(4);
    let data = bytes.get(data_start..data_start.checked_add(size)?)?;
    let length = (data_start + size).next_multiple_of(4).min(bytes.len());
    Some((Entry { mode, name, data }, length))
}
