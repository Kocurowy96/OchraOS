// SPDX-License-Identifier: MIT OR Apache-2.0

//! Boots the image in QEMU with OVMF.
//!
//! OVMF is UEFI firmware for virtual machines (part of the EDK II project). It comes as two
//! files: CODE (the firmware itself, read-only) and VARS (the UEFI variable store, e.g. boot
//! order). VARS is writable, so we give QEMU our own copy instead of the system one.

use std::path::{Path, PathBuf};
use std::process::Command;

use crate::{Result, step};

/// Where distributions install OVMF, as (CODE, VARS) pairs. The first pair that exists is used.
/// Set `OVMF_CODE` and `OVMF_VARS` to use other files.
const OVMF_LOCATIONS: &[(&str, &str)] = &[
    // Arch Linux, Garuda
    (
        "/usr/share/edk2/x64/OVMF_CODE.4m.fd",
        "/usr/share/edk2/x64/OVMF_VARS.4m.fd",
    ),
    // Debian, Ubuntu (and the GitHub Actions runners)
    (
        "/usr/share/OVMF/OVMF_CODE_4M.fd",
        "/usr/share/OVMF/OVMF_VARS_4M.fd",
    ),
    // Fedora
    (
        "/usr/share/edk2/ovmf/OVMF_CODE.fd",
        "/usr/share/edk2/ovmf/OVMF_VARS.fd",
    ),
];

/// Boots the image with a window and the serial port connected to this terminal.
pub fn run(image: &Path) -> Result<()> {
    let mut qemu = command(image)?;
    qemu.args(["-serial", "stdio"]);
    step("starting QEMU (close the window or press Ctrl+C to stop)");
    crate::run(&mut qemu)
}

/// The QEMU command line shared by `run` and (later) `test`.
fn command(image: &Path) -> Result<Command> {
    let (code, vars) = find_ovmf()?;
    let vars_copy = image.with_file_name("ovmf-vars.fd");
    std::fs::copy(&vars, &vars_copy)?;

    let mut qemu = Command::new("qemu-system-x86_64");
    qemu.args(["-machine", "q35", "-m", "256M"])
        // `max`: every CPU feature the accelerator can offer (we will want SMEP/SMAP later).
        .args(["-cpu", "max"])
        // Hardware virtualisation if available, otherwise plain emulation.
        .args(["-accel", "kvm", "-accel", "tcg"])
        .arg("-drive")
        .arg(pflash(&code, true))
        .arg("-drive")
        .arg(pflash(&vars_copy, false))
        .arg("-drive")
        .arg(format!("format=raw,file={}", image.display()))
        // A triple fault resets the machine; stop instead of looping through boot forever.
        .arg("-no-reboot");
    Ok(qemu)
}

fn pflash(file: &Path, read_only: bool) -> String {
    let read_only = if read_only { "on" } else { "off" };
    format!(
        "if=pflash,format=raw,readonly={read_only},file={}",
        file.display()
    )
}

fn find_ovmf() -> Result<(PathBuf, PathBuf)> {
    if let (Ok(code), Ok(vars)) = (std::env::var("OVMF_CODE"), std::env::var("OVMF_VARS")) {
        return Ok((code.into(), vars.into()));
    }
    OVMF_LOCATIONS
        .iter()
        .map(|(code, vars)| (PathBuf::from(code), PathBuf::from(vars)))
        .find(|(code, vars)| code.exists() && vars.exists())
        .ok_or_else(|| "OVMF not found; install it or set OVMF_CODE and OVMF_VARS".into())
}
