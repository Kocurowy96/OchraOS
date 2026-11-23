// SPDX-License-Identifier: MIT OR Apache-2.0

//! `cargo xtask test`: boots the kernel in QEMU without a window and checks the serial log.
//!
//! The kernel is built with the `selftest` feature, and booted once per [`CASES`] entry with a
//! different kernel command line: a normal boot, and boots that deliberately crash the kernel
//! to check that the crash report is right.
//!
//! The kernel never shuts the machine down; it halts. So QEMU's serial output is read line by
//! line, and QEMU is stopped once the kernel prints a line that means "nothing more will
//! come" (or after a timeout). Each log is saved as `build/test-<name>.log`; the log of a
//! failed boot, with QEMU's own error output, is also kept as `build/failed/<name>-<time>.log`,
//! so a rare failure is not overwritten by the next run.
//!
//! The `usertests` boot runs the syscall test programs (`userspace/tests/`) in OchraOS; their
//! output and exit status must be exactly what the same binaries give on the host Linux.

use std::io::{BufRead, BufReader, Read};
use std::path::{Path, PathBuf};
use std::process::{Child, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use crate::{Profile, Result, build_dir, build_kernel, image, limine, qemu, step, userspace};

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
    "random: ChaCha20 seeded",
    "physical memory:",
    "page tables:",
    "heap:",
    "interrupts: legacy PIC masked, local APIC in",
    "timer: local APIC timer at 100 Hz",
    "self-test random: ok",
    "self-test frames: ok",
    "self-test heap: ok",
    "self-test cpus: ok",
    "self-test interrupts: ok",
    "self-test timer: ok",
    "self-test user space: ok",
    "breakpoint (int3) at",
];

const CASES: &[Case] = &[
    Case {
        name: "boot",
        command_line: "",
        qemu_args: &[],
        expected: &[
            "started /init as process 1",
            "init: running as process 1",
            "Hello from musl",
            "init: /bin/hello exited with status 0",
            "init: pipeline OCHRA",
            "init: counted 5 lines",
            "init: sum of squares 14",
            "init: false gave 1",
            "init: subshell gave 7",
            "init: background job gave 9",
            "init: trap ran for USR1",
            "init: ochra ochra ochra from yes",
            "init: here-document with 42",
            "init: files ochra tree/branch tree/link note",
            "init: /tmp holds 2 entries again",
            "init: checks done",
            "init: starting a shell",
            "shell got 42",
            "init: the shell exited with status 3",
            "init: starting a shell",
        ],
        anywhere: &[
            // `yes`, ended by SIGPIPE when `head` is done (the kernel logs it).
            "(/bin/yes) killed by signal 13",
            "smp: 4 CPUs",
            "cpu 1 online",
            "cpu 2 online",
            "cpu 3 online",
            "hello from ring 3",
            "user task hello exited with status 0",
            "boot finished, idling",
        ],
    },
    // The syscall test programs; their results are compared with the host afterwards.
    Case {
        name: "usertests",
        command_line: "test=usertests",
        qemu_args: &[],
        expected: &[
            "programs finished",
            "no frames or heap memory leaked",
            "boot finished, idling",
        ],
        anywhere: &[],
    },
    // Init must never end: the kernel panics, as Linux does. `/bin/hello` exits at once.
    Case {
        name: "initexit",
        command_line: "rdinit=/bin/hello",
        qemu_args: &[],
        expected: &[
            "started /bin/hello as process 1",
            "Hello from musl",
            "KERNEL PANIC",
            "init (process 1) exited with status 0",
        ],
        anywhere: &[],
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
    // User programs that break the rules are killed; the kernel carries on.
    Case {
        name: "userkernel",
        command_line: "test=userkernel",
        qemu_args: &[],
        expected: &["boot finished, idling"],
        anywhere: &["user task kernel killed: Page fault at 0xffff800000000000"],
    },
    Case {
        name: "userhlt",
        command_line: "test=userhlt",
        qemu_args: &[],
        expected: &["boot finished, idling"],
        anywhere: &["user task hlt killed: General protection"],
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

/// Keys typed on the serial console during a boot (as if on a terminal), once `after`
/// appears; that boot is complete only when every line in `until` has appeared (or on a
/// crash), since `/init` keeps running after the kernel reports "idling".
struct Typing {
    case: &'static str,
    after: &'static str,
    keys: &'static str,
    until: &'static [&'static str],
}

const TYPING: &[Typing] = &[Typing {
    case: "boot",
    after: "init: starting a shell",
    // Two commands for the interactive shell; Enter sends a carriage return, as on a real
    // terminal. The console holds the keys until the shell reads them.
    keys: "echo shell got $((6 * 7))\rexit 3\r",
    until: &["init: the shell exited with status 3", "idling"],
}];

/// Generous, so the test also passes without KVM (plain emulation on CI runners).
const BOOT_TIMEOUT: Duration = Duration::from_secs(90);

/// How long to keep reading after a final line, to catch the rest of a panic message.
const DRAIN_TIME: Duration = Duration::from_millis(500);

pub fn run() -> Result<()> {
    // Self-tests exist only in debug builds (the feature refuses release).
    let kernel = build_kernel(&["selftest"], Profile::Debug)?;
    let bootloader = limine::build(&build_dir()?)?;
    let (initramfs, programs) = userspace::build()?;
    let image = build_dir()?.join("test.img");

    let mut failed = Vec::new();
    let mut kernel_addresses = Vec::new();
    let mut boot_log = String::new();
    let mut one_cpu_log = String::new();
    let mut user_tests_log = String::new();
    for case in CASES {
        image::create(&image, &kernel, &bootloader, &initramfs, case.command_line)?;
        step(&format!("test {}: booting", case.name));
        let typing = TYPING.iter().find(|typing| typing.case == case.name);
        let (log, qemu_errors) = boot(&image, case.qemu_args, typing)?;
        std::fs::write(build_dir()?.join(format!("test-{}.log", case.name)), &log)?;
        kernel_addresses.extend(kernel_virtual_address(&log));
        match case.name {
            "boot" => boot_log = log.clone(),
            "onecpu" => one_cpu_log = log.clone(),
            "usertests" => user_tests_log = log.clone(),
            _ => {}
        }

        match check(case, &log) {
            Ok(()) => eprintln!("    ok"),
            Err(problem) => {
                eprintln!("    FAILED: {problem}");
                let kept = keep_failed_log(case.name, &problem, &log, &qemu_errors)?;
                eprintln!("    full log: {}", kept.display());
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

    // On one CPU, A and B can only make progress together if the timer switches between them.
    step("test scheduler: on one CPU, tasks A and B take turns");
    match check_interleaving(&one_cpu_log) {
        Ok(switches) => eprintln!("    ok ({switches} switches between A and B)"),
        Err(problem) => {
            eprintln!("    FAILED: {problem}");
            failed.push("scheduler");
        }
    }

    // Stage 1.5 "Done when": with four CPUs, the tasks run on more than one of them.
    step("test smp: tasks run on more than one CPU");
    match check_cpus_used(&boot_log) {
        Ok(cpus) => eprintln!("    ok (task steps ran on {} CPUs: {cpus:?})", cpus.len()),
        Err(problem) => {
            eprintln!("    FAILED: {problem}");
            failed.push("smp");
        }
    }

    // Stage 2: Linux programs behave the same on OchraOS as on Linux.
    step("test linux abi: user programs give the same results as on the host");
    let tests: Vec<&userspace::Program> = programs
        .iter()
        .filter(|program| program.path.starts_with("/tests/"))
        .collect();
    let mut differences = Vec::new();
    for program in &tests {
        let name = &program.path["/tests/".len()..];
        let on_host = run_on_host(program)?;
        match result_in_log(&user_tests_log, name) {
            Some(in_ochra) if in_ochra == on_host => {}
            Some(in_ochra) => differences.push(format!(
                "{name}:\n      host:    {}\n      OchraOS: {}",
                on_host.describe(),
                in_ochra.describe()
            )),
            None => differences.push(format!("{name}: no result in the log")),
        }
    }
    if differences.is_empty() {
        eprintln!("    ok ({} programs)", tests.len());
    } else {
        for difference in &differences {
            eprintln!("    FAILED: {difference}");
        }
        failed.push("linux abi");
    }

    if failed.is_empty() {
        step(&format!("all {} tests passed", CASES.len() + 4));
        Ok(())
    } else {
        Err(format!(
            "failed tests: {} (logs in build/test-<name>.log)",
            failed.join(", ")
        )
        .into())
    }
}

/// How a test program ended and what it wrote to stdout.
#[derive(PartialEq, Eq)]
struct Outcome {
    /// `exit <status>` or `signal <number>`, as the kernel logs it.
    ending: String,
    output: Vec<u8>,
}

impl Outcome {
    fn describe(&self) -> String {
        format!(
            "{}, output {:?}",
            self.ending,
            String::from_utf8_lossy(&self.output)
        )
    }
}

/// Runs a test program on the host as the kernel does: `argv[0]` is its path in the
/// initramfs, and the environment is empty.
fn run_on_host(program: &userspace::Program) -> Result<Outcome> {
    use std::os::unix::process::{CommandExt, ExitStatusExt};

    let output = std::process::Command::new(&program.file)
        .arg0(&program.path)
        .env_clear()
        .stdin(Stdio::null())
        .stderr(Stdio::inherit())
        .output()?;
    let ending = match (output.status.code(), output.status.signal()) {
        (Some(code), _) => format!("exit {code}"),
        (None, Some(signal)) => format!("signal {signal}"),
        (None, None) => return Err(format!("{}: no exit status", program.path).into()),
    };
    Ok(Outcome {
        ending,
        output: output.stdout,
    })
}

/// Finds `user test <name>: <ending>, output <hex>` in the log.
fn result_in_log(log: &str, name: &str) -> Option<Outcome> {
    let log = strip_ansi(log);
    let prefix = format!("user test {name}: ");
    let line = log
        .lines()
        .find_map(|line| Some(&line[line.find(&prefix)? + prefix.len()..]))?;
    // An empty output leaves nothing after "output" (the trailing space is trimmed).
    let (ending, hex) = line.trim_end().split_once(", output")?;
    let hex = hex.trim_start();
    let output = (0..hex.len())
        .step_by(2)
        .map(|index| u8::from_str_radix(hex.get(index..index + 2)?, 16).ok())
        .collect::<Option<Vec<u8>>>()?;
    Some(Outcome {
        ending: ending.to_string(),
        output,
    })
}

/// Saves the log of a failed boot under `build/failed/`, with the reason and QEMU's error
/// output, and returns its path.
fn keep_failed_log(name: &str, problem: &str, log: &str, qemu_errors: &str) -> Result<PathBuf> {
    let dir = build_dir()?.join("failed");
    std::fs::create_dir_all(&dir)?;
    let time = SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs();
    let path = dir.join(format!("{name}-{time}.log"));
    let text = format!(
        "test {name} failed: {problem}\n\n--- serial output ---\n{log}\n--- QEMU stderr ---\n{qemu_errors}\n"
    );
    std::fs::write(&path, text)?;
    Ok(path)
}

/// Boots `image` headless and returns the serial output with colour codes removed, and
/// QEMU's own error output.
fn boot(image: &Path, extra_args: &[&str], typing: Option<&Typing>) -> Result<(String, String)> {
    let mut qemu = qemu::command(image)?;
    qemu.args(["-display", "none", "-serial", "stdio", "-monitor", "none"])
        .args(extra_args)
        // QEMU's stdin is the serial port's input.
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = qemu.spawn()?;

    // QEMU's own messages (e.g. "KVM not available"), read on their own thread so a full
    // pipe can never block QEMU.
    let mut stderr = child.stderr.take().expect("stderr is piped");
    let errors = std::thread::spawn(move || {
        let mut text = String::new();
        let _ = stderr.read_to_string(&mut text);
        text
    });

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

    let stdin = child.stdin.take().expect("stdin is piped");
    let result = collect(&receiver, typing, stdin);
    stop(&mut child);
    let qemu_errors = errors.join().unwrap_or_default();
    Ok((result?, qemu_errors))
}

/// Collects lines until the kernel is done, QEMU exits, or the timeout runs out; types the
/// keys of `typing` on the console when its line appears.
fn collect(
    lines: &mpsc::Receiver<String>,
    typing: Option<&Typing>,
    mut console: impl std::io::Write,
) -> Result<String> {
    let mut log = String::new();
    let deadline = Instant::now() + BOOT_TIMEOUT;
    let mut finished = false;
    let mut typed = false;
    loop {
        let wait = if finished {
            DRAIN_TIME
        } else {
            deadline.saturating_duration_since(Instant::now())
        };
        match lines.recv_timeout(wait) {
            Ok(line) => {
                log.push_str(&line);
                log.push('\n');
                let crashed = ["system halted", "KERNEL PANIC"]
                    .iter()
                    .any(|end| line.contains(end));
                finished |= match typing {
                    Some(typing) => crashed || typing.until.iter().all(|until| log.contains(until)),
                    None => FINAL_LINES.iter().any(|end| line.contains(end)),
                };
                if let Some(typing) = typing
                    && !typed
                    && line.contains(typing.after)
                {
                    console.write_all(typing.keys.as_bytes())?;
                    console.flush()?;
                    typed = true;
                }
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

/// The CPUs (from the `cpuN]` log prefix) on which tasks A and B logged their steps. Only the
/// set counts, never timing: which CPU picks a task up after a tick is up to the scheduler.
fn check_cpus_used(log: &str) -> std::result::Result<Vec<u32>, String> {
    let mut cpus: Vec<u32> = log
        .lines()
        .filter(|line| line.contains("task A: step") || line.contains("task B: step"))
        .filter_map(|line| line.split(" cpu").nth(1)?.split(']').next()?.parse().ok())
        .collect();
    let steps = cpus.len();
    if steps != 10 {
        return Err(format!("tasks A and B logged {steps} of 10 steps"));
    }
    cpus.sort();
    cpus.dedup();
    if cpus.len() < 2 {
        return Err(format!("all task steps ran on cpu {cpus:?}"));
    }
    Ok(cpus)
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
