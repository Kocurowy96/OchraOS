// SPDX-License-Identifier: GPL-2.0-only

//! Who may do what with a file: credentials, [`may_access`] and [`may_delete`].
//!
//! `may_access` is the only place that decides about access (`docs/vfs.md`): syscalls ask
//! it and never look at mode bits themselves. Every process runs as root for now, but the
//! IDs are stored and the rules are Linux's, so nothing changes when users come.

use alloc::vec::Vec;

use super::{Metadata, mode};
use crate::errno::{EACCES, EPERM, Errno};

/// A process's identity for permission checks (`man 7 credentials`).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Credentials {
    pub user: u32,
    pub effective_user: u32,
    pub saved_user: u32,
    pub group: u32,
    pub effective_group: u32,
    pub saved_group: u32,
    /// Supplementary groups.
    pub groups: Vec<u32>,
}

impl Credentials {
    /// Root: every ID 0.
    pub fn root() -> Self {
        Self::default()
    }

    /// Whether these credentials have the privileges of root (Linux checks capabilities:
    /// `CAP_DAC_OVERRIDE`, `CAP_FOWNER`…; root has them all, nobody else has any yet).
    pub fn is_root(&self) -> bool {
        self.effective_user == 0
    }

    fn in_group(&self, group: u32) -> bool {
        self.effective_group == group || self.groups.contains(&group)
    }
}

/// What is asked for, as the bits of a permission triple.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Access(u32);

impl Access {
    pub const READ: Self = Self(4);
    pub const WRITE: Self = Self(2);
    /// For a directory: search (look names up in it).
    pub const EXECUTE: Self = Self(1);
    pub const NONE: Self = Self(0);

    pub const fn union(self, other: Self) -> Self {
        Self(self.0 | other.0)
    }

    pub const fn contains(self, other: Self) -> bool {
        self.0 & other.0 == other.0
    }

    /// From the bits of `access(2)`'s mode (`R_OK` 4, `W_OK` 2, `X_OK` 1).
    pub const fn from_bits(bits: u32) -> Self {
        Self(bits & 7)
    }
}

/// Whether `credentials` may access the inode described by `metadata` as `access` asks.
///
/// As Linux (`generic_permission`): the owner's bits if the caller owns the file, else the
/// group's if it is in the file's group, else the others'. Root may read and write anything
/// and search any directory, but execute a file only if some execute bit is set.
pub fn may_access(
    metadata: &Metadata,
    credentials: &Credentials,
    access: Access,
) -> Result<(), Errno> {
    let allowed = if credentials.is_root() {
        let executable = metadata.is_directory() || metadata.mode & 0o111 != 0;
        !access.contains(Access::EXECUTE) || executable
    } else {
        let shift = if credentials.effective_user == metadata.user {
            6
        } else if credentials.in_group(metadata.group) {
            3
        } else {
            0
        };
        let granted = (metadata.mode >> shift) & 7;
        granted & access.0 == access.0
    };
    if !allowed {
        return Err(EACCES);
    }
    policy_hook(metadata, credentials, access)
}

/// Where a later security policy (Ochra Guard) plugs in, as Linux security modules do: it is
/// asked after the mode bits allowed the access and can only refuse it. There is no policy
/// yet, so it allows everything.
fn policy_hook(
    _metadata: &Metadata,
    _credentials: &Credentials,
    _access: Access,
) -> Result<(), Errno> {
    Ok(())
}

/// Whether `credentials` may remove (or rename away) `victim` from `directory`: writing and
/// searching the directory, and, in a sticky directory (as `/tmp`), owning the entry or the
/// directory, or being root ("restricted deletion").
pub fn may_delete(
    directory: &Metadata,
    victim: &Metadata,
    credentials: &Credentials,
) -> Result<(), Errno> {
    may_access(directory, credentials, Access::WRITE.union(Access::EXECUTE))?;
    let sticky = directory.mode & mode::STICKY != 0;
    let owns = credentials.effective_user == victim.user
        || credentials.effective_user == directory.user
        || credentials.is_root();
    if sticky && !owns {
        return Err(EPERM);
    }
    Ok(())
}

/// Whether `credentials` may change the metadata of an inode it may not own (`chmod`,
/// `utimensat` with explicit times): only its owner, or root.
pub fn may_own(metadata: &Metadata, credentials: &Credentials) -> Result<(), Errno> {
    if credentials.effective_user == metadata.user || credentials.is_root() {
        Ok(())
    } else {
        Err(EPERM)
    }
}
