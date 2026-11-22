// SPDX-License-Identifier: MIT OR Apache-2.0

//! `cargo xtask` — the OchraOS build helper.
//!
//! The commands are filled in during stage 0; for now this only parses the command line.

use std::process::ExitCode;

const USAGE: &str = "\
usage: cargo xtask <command>

commands:
  build    build the kernel and the bootable image
  run      boot in QEMU (UEFI via OVMF), serial output in the terminal
  test     boot in QEMU headless and check the serial log";

fn main() -> ExitCode {
    let command = std::env::args().nth(1);
    match command.as_deref() {
        Some("build" | "run" | "test") => {
            eprintln!("xtask: not implemented yet");
            ExitCode::FAILURE
        }
        _ => {
            eprintln!("{USAGE}");
            ExitCode::FAILURE
        }
    }
}
