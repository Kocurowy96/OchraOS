// SPDX-License-Identifier: MIT OR Apache-2.0

//! Writes cpio archives in the "newc" format, which is what Linux (and our kernel) reads as
//! an initramfs.
//!
//! Every entry is a 110-byte ASCII header (magic `070701` and 13 hexadecimal fields), the
//! path, and the file data, each padded to 4 bytes; a `TRAILER!!!` entry ends the archive.
//! Times, owners and inode numbers are fixed, so the same input always gives the same
//! archive, byte for byte (reproducible builds).

/// File type bits of `st_mode`.
const DIRECTORY: u32 = 0o040_000;
const REGULAR: u32 = 0o100_000;
const SYMLINK: u32 = 0o120_000;
const CHARACTER_DEVICE: u32 = 0o020_000;

/// An archive being built in memory.
#[derive(Default)]
pub struct Archive {
    bytes: Vec<u8>,
    entries: u32,
}

impl Archive {
    /// Adds a directory (`path` without a leading slash, e.g. `bin`).
    pub fn directory(&mut self, path: &str) {
        self.entry(path, DIRECTORY | 0o755, &[]);
    }

    /// Adds a regular file with permission bits `permissions` (e.g. `0o755`).
    pub fn file(&mut self, path: &str, permissions: u32, data: &[u8]) {
        self.entry(path, REGULAR | permissions, data);
    }

    /// Adds a symbolic link at `path` pointing to `target` (stored as the entry's data, as
    /// Linux does).
    pub fn symlink(&mut self, path: &str, target: &str) {
        self.entry(path, SYMLINK | 0o777, target.as_bytes());
    }

    /// Adds a character device node (e.g. `dev/null`, 1:3), readable and writable by all.
    pub fn character_device(&mut self, path: &str, major: u32, minor: u32) {
        self.entry_with_device(path, CHARACTER_DEVICE | 0o666, &[], (major, minor));
    }

    /// Ends the archive and returns its bytes.
    pub fn finish(mut self) -> Vec<u8> {
        self.entry("TRAILER!!!", 0, &[]);
        self.bytes
    }

    fn entry(&mut self, path: &str, mode: u32, data: &[u8]) {
        self.entry_with_device(path, mode, data, (0, 0));
    }

    /// An entry; `rdev` is the device number of a device node.
    fn entry_with_device(&mut self, path: &str, mode: u32, data: &[u8], rdev: (u32, u32)) {
        self.entries += 1;
        let fields = [
            self.entries,          // inode number
            mode,                  // mode
            0,                     // uid
            0,                     // gid
            1,                     // number of links
            0,                     // modification time
            data.len() as u32,     // file size
            0,                     // device major
            0,                     // device minor
            rdev.0,                // rdev major
            rdev.1,                // rdev minor
            path.len() as u32 + 1, // name size, with the terminating NUL
            0,                     // checksum (unused in newc)
        ];
        self.bytes.extend_from_slice(b"070701");
        for field in fields {
            self.bytes
                .extend_from_slice(format!("{field:08x}").as_bytes());
        }
        self.bytes.extend_from_slice(path.as_bytes());
        self.bytes.push(0);
        self.pad();
        self.bytes.extend_from_slice(data);
        self.pad();
    }

    fn pad(&mut self) {
        while !self.bytes.len().is_multiple_of(4) {
            self.bytes.push(0);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn layout() {
        let mut archive = Archive::default();
        archive.file("init", 0o755, b"abc");
        let bytes = archive.finish();
        // Header, "init\0" padded from 115 to 116 bytes, data padded to 4 bytes.
        assert_eq!(&bytes[..6], b"070701");
        assert_eq!(&bytes[6 + 8..6 + 16], b"000081ed");
        assert_eq!(&bytes[110..115], b"init\0");
        assert_eq!(&bytes[116..119], b"abc");
        assert_eq!(&bytes[120..126], b"070701");
        assert!(bytes.len().is_multiple_of(4));
    }
}
