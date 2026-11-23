// SPDX-License-Identifier: MIT OR Apache-2.0

//! Sources fetched from Git at a pinned tag (musl, BusyBox). The checked-out commit must be
//! exactly the pinned one, so a moved tag cannot slip other code in.

use std::path::Path;
use std::process::Command;

use crate::{Result, run, step};

/// A pinned source. To upgrade: change `tag` and `commit`; the commit is printed by
/// `git ls-remote <repository> <tag>` (for an annotated tag, the line ending in `^{}`).
pub struct Pinned {
    pub name: &'static str,
    pub repository: &'static str,
    pub tag: &'static str,
    pub commit: &'static str,
}

impl Pinned {
    /// Clones the source into `dest` unless it is already there, then checks the commit.
    pub fn fetch(&self, dest: &Path) -> Result<()> {
        if !dest.exists() {
            step(&format!("fetching {} {}", self.name, self.tag));
            if let Some(parent) = dest.parent() {
                std::fs::create_dir_all(parent)?;
            }
            run(Command::new("git")
                .args([
                    "clone",
                    "--quiet",
                    "--depth",
                    "1",
                    "--branch",
                    self.tag,
                    self.repository,
                ])
                .arg(dest)
                .env("GIT_ADVICE", "0"))?;
        }
        self.verify(dest)
    }

    /// Fails unless the checked-out commit is the pinned one.
    fn verify(&self, source: &Path) -> Result<()> {
        let output = Command::new("git")
            .arg("-C")
            .arg(source)
            .args(["rev-parse", "HEAD"])
            .output()?;
        let commit = String::from_utf8_lossy(&output.stdout).trim().to_string();
        if commit != self.commit {
            return Err(format!(
                "{} {} is commit {commit}, expected {}; delete {} to fetch it again",
                self.name,
                self.tag,
                self.commit,
                source.display()
            )
            .into());
        }
        Ok(())
    }
}
