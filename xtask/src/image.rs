// SPDX-License-Identifier: MIT OR Apache-2.0

//! Creates the bootable disk image.
//!
//! The image is a single FAT32 file system without a partition table. UEFI firmware can boot
//! from it directly: it looks for `\EFI\BOOT\BOOTX64.EFI` (the "fallback" boot path), which
//! is Limine. Limine then reads its configuration and the kernel from the same file system.
//!
//! The FAT file system is written with mtools (`mformat`, `mmd`, `mcopy`), which work on an
//! image file directly, without mounting it and without root.

use std::fs::File;
use std::path::Path;
use std::process::Command;

use crate::{Result, root, run, step};

/// 64 MiB: comfortably above the FAT32 minimum (about 33 MiB) and plenty for stage 0.
const IMAGE_SIZE: u64 = 64 * 1024 * 1024;

/// Writes the boot image to `image`, with `initramfs` as a module next to the kernel. A
/// non-empty `command_line` is passed to the kernel.
pub fn create(
    image: &Path,
    kernel: &Path,
    bootloader: &Path,
    initramfs: &Path,
    command_line: &str,
) -> Result<()> {
    step(&format!("creating the boot image {}", image.display()));
    // Start from an empty (all zero) file each time, so no stale files survive.
    File::create(image)?.set_len(IMAGE_SIZE)?;

    let mtools = |tool: &str| {
        let mut command = Command::new(tool);
        command.arg("-i").arg(image);
        command
    };

    // -F: FAT32; -v: volume label.
    run(mtools("mformat").args(["-F", "-v", "OCHRAOS", "::"]))?;
    run(mtools("mmd").args(["::/EFI", "::/EFI/BOOT", "::/boot", "::/boot/limine"]))?;
    run(mtools("mcopy")
        .arg(bootloader)
        .arg("::/EFI/BOOT/BOOTX64.EFI"))?;
    let config = image.with_extension("limine.conf");
    std::fs::write(&config, limine_config(command_line)?)?;
    run(mtools("mcopy")
        .arg(&config)
        .arg("::/boot/limine/limine.conf"))?;
    run(mtools("mcopy").arg(kernel).arg("::/boot/kernel"))?;
    run(mtools("mcopy").arg(initramfs).arg("::/boot/initramfs.cpio"))?;

    Ok(())
}

/// `xtask/limine.conf`, plus a `cmdline:` line for the (only) boot entry if needed.
fn limine_config(command_line: &str) -> Result<String> {
    let mut config = std::fs::read_to_string(root().join("xtask/limine.conf"))?;
    if !command_line.is_empty() {
        config.push_str(&format!("    cmdline: {command_line}\n"));
    }
    Ok(config)
}
