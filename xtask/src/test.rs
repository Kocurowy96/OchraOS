// SPDX-License-Identifier: MIT OR Apache-2.0

//! `cargo xtask test`: boots the kernel in QEMU without a window and checks the serial log.
//!
//! The kernel is built with the `selftest` feature, and booted once per [`CASES`] entry with a
//! different kernel command line: a normal boot, and boots that deliberately crash the kernel
//! to check that the crash report is right.
//!
//! The kernel never shuts the machine down; it halts. So QEMU's serial output is read line by
//! line, and QEMU is stopped once the kernel prints a line that means "nothing more will
//! come" (or after a timeout). Each log is saved as `build/test-<name>.log`.

use std::io::{BufRead, BufReader};
use std::path::Path;
use std::process::{Child, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use crate::{Result, build_dir, build_kernel, image, limine, qemu, step};

struct Case {
    name: &'static str,
    /// Kernel command line.
    command_line: &'static str,
    /// Lines that must appear in the log, in this order (each matched as a substring).
    expected: &'static [&'static str],
}

/// Lines that must appear in every boot, before the kernel does anything case-specific.
const BOOT: &[&str] = &[
    r"/\_____/\",
    "OchraOS",
    "bootloader: Limine",
    "GDT, TSS and IDT loaded",
    "breakpoint (int3) at",
];

const CASES: &[Case] = &[
    Case {
        name: "boot",
        command_line: "",
        expected: &["stage 0 boot finished, halting"],
    },
    Case {
        name: "pagefault",
        command_line: "test=pagefault",
        expected: &[
            "CPU EXCEPTION: Page fault (#PF, vector 14)",
            "write to a non-present page, in kernel mode",
            "address  0x00000000deadbeef",
            "system halted",
        ],
    },
    Case {
        name: "doublefault",
        command_line: "test=doublefault",
        expected: &[
            "CPU EXCEPTION: Double fault (#DF, vector 8)",
            "system halted",
        ],
    },
];

/// Text that must never appear, except where a case expects it.
const FORBIDDEN: &[&str] = &["KERNEL PANIC", "CPU EXCEPTION", "ERROR"];

/// After one of these lines the kernel prints nothing more (except, for a panic, the message).
const FINAL_LINES: &[&str] = &["halting", "system halted", "KERNEL PANIC"];

/// Generous, so the test also passes without KVM (plain emulation on CI runners).
const BOOT_TIMEOUT: Duration = Duration::from_secs(90);

/// How long to keep reading after a final line, to catch the rest of a panic message.
const DRAIN_TIME: Duration = Duration::from_millis(500);

pub fn run() -> Result<()> {
    let kernel = build_kernel(&["selftest"])?;
    let bootloader = limine::build(&build_dir()?)?;
    let image = build_dir()?.join("test.img");

    let mut failed = Vec::new();
    for case in CASES {
        image::create(&image, &kernel, &bootloader, case.command_line)?;
        step(&format!("test {}: booting", case.name));
        let log = boot(&image)?;
        std::fs::write(build_dir()?.join(format!("test-{}.log", case.name)), &log)?;

        match check(case, &log) {
            Ok(()) => eprintln!("    ok"),
            Err(problem) => {
                eprintln!("    FAILED: {problem}");
                failed.push(case.name);
            }
        }
    }

    if failed.is_empty() {
        step(&format!("all {} tests passed", CASES.len()));
        Ok(())
    } else {
        Err(format!(
            "failed tests: {} (logs in build/test-<name>.log)",
            failed.join(", ")
        )
        .into())
    }
}

/// Boots `image` headless and returns the serial output with colour codes removed.
fn boot(image: &Path) -> Result<String> {
    let mut qemu = qemu::command(image)?;
    qemu.args(["-display", "none", "-serial", "stdio", "-monitor", "none"])
        .stdin(Stdio::null())
        .stdout(Stdio::piped());
    let mut child = qemu.spawn()?;

    // Read QEMU's output on a separate thread, so the main thread can wait with a timeout.
    let stdout = child.stdout.take().expect("stdout is piped");
    let (sender, receiver) = mpsc::channel();
    std::thread::spawn(move || {
        for line in BufReader::new(stdout).split(b'\n') {
            let Ok(line) = line else { break };
            // The UART sends "\r\n"; firmware output may not be valid UTF-8.
            let line = String::from_utf8_lossy(&line).trim_end().to_string();
            if sender.send(strip_ansi(&line)).is_err() {
                break;
            }
        }
    });

    let result = collect(&receiver);
    stop(&mut child);
    result
}

/// Collects lines until the kernel is done, QEMU exits, or the timeout runs out.
fn collect(lines: &mpsc::Receiver<String>) -> Result<String> {
    let mut log = String::new();
    let deadline = Instant::now() + BOOT_TIMEOUT;
    let mut finished = false;
    loop {
        let wait = if finished {
            DRAIN_TIME
        } else {
            deadline.saturating_duration_since(Instant::now())
        };
        match lines.recv_timeout(wait) {
            Ok(line) => {
                finished |= FINAL_LINES.iter().any(|end| line.contains(end));
                log.push_str(&line);
                log.push('\n');
            }
            // QEMU exited (e.g. a triple fault with `-no-reboot`): the log is complete.
            Err(mpsc::RecvTimeoutError::Disconnected) => return Ok(log),
            Err(mpsc::RecvTimeoutError::Timeout) if finished => return Ok(log),
            Err(mpsc::RecvTimeoutError::Timeout) => {
                return Ok(log + "\n[xtask: timed out waiting for the kernel]\n");
            }
        }
    }
}

fn stop(child: &mut Child) {
    // QEMU may already be gone; either way we only need it not to be running any more.
    let _ = child.kill();
    let _ = child.wait();
}

/// Checks the expected lines (in order) and the forbidden ones.
fn check(case: &Case, log: &str) -> std::result::Result<(), String> {
    // Firmware and bootloader output comes first; the kernel log starts at the banner.
    let banner = BOOT[0];
    let Some(start) = log.find(banner) else {
        return Err(format!("the kernel banner {banner:?} never appeared"));
    };
    let kernel_log = &log[start..];

    let mut rest = kernel_log;
    for expected in BOOT.iter().chain(case.expected) {
        match rest.find(expected) {
            Some(position) => rest = &rest[position + expected.len()..],
            None => return Err(format!("missing (or out of order): {expected:?}")),
        }
    }

    for forbidden in FORBIDDEN {
        let allowed = case.expected.iter().any(|line| line.contains(forbidden));
        if !allowed && kernel_log.contains(forbidden) {
            return Err(format!("unexpected {forbidden:?} in the log"));
        }
    }
    Ok(())
}

/// Removes ANSI escape sequences (`ESC [ … letter`), i.e. the colours.
fn strip_ansi(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars();
    while let Some(c) = chars.next() {
        if c == '\x1b' {
            // Skip up to and including the final letter of the sequence.
            for c in chars.by_ref() {
                if c.is_ascii_alphabetic() {
                    break;
                }
            }
        } else {
            out.push(c);
        }
    }
    out
}
