// SPDX-License-Identifier: MIT OR Apache-2.0

//! Downloads and builds the Limine bootloader from its source release.
//!
//! The project rule is "no unexplained binary files", so instead of committing or downloading
//! a ready-made `BOOTX64.EFI`, we fetch the source tarball of a pinned release, check its
//! SHA-256 against the value below, and compile it. The result is cached in `build/limine/`.

use std::path::{Path, PathBuf};
use std::process::Command;

use crate::{Result, run, run_logged, step};

/// Limine release we build. To upgrade: change both constants, the hash is printed by
/// `sha256sum limine-<version>.tar.xz` after downloading it from the release page.
const VERSION: &str = "12.11.1";
const SHA256: &str = "31518344aa35d0c86910e8a03016a7d742ab7d0e42098c7a7fa322945034d28a";

/// Returns the path of Limine's UEFI executable, building it first if needed.
pub fn build(build_dir: &Path) -> Result<PathBuf> {
    let dir = build_dir.join("limine");
    let source = dir.join(format!("limine-{VERSION}"));
    let out = dir.join(format!("build-{VERSION}"));
    let efi = out.join("bin/BOOTX64.EFI");
    if efi.exists() {
        return Ok(efi);
    }

    std::fs::create_dir_all(&dir)?;
    let tarball = dir.join(format!("limine-{VERSION}.tar.xz"));
    if !tarball.exists() {
        download(&tarball)?;
    }
    verify(&tarball)?;

    step(&format!("unpacking Limine {VERSION}"));
    run(Command::new("tar")
        .arg("xf")
        .arg(&tarball)
        .arg("-C")
        .arg(&dir))?;

    step(&format!("building Limine {VERSION} (only the first time)"));
    std::fs::create_dir_all(&out)?;
    // Only the x86-64 UEFI port. Limine defaults to clang + ld.lld; we use the host GCC and
    // GNU ld, which every Linux distribution (and the CI image) has anyway.
    // The compiler output is long and uninteresting; it goes to a log file instead.
    let log = dir.join("build.log");
    run_logged(
        Command::new(source.join("configure"))
            .current_dir(&out)
            .args([
                "--enable-uefi-x86-64",
                "CC_FOR_TARGET=gcc",
                "LD_FOR_TARGET=ld",
            ]),
        &log,
    )?;
    let jobs = std::thread::available_parallelism().map_or(1, |n| n.get());
    run_logged(
        Command::new("make")
            .current_dir(&out)
            .arg(format!("-j{jobs}")),
        &log,
    )?;

    Ok(efi)
}

fn download(tarball: &Path) -> Result<()> {
    let url = format!(
        "https://github.com/Limine-Bootloader/Limine/releases/download/v{VERSION}/limine-{VERSION}.tar.xz"
    );
    step(&format!("downloading {url}"));
    // Download to a temporary name, so an interrupted download is not mistaken for a
    // finished one on the next run.
    let partial = tarball.with_extension("partial");
    run(Command::new("curl")
        .args(["--fail", "--location", "--silent", "--show-error"])
        .args(["--proto", "=https", "--tlsv1.2"])
        .arg("--output")
        .arg(&partial)
        .arg(&url))?;
    std::fs::rename(&partial, tarball)?;
    Ok(())
}

/// Checks the tarball against the pinned hash; deletes it if it does not match.
fn verify(tarball: &Path) -> Result<()> {
    let output = Command::new("sha256sum").arg(tarball).output()?;
    let stdout = String::from_utf8(output.stdout)?;
    let actual = stdout.split_whitespace().next().unwrap_or_default();
    if output.status.success() && actual == SHA256 {
        return Ok(());
    }
    std::fs::remove_file(tarball)?;
    Err(format!(
        "{} has SHA-256 `{actual}`, expected `{SHA256}`; the file was deleted",
        tarball.display()
    )
    .into())
}
