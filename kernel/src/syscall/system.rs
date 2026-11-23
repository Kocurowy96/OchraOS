// SPDX-License-Identifier: GPL-2.0-only

//! Syscalls about the system as a whole.

use super::{SyscallResult, write_user};

/// The Linux release whose syscall interface OchraOS aims at (the "ABI target", see
/// `docs/architecture.md`). Programs and C libraries read it to decide what they may use
/// (glibc refuses kernels older than it was built for), so it is a Linux version, marked as
/// ours by the suffix.
const ABI_TARGET_RELEASE: &str = "6.12.0-ochra";

/// `uname(buf)`: `struct utsname`, six NUL-terminated fields of 65 bytes. The system name is
/// "Linux", as programs expect of a Linux ABI; the version field says what really runs.
/// The host and domain names are "(none)", as in Linux until something sets them.
pub fn uname(buffer: u64) -> SyscallResult {
    const FIELD: usize = 65;
    let version = concat!("OchraOS ", env!("CARGO_PKG_VERSION"));
    let fields = [
        "Linux",
        "(none)",
        ABI_TARGET_RELEASE,
        version,
        "x86_64",
        "(none)",
    ];
    let mut bytes = [0; FIELD * 6];
    for (index, field) in fields.iter().enumerate() {
        // Every field is far shorter than 64 bytes, so a NUL always follows it.
        bytes[index * FIELD..][..field.len()].copy_from_slice(field.as_bytes());
    }
    write_user(buffer, &bytes)?;
    Ok(0)
}
