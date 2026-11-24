// SPDX-License-Identifier: GPL-2.0-only

//! `/proc` (`man 5 proc`): the kernel's view of processes and of the system, as text files
//! made when they are opened.
//!
//! What a process may learn about another one follows Linux: `environ`, `maps`, the `fd`
//! directory and the `exe`/`cwd` links need the caller to be allowed to inspect the target
//! ([`may_inspect`], Linux's `ptrace_may_access`), and no file shows a kernel address
//! (`kstkesp`, `kstkeip` and `wchan` in `stat` are always 0, docs/security.md).
//!
//! Differences from Linux: `cmdline` and `environ` are copies taken at `execve`, not read
//! from the process's memory; the `fd` and `exe` links are ordinary symbolic links to a
//! path (Linux's open the file itself), so `pipe:[…]` cannot be opened through them; and
//! there is no CPU time accounting yet, so every time is 0.

use alloc::boxed::Box;
use alloc::format;
use alloc::string::{String, ToString};
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::fmt::Write;

use ochra_frame::arch::cpu;
use ochra_frame::mm::{FRAME_SIZE, frames};
use ochra_frame::sync::IrqMutex;

use super::synthetic::{self, Attributes, Contents, Entry, Listing, Maker, Target};
use crate::errno::{EACCES, ENOENT, Errno};
use crate::file::{PIPE_DEVICE, flags, major_minor};
use crate::process::{self, Process};
use crate::syscall::{ABI_TARGET_RELEASE, VERSION};
use crate::time::{self, NANOSECONDS_PER_SECOND};
use crate::vfs::{InodeRef, anonymous_device, mode, mounts};

/// Clock ticks per second in the times `/proc` shows (`USER_HZ`, `sysconf(_SC_CLK_TCK)`).
const USER_HZ: u64 = 100;

/// The device number of procfs, made once.
fn device() -> u64 {
    static DEVICE: IrqMutex<Option<u64>> = IrqMutex::new(None);
    *DEVICE.lock().get_or_insert_with(anonymous_device)
}

/// Inode numbers: the root 1, the system files 2.., a process's files `pid << 16 | n`, its
/// descriptors `pid << 16 | 0x1000 | fd`.
fn attributes(inode: u64, permissions: u32, owner: (u32, u32)) -> Attributes {
    Attributes {
        device: device(),
        inode,
        permissions,
        owner,
    }
}

fn text(make: impl Fn() -> Result<String, Errno> + Send + Sync + 'static) -> Contents {
    Box::new(move || make().map(String::into_bytes))
}

fn system_file(inode: u64, make: fn() -> Result<String, Errno>) -> InodeRef {
    synthetic::file(attributes(inode, 0o444, (0, 0)), text(make))
}

/// The root of procfs, to mount on `/proc`.
pub fn root() -> InodeRef {
    synthetic::directory(attributes(1, 0o555, (0, 0)), Box::new(Root))
}

/// The files about the system, in listing order, and how to make each.
const SYSTEM: &[(&str, Maker)] = &[
    ("self", || {
        synthetic::symlink(
            attributes(2, 0o777, (0, 0)),
            Box::new(|| {
                let process = process::current().ok_or(ENOENT)?;
                Ok(process.id.to_string())
            }),
        )
    }),
    ("cpuinfo", || system_file(3, cpuinfo)),
    ("filesystems", || system_file(4, filesystems)),
    ("meminfo", || system_file(5, meminfo)),
    ("mounts", || system_file(6, mounts_file)),
    ("uptime", || system_file(7, uptime)),
    ("version", || system_file(8, version)),
];

/// `/proc`: the system files, then a directory per process.
struct Root;

impl Listing for Root {
    fn entries(&self) -> Result<Vec<Entry>, Errno> {
        let mut entries = SYSTEM.entries()?;
        for process in process::matching(|_| true) {
            entries.push(Entry {
                cookie: 1000 + u64::from(process.id),
                name: process.id.to_string(),
                inode: process_directory(&process),
            });
        }
        Ok(entries)
    }

    fn lookup(&self, name: &str) -> Result<InodeRef, Errno> {
        if let Ok(id) = name.parse::<u32>() {
            return process::find(id)
                .map(|process| process_directory(&process))
                .ok_or(ENOENT);
        }
        SYSTEM.lookup(name)
    }
}

/// Whether the caller may see private things about `target` (its environment, memory map,
/// open files): Linux's `ptrace_may_access` with `PTRACE_MODE_READ_FSCREDS`. Itself, root,
/// or a caller whose user and group match all of the target's real, effective and saved
/// ones. Everything runs as root for now; the check is here so it stays when users come.
fn may_inspect(target: &Process) -> Result<(), Errno> {
    let Some(caller) = process::current() else {
        return Ok(());
    };
    if caller.id == target.id {
        return Ok(());
    }
    let caller = caller.credentials.lock().clone();
    if caller.is_root() {
        return Ok(());
    }
    let target = target.credentials.lock().clone();
    let user = caller.effective_user;
    let group = caller.effective_group;
    let same_user = [target.user, target.effective_user, target.saved_user]
        .iter()
        .all(|&id| id == user);
    let same_group = [target.group, target.effective_group, target.saved_group]
        .iter()
        .all(|&id| id == group);
    if same_user && same_group {
        Ok(())
    } else {
        Err(EACCES)
    }
}

/// The process `id`, while it exists.
fn process(id: u32) -> Result<Arc<Process>, Errno> {
    process::find(id).ok_or(ENOENT)
}

fn owner(process: &Process) -> (u32, u32) {
    let credentials = process.credentials.lock();
    (credentials.effective_user, credentials.effective_group)
}

/// What a process's directory holds; `inspect` marks the files that need [`may_inspect`].
struct ProcessFile {
    name: &'static str,
    number: u64,
    kind: ProcessKind,
}

enum ProcessKind {
    File {
        permissions: u32,
        inspect: bool,
        make: fn(&Process) -> Result<String, Errno>,
    },
    Bytes {
        permissions: u32,
        inspect: bool,
        make: fn(&Process) -> Vec<u8>,
    },
    Link(fn(&Process) -> Result<String, Errno>),
    Descriptors,
}

const PROCESS_FILES: &[ProcessFile] = &[
    ProcessFile {
        name: "cmdline",
        number: 1,
        kind: ProcessKind::Bytes {
            permissions: 0o444,
            inspect: false,
            make: |process| alive(process, || process.arguments.lock().clone()),
        },
    },
    ProcessFile {
        name: "cwd",
        number: 2,
        kind: ProcessKind::Link(|process| process.cwd.lock().absolute_name()),
    },
    ProcessFile {
        name: "environ",
        number: 3,
        kind: ProcessKind::Bytes {
            permissions: 0o400,
            inspect: true,
            make: |process| alive(process, || process.environment.lock().clone()),
        },
    },
    ProcessFile {
        name: "exe",
        number: 4,
        kind: ProcessKind::Link(|process| {
            let executable = process.executable.lock().clone();
            executable.ok_or(ENOENT)?.absolute_name()
        }),
    },
    ProcessFile {
        name: "fd",
        number: 5,
        kind: ProcessKind::Descriptors,
    },
    ProcessFile {
        name: "maps",
        number: 6,
        kind: ProcessKind::File {
            permissions: 0o444,
            inspect: true,
            make: maps,
        },
    },
    ProcessFile {
        name: "stat",
        number: 7,
        kind: ProcessKind::File {
            permissions: 0o444,
            inspect: false,
            make: stat,
        },
    },
    ProcessFile {
        name: "status",
        number: 8,
        kind: ProcessKind::File {
            permissions: 0o444,
            inspect: false,
            make: status,
        },
    },
];

/// `make()`, or nothing once the process has exited (a zombie's `cmdline` is empty).
fn alive(process: &Process, make: impl FnOnce() -> Vec<u8>) -> Vec<u8> {
    if process.exit().is_some() {
        Vec::new()
    } else {
        make()
    }
}

/// `/proc/<pid>`.
fn process_directory(process: &Process) -> InodeRef {
    let id = process.id;
    let inode = u64::from(id) << 16;
    synthetic::directory(
        attributes(inode, 0o555, owner(process)),
        Box::new(ProcessDirectory(id)),
    )
}

struct ProcessDirectory(u32);

impl ProcessDirectory {
    fn make(&self, file: &ProcessFile) -> Result<InodeRef, Errno> {
        let id = self.0;
        let target = process(id)?;
        let inode = u64::from(id) << 16 | file.number;
        let owner = owner(&target);
        Ok(match file.kind {
            ProcessKind::File {
                permissions,
                inspect,
                make,
            } => synthetic::file(
                attributes(inode, permissions, owner),
                Box::new(move || {
                    let target = process(id)?;
                    if inspect {
                        may_inspect(&target)?;
                    }
                    make(&target).map(String::into_bytes)
                }),
            ),
            ProcessKind::Bytes {
                permissions,
                inspect,
                make,
            } => synthetic::file(
                attributes(inode, permissions, owner),
                Box::new(move || {
                    let target = process(id)?;
                    if inspect {
                        may_inspect(&target)?;
                    }
                    Ok(make(&target))
                }),
            ),
            ProcessKind::Link(make) => {
                synthetic::symlink(attributes(inode, 0o777, owner), inspected_link(id, make))
            }
            ProcessKind::Descriptors => {
                synthetic::directory(attributes(inode, 0o500, owner), Box::new(Descriptors(id)))
            }
        })
    }
}

/// A link to something private of process `id`: read only by those who may inspect it,
/// and gone once the process has exited (as Linux's `exe` and `cwd` of a zombie).
fn inspected_link(
    id: u32,
    make: impl Fn(&Process) -> Result<String, Errno> + Send + Sync + 'static,
) -> Target {
    Box::new(move || {
        let target = process(id)?;
        may_inspect(&target)?;
        if target.exit().is_some() {
            return Err(ENOENT);
        }
        make(&target)
    })
}

impl Listing for ProcessDirectory {
    fn entries(&self) -> Result<Vec<Entry>, Errno> {
        PROCESS_FILES
            .iter()
            .map(|file| {
                Ok(Entry {
                    cookie: file.number + 2,
                    name: file.name.to_string(),
                    inode: self.make(file)?,
                })
            })
            .collect()
    }

    fn lookup(&self, name: &str) -> Result<InodeRef, Errno> {
        let file = PROCESS_FILES
            .iter()
            .find(|file| file.name == name)
            .ok_or(ENOENT)?;
        self.make(file)
    }
}

/// `/proc/<pid>/fd`: a link per open descriptor, to where its file is.
struct Descriptors(u32);

impl Descriptors {
    /// The open descriptors as (number, link permissions, target).
    fn list(&self) -> Result<Vec<(u64, u32, String)>, Errno> {
        let target = process(self.0)?;
        may_inspect(&target)?;
        let files = target.files.lock();
        Ok(files
            .descriptors()
            .map(|(fd, descriptor)| {
                let file = &descriptor.file.file;
                // Linux: read and write permission as the file was opened, both with x.
                let permissions = match file.access_mode() {
                    flags::O_RDONLY => 0o500,
                    flags::O_WRONLY => 0o300,
                    _ => 0o700,
                };
                (fd, permissions, describe(file.as_ref()))
            })
            .collect())
    }

    fn link(&self, fd: u64, permissions: u32, target: String) -> InodeRef {
        let owner = process(self.0)
            .map(|process| owner(&process))
            .unwrap_or((0, 0));
        let inode = u64::from(self.0) << 16 | 0x1000 | fd;
        synthetic::symlink(
            attributes(inode, permissions, owner),
            inspected_link(self.0, move |_| Ok(target.clone())),
        )
    }
}

impl Listing for Descriptors {
    fn entries(&self) -> Result<Vec<Entry>, Errno> {
        Ok(self
            .list()?
            .into_iter()
            .map(|(fd, permissions, target)| Entry {
                cookie: fd + 3,
                name: fd.to_string(),
                inode: self.link(fd, permissions, target),
            })
            .collect())
    }

    fn lookup(&self, name: &str) -> Result<InodeRef, Errno> {
        let wanted: u64 = name.parse().map_err(|_| ENOENT)?;
        let (fd, permissions, target) = self
            .list()?
            .into_iter()
            .find(|&(fd, _, _)| fd == wanted)
            .ok_or(ENOENT)?;
        Ok(self.link(fd, permissions, target))
    }
}

/// What a descriptor's link shows: the file's path, `pipe:[inode]` for a pipe, or the
/// terminal's node for a terminal opened without a path (init's console).
fn describe(file: &dyn crate::file::File) -> String {
    if let Some(name) = file.path().and_then(|path| path.absolute_name().ok()) {
        return name;
    }
    let metadata = file.stat();
    if metadata.file_type() == mode::FIFO && metadata.device == PIPE_DEVICE {
        return format!("pipe:[{}]", metadata.inode);
    }
    match major_minor(metadata.rdev) {
        (5, 1) => "/dev/console".to_string(),
        (4, 64) => "/dev/ttyS0".to_string(),
        (5, 2) => "/dev/ptmx".to_string(),
        (136, index) => format!("/dev/pts/{index}"),
        _ => "anon_inode:[unknown]".to_string(),
    }
}

/// The state letter and name of `/proc/<pid>/stat` and `status`.
fn state(process: &Process) -> (char, &'static str) {
    if process.exit().is_some() {
        ('Z', "zombie")
    } else if process.is_stopped() {
        ('T', "stopped")
    } else if process::current().is_some_and(|current| current.id == process.id) {
        ('R', "running")
    } else {
        // Waiting or ready to run: there is no record of which, yet.
        ('S', "sleeping")
    }
}

/// The name in `stat` and `status`: the program's file name, at most 15 bytes (Linux's
/// `TASK_COMM_LEN`).
fn command(process: &Process) -> String {
    let name = process.name();
    let base = name.rsplit('/').next().unwrap_or(&name);
    let mut end = base.len().min(15);
    while !base.is_char_boundary(end) {
        end -= 1;
    }
    base[..end].to_string()
}

/// The memory in use, in bytes (size and resident: the same, as memory is allocated
/// eagerly).
fn memory_size(process: &Process) -> u64 {
    let Ok(space) = process.space() else {
        return 0;
    };
    let memory = process.memory.lock();
    memory
        .regions(&space)
        .iter()
        .map(|region| region.end - region.start)
        .sum()
}

/// `/proc/<pid>/stat`: 52 fields on one line (`man 5 proc_pid_stat`).
fn stat(process: &Process) -> Result<String, Errno> {
    let (state, _) = state(process);
    let terminal = process::controlling_tty(process);
    let tty_nr = terminal.as_ref().map_or(0, |tty| {
        let (major, minor) = major_minor(tty.device);
        // Linux's `new_encode_dev`.
        (minor & 0xff) | (major << 8) | ((minor & !0xff) << 12)
    });
    let foreground = terminal
        .as_ref()
        .and_then(|tty| tty.foreground())
        .map_or(-1, i64::from);
    let start = process.started / (NANOSECONDS_PER_SECOND / USER_HZ);
    let size = memory_size(process);
    let [pending, blocked, ignored, caught] = process.signals.lock().masks();
    let exit_code = process.exit().map_or(0, |exit| exit.wait_status());
    let mut line = format!(
        "{} ({}) {} {} {} {} {} {}",
        process.id,
        command(process),
        state,
        process.parent(),
        process.group(),
        process.session(),
        tty_nr,
        foreground,
    );
    // flags, minflt, cminflt, majflt, cmajflt, utime, stime, cutime, cstime; priority,
    // nice, num_threads, itrealvalue.
    line.push_str(" 0 0 0 0 0 0 0 0 0 20 0 1 0");
    let _ = write!(line, " {start} {size} {}", size / FRAME_SIZE);
    // rsslim (unlimited), startcode, endcode, startstack, kstkesp, kstkeip: no addresses.
    line.push_str(" 18446744073709551615 0 0 0 0 0");
    // The obsolete signal fields, then wchan, nswap, cnswap (wchan is never an address).
    let _ = write!(line, " {pending} {blocked} {ignored} {caught} 0 0 0");
    // exit_signal (SIGCHLD), processor, rt_priority, policy, delayacct_blkio_ticks,
    // guest_time, cguest_time, then the data, brk, argument and environment addresses.
    line.push_str(" 17 0 0 0 0 0 0 0 0 0 0 0 0 0");
    let _ = writeln!(line, " {exit_code}");
    Ok(line)
}

/// `/proc/<pid>/status`: the same, by name.
fn status(process: &Process) -> Result<String, Errno> {
    let (letter, state) = state(process);
    let credentials = process.credentials.lock().clone();
    let mut text = String::new();
    let _ = writeln!(text, "Name:\t{}", command(process));
    let umask = process.umask.load(core::sync::atomic::Ordering::Relaxed);
    let _ = writeln!(text, "Umask:\t{umask:04o}");
    let _ = writeln!(text, "State:\t{letter} ({state})");
    let _ = writeln!(text, "Tgid:\t{}", process.id);
    text.push_str("Ngid:\t0\n");
    let _ = writeln!(text, "Pid:\t{}", process.id);
    let _ = writeln!(text, "PPid:\t{}", process.parent());
    text.push_str("TracerPid:\t0\n");
    let user = credentials.effective_user;
    let group = credentials.effective_group;
    let _ = writeln!(
        text,
        "Uid:\t{}\t{user}\t{}\t{user}",
        credentials.user, credentials.saved_user
    );
    let _ = writeln!(
        text,
        "Gid:\t{}\t{group}\t{}\t{group}",
        credentials.group, credentials.saved_group
    );
    let highest = process
        .files
        .lock()
        .descriptors()
        .map(|(fd, _)| fd + 1)
        .max()
        .unwrap_or(0);
    let _ = writeln!(text, "FDSize:\t{}", highest.next_power_of_two().max(64));
    text.push_str("Groups:\t");
    for group in &credentials.groups {
        let _ = write!(text, "{group} ");
    }
    text.push('\n');
    if process.exit().is_none() {
        let kilobytes = memory_size(process) / 1024;
        let _ = writeln!(text, "VmSize:\t{kilobytes:8} kB");
        let _ = writeln!(text, "VmRSS:\t{kilobytes:8} kB");
    }
    text.push_str("Threads:\t1\n");
    let [pending, blocked, ignored, caught] = process.signals.lock().masks();
    let _ = writeln!(text, "SigPnd:\t{pending:016x}");
    text.push_str("ShdPnd:\t0000000000000000\n");
    let _ = writeln!(text, "SigBlk:\t{blocked:016x}");
    let _ = writeln!(text, "SigIgn:\t{ignored:016x}");
    let _ = writeln!(text, "SigCgt:\t{caught:016x}");
    Ok(text)
}

/// `/proc/<pid>/maps`: one line per run of pages (see `Memory::regions`).
fn maps(process: &Process) -> Result<String, Errno> {
    let mut text = String::new();
    let Ok(space) = process.space() else {
        return Ok(text);
    };
    let regions = process.memory.lock().regions(&space);
    for region in regions {
        let access = region.access;
        let flag = |on: bool, letter: char| if on { letter } else { '-' };
        let start = text.len();
        let _ = write!(
            text,
            "{:08x}-{:08x} {}{}{}p 00000000 00:00 0 ",
            region.start,
            region.end,
            flag(access.readable, 'r'),
            flag(access.writable, 'w'),
            flag(access.executable, 'x'),
        );
        if !region.name.is_empty() {
            // Linux pads the name to column 73.
            while text.len() - start < 72 {
                text.push(' ');
            }
            text.push(' ');
            text.push_str(region.name);
        }
        text.push('\n');
    }
    Ok(text)
}

/// `/proc/cpuinfo`: a block per CPU (the boot CPU's identity for all).
fn cpuinfo() -> Result<String, Errno> {
    let identity = cpu::identity();
    let trim = |bytes: &[u8]| {
        String::from_utf8_lossy(bytes)
            .trim_matches(|c: char| c == '\0' || c == ' ')
            .to_string()
    };
    let vendor = trim(&identity.vendor);
    let model_name = trim(&identity.brand);
    let cpus = time::cpus();
    let mut text = String::new();
    for index in 0..cpus {
        let _ = writeln!(text, "processor\t: {index}");
        let _ = writeln!(text, "vendor_id\t: {vendor}");
        let _ = writeln!(text, "cpu family\t: {}", identity.family);
        let _ = writeln!(text, "model\t\t: {}", identity.model);
        let _ = writeln!(text, "model name\t: {model_name}");
        let _ = writeln!(text, "stepping\t: {}", identity.stepping);
        text.push_str("physical id\t: 0\n");
        let _ = writeln!(text, "siblings\t: {cpus}");
        let _ = writeln!(text, "core id\t\t: {index}");
        let _ = writeln!(text, "cpu cores\t: {cpus}");
        text.push_str("fpu\t\t: yes\n\n");
    }
    Ok(text)
}

/// `/proc/filesystems`: the kinds of filesystems the kernel has (`nodev`: not on a disk).
fn filesystems() -> Result<String, Errno> {
    Ok(["tmpfs", "proc", "sysfs", "devpts"]
        .iter()
        .map(|kind| format!("nodev\t{kind}\n"))
        .collect())
}

/// `/proc/meminfo`: physical memory, in kB, as Linux formats it.
fn meminfo() -> Result<String, Errno> {
    let stats = frames::stats();
    let kilobytes = |frames: u64| frames * FRAME_SIZE / 1024;
    let mut text = String::new();
    for (name, value) in [
        ("MemTotal:", kilobytes(stats.total)),
        ("MemFree:", kilobytes(stats.free)),
        ("MemAvailable:", kilobytes(stats.free)),
        ("Buffers:", 0),
        ("Cached:", 0),
        ("SwapCached:", 0),
        ("SwapTotal:", 0),
        ("SwapFree:", 0),
    ] {
        let _ = writeln!(text, "{name:<16}{value:>8} kB");
    }
    Ok(text)
}

/// `/proc/mounts`: one line per mount, as `fstab` writes them (no mount options yet).
fn mounts_file() -> Result<String, Errno> {
    Ok(mounts()
        .into_iter()
        .map(|(kind, source, at)| format!("{source} {at} {kind} rw 0 0\n"))
        .collect())
}

/// `/proc/uptime`: seconds since boot, and seconds the CPUs were idle (not counted yet).
fn uptime() -> Result<String, Errno> {
    let hundredths = time::monotonic() / (NANOSECONDS_PER_SECOND / 100);
    Ok(format!(
        "{}.{:02} 0.00\n",
        hundredths / 100,
        hundredths % 100
    ))
}

/// `/proc/version`: the release `uname -r` gives, then what really runs.
fn version() -> Result<String, Errno> {
    Ok(format!(
        "Linux version {ABI_TARGET_RELEASE} ({VERSION}) #1 SMP\n"
    ))
}
