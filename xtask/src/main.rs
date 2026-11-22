// SPDX-License-Identifier: MIT OR Apache-2.0

//! `cargo xtask` — the OchraOS build helper.
//!
//! Everything it produces goes to `build/` in the repository root (ignored by git).

mod image;
mod limine;
mod qemu;

use std::error::Error;
use std::fs::OpenOptions;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};

/// Errors are only ever printed, so a boxed error with a readable message is enough.
type Result<T> = std::result::Result<T, Box<dyn Error>>;

const USAGE: &str = "\
usage: cargo xtask <command>

commands:
  build    build the kernel and the bootable image
  run      boot in QEMU (UEFI via OVMF), serial output in the terminal
  test     boot in QEMU headless and check the serial log";

fn main() -> ExitCode {
    let command = std::env::args().nth(1);
    let result = match command.as_deref() {
        Some("build") => build().map(|_| ()),
        Some("run") => build().and_then(|image| qemu::run(&image)),
        Some("test") => Err("not implemented yet".into()),
        _ => {
            eprintln!("{USAGE}");
            return ExitCode::FAILURE;
        }
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("xtask: error: {error}");
            ExitCode::FAILURE
        }
    }
}

/// Builds the kernel, Limine and the boot image. Returns the path of the image.
fn build() -> Result<PathBuf> {
    let build_dir = root().join("build");
    std::fs::create_dir_all(&build_dir)?;

    let kernel = build_kernel()?;
    let bootloader = limine::build(&build_dir)?;
    image::create(&build_dir, &kernel, &bootloader)
}

/// Compiles the kernel crate for bare metal and returns the path of the ELF file.
fn build_kernel() -> Result<PathBuf> {
    step("building the kernel");
    // `cargo xtask` runs under cargo, which tells us which cargo binary it is.
    let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".into());
    run(Command::new(cargo).current_dir(root()).args([
        "build",
        "--package",
        "kernel",
        "--target",
        "x86_64-unknown-none",
    ]))?;
    Ok(root().join("target/x86_64-unknown-none/debug/kernel"))
}

/// The repository root (the parent of the `xtask/` directory).
fn root() -> &'static Path {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("xtask/ is inside the repository root")
}

/// Prints a progress line, so long steps (building Limine) do not look like a hang.
fn step(message: &str) {
    eprintln!("==> {message}");
}

/// Runs a command and turns a non-zero exit status into an error.
fn run(command: &mut Command) -> Result<()> {
    let program = command.get_program().to_string_lossy().into_owned();
    let status = command
        .status()
        .map_err(|error| format!("cannot start `{program}`: {error}"))?;
    if status.success() {
        Ok(())
    } else {
        Err(format!("`{program}` failed ({status})").into())
    }
}

/// Like [`run`], but appends the command's output to `log` instead of printing it.
fn run_logged(command: &mut Command, log: &Path) -> Result<()> {
    let file = OpenOptions::new().create(true).append(true).open(log)?;
    command.stdout(file.try_clone()?).stderr(file);
    run(command).map_err(|error| format!("{error}; see {}", log.display()).into())
}
