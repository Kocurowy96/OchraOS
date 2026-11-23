// SPDX-License-Identifier: MIT OR Apache-2.0

//! Builds musl, the C library our user programs are linked against.
//!
//! As with Limine, nothing prebuilt is used: the source comes from musl's own Git
//! repository at a pinned tag, and the checked-out commit must be exactly the pinned one, so
//! a moved tag cannot slip other code in. The result is cached in `build/musl/`.

use std::path::{Path, PathBuf};
use std::process::Command;

use crate::git::Pinned;
use crate::{Result, run_logged, step};

const MUSL: Pinned = Pinned {
    name: "musl",
    repository: "https://git.musl-libc.org/git/musl",
    tag: "v1.2.6",
    commit: "9fa28ece75d8a2191de7c5bb53bed224c5947417",
};

/// Returns musl's install prefix (with `include/` and `lib/`), building it first if needed.
pub fn build(build_dir: &Path) -> Result<PathBuf> {
    let tag = MUSL.tag;
    let dir = build_dir.join("musl");
    let install = dir.join(format!("install-{tag}"));
    if install.join("lib/libc.a").exists() {
        return Ok(install);
    }

    let source = dir.join(format!("src-{tag}"));
    MUSL.fetch(&source)?;

    step(&format!("building musl {tag} (only the first time)"));
    let objects = dir.join(format!("obj-{tag}"));
    std::fs::create_dir_all(&objects)?;
    let log = dir.join("build.log");
    // Static library only; no `musl-gcc` wrapper (its specs file mishandles static-pie, so
    // `userspace` passes the start files and libraries itself).
    run_logged(
        Command::new(source.join("configure"))
            .current_dir(&objects)
            .arg(format!("--prefix={}", install.display()))
            .args([
                "--disable-shared",
                "--disable-wrapper",
                "CC=gcc",
                "CFLAGS=-O2",
            ]),
        &log,
    )?;
    let jobs = std::thread::available_parallelism().map_or(1, |n| n.get());
    run_logged(
        Command::new("make")
            .current_dir(&objects)
            .arg(format!("-j{jobs}")),
        &log,
    )?;
    run_logged(
        Command::new("make").current_dir(&objects).arg("install"),
        &log,
    )?;
    Ok(install)
}
