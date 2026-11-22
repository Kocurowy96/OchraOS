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

use crate::{Profile, Result, build_dir, build_kernel, image, limine, qemu, step};

struct Case {
    name: &'static str,
    /// Kernel command line.
    command_line: &'static str,
    /// Extra QEMU arguments (a later `-cpu` replaces the default one).
    qemu_args: &'static [&'static str],
    /// Lines that must appear in the log, in this order (each matched as a substring).
    expected: &'static [&'static str],
    /// Lines that must appear somewhere in the kernel log, in any order.
    anywhere: &'static [&'static str],
}

/// Lines that must appear in every boot, before the kernel does anything case-specific.
const BOOT: &[&str] = &[
    r"/\_____/\",
    "OchraOS",
    "bootloader: Limine",
    "GDT, TSS and IDT loaded",
    "CPU protections:",
    "physical memory:",
    "page tables:",
    "heap:",
    "interrupts: legacy PIC masked, local APIC in",
    "timer: local APIC timer at 100 Hz",
    "self-test frames: ok",
    "self-test heap: ok",
    "self-test cpus: ok",
    "self-test interrupts: ok",
    "self-test timer: ok",
    "breakpoint (int3) at",
];

const CASES: &[Case] = &[
    Case {
        name: "boot",
        command_line: "",
        qemu_args: &[],
        expected: &["boot finished, idling"],
        anywhere: &[
            "smp: 4 CPUs",
            "cpu 1 online",
            "cpu 2 online",
            "cpu 3 online",
        ],
    },
    Case {
        name: "pagefault",
        command_line: "test=pagefault",
        qemu_args: &[],
        expected: &[
            "CPU EXCEPTION: Page fault (#PF, vector 14)",
            "stopped 3 other CPUs",
            "write to a non-present page, in kernel mode",
            "address  0x00000000deadbeef",
            "system halted",
        ],
        anywhere: &[],
    },
    Case {
        name: "doublefault",
        command_line: "test=doublefault",
        qemu_args: &[],
        expected: &[
            "CPU EXCEPTION: Double fault (#DF, vector 8)",
            "stopped 3 other CPUs",
            "system halted",
        ],
        anywhere: &[],
    },
    Case {
        name: "wx",
        command_line: "test=wx",
        qemu_args: &[],
        expected: &[
            "CPU EXCEPTION: Page fault (#PF, vector 14)",
            "write to a page without the needed permission, in kernel mode",
            "system halted",
        ],
        anywhere: &[],
    },
    Case {
        name: "nx",
        command_line: "test=nx",
        qemu_args: &[],
        expected: &[
            "CPU EXCEPTION: Page fault (#PF, vector 14)",
            "instruction fetch from a page without the needed permission, in kernel mode",
            "system halted",
        ],
        anywhere: &[],
    },
    Case {
        name: "guardpage",
        command_line: "test=guardpage",
        qemu_args: &[],
        expected: &[
            "CPU EXCEPTION: Page fault (#PF, vector 14)",
            "write to a non-present page, in kernel mode",
            "system halted",
        ],
        anywhere: &[],
    },
    Case {
        name: "smep",
        command_line: "test=smep",
        qemu_args: &[],
        expected: &[
            "CPU EXCEPTION: Page fault (#PF, vector 14)",
            "instruction fetch from a page without the needed permission, in kernel mode",
            "address  0x0000000010000000",
            "system halted",
        ],
        anywhere: &[],
    },
    Case {
        name: "smap",
        command_line: "test=smap",
        qemu_args: &[],
        expected: &[
            "CPU EXCEPTION: Page fault (#PF, vector 14)",
            "read from a page without the needed permission, in kernel mode",
            "address  0x0000000010000000",
            "system halted",
        ],
        anywhere: &[],
    },
    // An AP takes a double fault: it must land on that AP's own interrupt stack.
    Case {
        name: "apdoublefault",
        command_line: "test=apdoublefault",
        qemu_args: &[],
        expected: &[
            "CPU EXCEPTION: Double fault (#DF, vector 8)",
            "on cpu 1",
            "stopped 3 other CPUs",
            "system halted",
        ],
        anywhere: &[],
    },
    // A task overflows its stack into the guard page.
    Case {
        name: "taskstack",
        command_line: "test=taskstack",
        qemu_args: &[],
        expected: &[
            "CPU EXCEPTION: Double fault (#DF, vector 8)",
            "stopped 3 other CPUs",
            "system halted",
        ],
        anywhere: &[],
    },
    // A machine with a single core still boots (a later `-smp` replaces the default one).
    Case {
        name: "onecpu",
        command_line: "",
        qemu_args: &["-smp", "1"],
        expected: &["boot finished, idling"],
        anywhere: &["smp: 1 CPUs, 0 started besides the boot CPU"],
    },
    // The same boot on a CPU without x2APIC, so the xAPIC (memory-mapped) path is tested too.
    Case {
        name: "xapic",
        command_line: "",
        qemu_args: &["-cpu", "max,-x2apic"],
        expected: &["boot finished, idling"],
        anywhere: &["local APIC in xAPIC mode"],
    },
];

/// Text that must never appear, except where a case expects it.
const FORBIDDEN: &[&str] = &["KERNEL PANIC", "CPU EXCEPTION", "ERROR"];

/// After one of these lines the kernel prints nothing more (except, for a panic, the message).
const FINAL_LINES: &[&str] = &["idling", "system halted", "KERNEL PANIC"];

/// Generous, so the test also passes without KVM (plain emulation on CI runners).
const BOOT_TIMEOUT: Duration = Duration::from_secs(90);

/// How long to keep reading after a final line, to catch the rest of a panic message.
const DRAIN_TIME: Duration = Duration::from_millis(500);

pub fn run() -> Result<()> {
    // Self-tests exist only in debug builds (the feature refuses release).
    let kernel = build_kernel(&["selftest"], Profile::Debug)?;
    let bootloader = limine::build(&build_dir()?)?;
    let image = build_dir()?.join("test.img");

    let mut failed = Vec::new();
    let mut kernel_addresses = Vec::new();
    let mut boot_log = String::new();
    for case in CASES {
        image::create(&image, &kernel, &bootloader, case.command_line)?;
        step(&format!("test {}: booting", case.name));
        let log = boot(&image, case.qemu_args)?;
        std::fs::write(build_dir()?.join(format!("test-{}.log", case.name)), &log)?;
        kernel_addresses.extend(kernel_virtual_address(&log));
        if case.name == "boot" {
            boot_log = log.clone();
        }

        match check(case, &log) {
            Ok(()) => eprintln!("    ok"),
            Err(problem) => {
                eprintln!("    FAILED: {problem}");
                failed.push(case.name);
            }
        }
    }

    // KASLR: Limine picks a new random address at every boot. The chance that all boots land
    // on the same one by accident is negligible (the offset is chosen in 4 KiB steps).
    step("test kaslr: comparing the kernel addresses of all boots");
    kernel_addresses.sort();
    kernel_addresses.dedup();
    if kernel_addresses.len() >= 2 {
        eprintln!("    ok ({} different addresses)", kernel_addresses.len());
    } else {
        eprintln!("    FAILED: the kernel was loaded at the same address every time");
        failed.push("kaslr");
    }

    step("test scheduler: tasks A and B take turns");
    match check_interleaving(&boot_log) {
        Ok(switches) => eprintln!("    ok ({switches} switches between A and B)"),
        Err(problem) => {
            eprintln!("    FAILED: {problem}");
            failed.push("scheduler");
        }
    }

    if failed.is_empty() {
        step(&format!("all {} tests passed", CASES.len() + 2));
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
fn boot(image: &Path, extra_args: &[&str]) -> Result<String> {
    let mut qemu = qemu::command(image)?;
    qemu.args(["-display", "none", "-serial", "stdio", "-monitor", "none"])
        .args(extra_args)
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

    for line in case.anywhere {
        if !kernel_log.contains(line) {
            return Err(format!("missing: {line:?}"));
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

/// Checks that the steps of tasks A and B interleave: only their order counts, never timing.
/// Each task waits for a timer tick after every step, so with round-robin switching on every
/// tick they alternate; we only require more switches than running one after the other would
/// give (1), so an occasional double step on a slow machine still passes.
fn check_interleaving(log: &str) -> std::result::Result<usize, String> {
    let tasks: Vec<char> = log
        .lines()
        .filter_map(|line| line.split("task ").nth(1))
        .filter_map(|rest| rest.strip_suffix(" of 5").and(rest.chars().next()))
        .collect();
    for name in ['A', 'B'] {
        let steps = tasks.iter().filter(|&&task| task == name).count();
        if steps != 5 {
            return Err(format!("task {name} logged {steps} of 5 steps"));
        }
    }
    let switches = tasks.windows(2).filter(|pair| pair[0] != pair[1]).count();
    if switches < 4 {
        return Err(format!(
            "only {switches} switches between A and B: {tasks:?}"
        ));
    }
    Ok(switches)
}

/// The virtual address from the log line "loaded at physical …, virtual 0x…".
fn kernel_virtual_address(log: &str) -> Option<String> {
    let line = log
        .lines()
        .find(|line| line.contains("loaded at physical"))?;
    let address = line.split("virtual ").nth(1)?;
    Some(address.trim().to_string())
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
