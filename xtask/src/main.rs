// SPDX-License-Identifier: MIT OR Apache-2.0

//! `cargo xtask` — the OchraOS build helper.
//!
//! Everything it produces goes to `build/` in the repository root (ignored by git).

mod check_unsafe;
mod cpio;
mod image;
mod limine;
mod musl;
mod qemu;
mod test;
mod userspace;

use std::error::Error;
use std::fs::OpenOptions;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};

/// Errors are only ever printed, so a boxed error with a readable message is enough.
type Result<T> = std::result::Result<T, Box<dyn Error>>;

const USAGE: &str = "\
usage: cargo xtask <command>

commands:
  build [--release]  build the kernel and the bootable image
  run [--release]    boot in QEMU (UEFI via OVMF), serial output in the terminal
  test               boot in QEMU headless and check the serial log
  check-unsafe       fail if `unsafe` appears in code outside frame/

--release builds an optimised kernel without debug output (no addresses in the log).";

fn main() -> ExitCode {
    let arguments: Vec<String> = std::env::args().skip(1).collect();
    let command = arguments.first().map(String::as_str);
    let profile = match arguments.get(1).map(String::as_str) {
        None => Profile::Debug,
        Some("--release") if matches!(command, Some("build" | "run")) => Profile::Release,
        Some(_) => {
            eprintln!("{USAGE}");
            return ExitCode::FAILURE;
        }
    };
    if arguments.len() > 2 {
        eprintln!("{USAGE}");
        return ExitCode::FAILURE;
    }
    let result = match command {
        Some("build") => build(profile).map(|_| ()),
        Some("run") => build(profile).and_then(|image| qemu::run(&image)),
        Some("test") => test::run(),
        Some("check-unsafe") => check_unsafe::run(),
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

/// Which cargo profile the kernel is built with.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Profile {
    Debug,
    Release,
}

/// Builds the kernel, Limine and the boot image `build/ochraos.img`. Returns the image path.
fn build(profile: Profile) -> Result<PathBuf> {
    let kernel = build_kernel(&[], profile)?;
    let bootloader = limine::build(&build_dir()?)?;
    let (initramfs, _) = userspace::build()?;
    let image = build_dir()?.join("ochraos.img");
    image::create(&image, &kernel, &bootloader, &initramfs, "")?;
    Ok(image)
}

/// Compiles the kernel crate for bare metal with the given cargo features and returns the
/// path of the ELF file.
fn build_kernel(features: &[&str], profile: Profile) -> Result<PathBuf> {
    step("building the kernel");
    // `cargo xtask` runs under cargo, which tells us which cargo binary it is.
    let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".into());
    let mut command = Command::new(cargo);
    command.current_dir(root()).args([
        "build",
        "--package",
        "kernel",
        "--target",
        "x86_64-unknown-none",
    ]);
    if !features.is_empty() {
        command.arg("--features").arg(features.join(","));
    }
    let directory = match profile {
        Profile::Debug => "debug",
        Profile::Release => {
            command.arg("--release");
            "release"
        }
    };
    run(&mut command)?;
    Ok(root()
        .join("target/x86_64-unknown-none")
        .join(directory)
        .join("kernel"))
}

/// `build/` in the repository root, created if needed.
fn build_dir() -> Result<PathBuf> {
    let dir = root().join("build");
    std::fs::create_dir_all(&dir)?;
    Ok(dir)
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
/// On failure the end of the log is printed, so the cause shows up in CI output too.
fn run_logged(command: &mut Command, log: &Path) -> Result<()> {
    let file = OpenOptions::new().create(true).append(true).open(log)?;
    command.stdout(file.try_clone()?).stderr(file);
    run(command).map_err(|error| {
        let text = std::fs::read_to_string(log).unwrap_or_default();
        let lines: Vec<&str> = text.lines().collect();
        eprintln!("--- last lines of {} ---", log.display());
        for line in &lines[lines.len().saturating_sub(20)..] {
            eprintln!("{line}");
        }
        format!("{error}; full output in {}", log.display()).into()
    })
}
