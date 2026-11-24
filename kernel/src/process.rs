// SPDX-License-Identifier: GPL-2.0-only

//! Processes: programs from files, each in its own address space, as in Linux.
//!
//! Starting a program ("exec", [`load_program`]):
//! 0. a file that starts with `#!` is a script: the program to run is the interpreter named
//!    on that line, with the script's path as an argument (`man 2 execve`, "Interpreter
//!    scripts");
//! 1. read the ELF file and map its segments (at a random address if it is position
//!    independent — ASLR);
//! 2. build the initial stack: the argument and environment strings, then, from the stack
//!    pointer up: `argc`, the `argv` pointers, `NULL`, the `envp` pointers, `NULL`, and the
//!    auxiliary vector ("auxv": pairs of type and value that tell the C library about the
//!    program and the system, `man 3 getauxval`);
//! 3. run it from the ELF entry point with that stack pointer.
//!
//! The life of a process (`man 2 fork`, `execve`, `wait4`, `_exit`):
//! - `fork` copies the parent: memory (every page, eagerly), open files, registers;
//! - `execve` replaces the program but keeps the process (its ID, parent, open files);
//! - when it exits, its memory and files are released at once, but it stays in the process
//!   table as a "zombie" until its parent collects the exit status with `wait4`;
//! - its children go to init (PID 1), which collects them ("reparenting"). Without an init
//!   (the user-program tests), orphans are collected by the kernel itself.
//! - init must never end: that is a kernel panic, as in Linux.
//!
//! Sessions and process groups (`man 7 credentials`, `man 2 setsid`): a process group is a
//! job (a pipeline the shell started); a session is the groups of one login. A session may
//! have a controlling terminal, whose foreground group gets the terminal's signals (Ctrl-C)
//! and may read from it. A group whose members' parents are all inside it or outside the
//! session is "orphaned": nobody could continue it, so it may not stay stopped.
//!
//! For now a process has one task (thread).

use alloc::collections::BTreeMap;
use alloc::string::String;
use alloc::sync::{Arc, Weak};
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, AtomicU32, Ordering};

use log::info;
use ochra_frame::mm::FRAME_SIZE;
use ochra_frame::mm::user::{USER_END, UserAccess, UserSpace};
use ochra_frame::random;
use ochra_frame::sync::IrqMutex;
use ochra_frame::task::{self, Task};
use ochra_frame::user_mode::UserContext;

use crate::elf::{self, Elf, Kind};
use crate::errno::{E2BIG, EACCES, ECHILD, ELOOP, ENOEXEC, ENOMEM, EPERM, ESRCH, Errno};
use crate::file::FdTable;
use crate::memory::{self, Memory, page_down, page_up};
use crate::sched::{self, WaitQueue};
use crate::signal::{self, SIGCHLD, Signals};
use crate::tty::Tty;
use crate::vfs::{self, Access, Credentials, Walk, may_access};

pub struct Process {
    pub id: u32,
    /// The parent's ID; 0 if the kernel started the process (or it was orphaned without
    /// an init to take it).
    parent: AtomicU32,
    /// The process group: inherited, changed by `setpgid` and `setsid`.
    group: AtomicU32,
    /// The session: inherited, changed by `setsid`.
    session: AtomicU32,
    /// The controlling terminal (inherited). It counts only while the terminal still belongs
    /// to this process's session: see [`controlling_tty`].
    pub controlling_tty: IrqMutex<Option<Arc<Tty>>>,
    /// Whether the process has called `execve` (its parent may no longer move it to another
    /// group, `setpgid` gives `EACCES`).
    executed: AtomicBool,
    /// Stopped by a signal, and what `wait4` has not reported yet.
    job: IrqMutex<Job>,
    /// Woken when the process is continued (or killed while stopped).
    resumed: WaitQueue,
    /// When it started, in monotonic nanoseconds (`/proc/<pid>/stat`).
    pub started: u64,
    /// The program file, for `/proc/<pid>/exe`.
    pub executable: IrqMutex<Option<vfs::Path>>,
    /// The arguments and the environment of the program, each string with its NUL
    /// (`/proc/<pid>/cmdline` and `environ`). Linux reads them from the process's memory;
    /// these are copies taken at `execve`.
    pub arguments: IrqMutex<Vec<u8>>,
    pub environment: IrqMutex<Vec<u8>>,
    /// The program's path, for the log.
    name: IrqMutex<String>,
    /// The current directory.
    pub cwd: IrqMutex<vfs::Path>,
    /// Who the process is, for permission checks; everyone is root until there are users.
    pub credentials: IrqMutex<Credentials>,
    /// Permission bits taken away from new files (`umask`).
    pub umask: AtomicU32,
    /// `None` once the process has exited.
    space: IrqMutex<Option<Arc<UserSpace>>>,
    pub memory: IrqMutex<Memory>,
    pub files: IrqMutex<FdTable>,
    /// How the process ended, once it has.
    exit: IrqMutex<Option<Exit>>,
    /// The parent is gone and nobody will `wait4`: the process is removed when it exits.
    orphan: AtomicBool,
    /// Woken when a child exits (`wait4` waits here).
    children_changed: WaitQueue,
    pub signals: IrqMutex<Signals>,
    /// The process's task (thread), for signals to wake it.
    task: IrqMutex<Weak<Task>>,
}

/// Job control state.
#[derive(Default)]
struct Job {
    stopped: bool,
    /// What the parent's `wait4` has yet to see.
    report: Option<Report>,
}

#[derive(Clone, Copy)]
enum Report {
    Stopped(u8),
    Continued,
}

/// How a process ended (what `wait4` reports).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Exit {
    /// It called `exit` with this status (0..=255).
    Status(u8),
    /// It was killed by this signal.
    Signal(u8),
}

impl Exit {
    /// The status word `wait4` writes (`WIFEXITED`, `WEXITSTATUS`, `WTERMSIG` decode it).
    pub fn wait_status(self) -> u32 {
        match self {
            Self::Status(status) => u32::from(status) << 8,
            Self::Signal(signal) => u32::from(signal),
        }
    }
}

impl core::fmt::Display for Exit {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Status(status) => write!(f, "exited with status {status}"),
            Self::Signal(signal) => write!(f, "killed by signal {signal}"),
        }
    }
}

impl Process {
    pub fn exit(&self) -> Option<Exit> {
        *self.exit.lock()
    }

    pub fn parent(&self) -> u32 {
        self.parent.load(Ordering::Relaxed)
    }

    pub fn name(&self) -> String {
        self.name.lock().clone()
    }

    pub fn group(&self) -> u32 {
        self.group.load(Ordering::Relaxed)
    }

    pub fn session(&self) -> u32 {
        self.session.load(Ordering::Relaxed)
    }

    /// Whether the process is stopped (`/proc/<pid>/stat`).
    pub fn is_stopped(&self) -> bool {
        self.job.lock().stopped
    }

    /// The address space; `ESRCH` once the process has exited (it never runs then).
    pub fn space(&self) -> Result<Arc<UserSpace>, Errno> {
        self.space.lock().clone().ok_or(ESRCH)
    }

    /// The process's task, while it exists.
    pub fn task(&self) -> Option<Arc<Task>> {
        self.task.lock().upgrade()
    }

    /// A new process record, not yet in the table and without a task.
    fn new(parent: u32, group: u32, session: u32, name: String, space: Arc<UserSpace>) -> Self {
        Self {
            id: NEXT_ID.fetch_add(1, Ordering::Relaxed),
            parent: AtomicU32::new(parent),
            group: AtomicU32::new(group),
            session: AtomicU32::new(session),
            controlling_tty: IrqMutex::new(None),
            executed: AtomicBool::new(false),
            job: IrqMutex::new(Job::default()),
            resumed: WaitQueue::new(),
            started: crate::time::monotonic(),
            executable: IrqMutex::new(None),
            arguments: IrqMutex::new(Vec::new()),
            environment: IrqMutex::new(Vec::new()),
            name: IrqMutex::new(name),
            cwd: IrqMutex::new(vfs::root()),
            credentials: IrqMutex::new(Credentials::root()),
            umask: AtomicU32::new(0o022),
            space: IrqMutex::new(Some(space)),
            memory: IrqMutex::new(Memory::new(0)),
            files: IrqMutex::new(FdTable::default()),
            exit: IrqMutex::new(None),
            orphan: AtomicBool::new(false),
            children_changed: WaitQueue::new(),
            signals: IrqMutex::new(Signals::new()),
            task: IrqMutex::new(Weak::new()),
        }
    }
}

/// Every process that exists, zombies included, by ID.
static TABLE: IrqMutex<BTreeMap<u32, Arc<Process>>> = IrqMutex::new(BTreeMap::new());
/// The ID of init, once started (0: none).
static INIT: AtomicU32 = AtomicU32::new(0);
/// Woken when a process the kernel started (parent 0) exits.
static KERNEL_CHILDREN: WaitQueue = WaitQueue::new();
static NEXT_ID: AtomicU32 = AtomicU32::new(1);

/// The process of the running task, if it is a user task with one.
pub fn current() -> Option<Arc<Process>> {
    let task = task::current()?;
    sched::thread(&task).process.clone()
}

/// A program loaded into a new address space, ready to run.
pub struct Image {
    space: Arc<UserSpace>,
    memory: Memory,
    entry: u64,
    stack_pointer: u64,
    executable: Option<vfs::Path>,
    arguments: Vec<u8>,
    environment: Vec<u8>,
}

/// Most interpreters in a row (a script whose interpreter is a script…), as in Linux.
const MAX_INTERPRETERS: usize = 4;
/// How much of a script's first line counts (Linux's `BINPRM_BUF_SIZE`).
const SCRIPT_LINE: usize = 256;

/// Loads the program at `path` (relative to `cwd`) with the given arguments and
/// environment, as `credentials` may.
pub fn load_program(
    cwd: &vfs::Path,
    credentials: &Credentials,
    path: &str,
    arguments: &[&[u8]],
    environment: &[&[u8]],
) -> Result<Image, Errno> {
    let mut arguments: Vec<Vec<u8>> = arguments.iter().map(|text| text.to_vec()).collect();
    let mut file_path = String::from(path);
    for _ in 0..=MAX_INTERPRETERS {
        let (file, found) = read_program(cwd, credentials, &file_path)?;
        let Some((interpreter, argument)) = script_interpreter(&file)? else {
            let arguments: Vec<&[u8]> = arguments.iter().map(Vec::as_slice).collect();
            let mut image = load_elf(&file, path, &arguments, environment)?;
            image.executable = Some(found);
            image.arguments = joined(&arguments);
            image.environment = joined(environment);
            return Ok(image);
        };
        // The interpreter gets: its own name, the optional argument from the `#!` line,
        // the script's path, then the original arguments after the first.
        let mut new_arguments = alloc::vec![interpreter.clone().into_bytes()];
        new_arguments.extend(argument.map(String::into_bytes));
        new_arguments.push(file_path.into_bytes());
        new_arguments.extend(arguments.into_iter().skip(1));
        arguments = new_arguments;
        file_path = interpreter;
    }
    Err(ELOOP)
}

/// Reads the whole program file at `path`: a regular file that `credentials` may execute.
/// Set-user-ID and set-group-ID bits are ignored until there are users (`docs/security.md`).
fn read_program(
    cwd: &vfs::Path,
    credentials: &Credentials,
    path: &str,
) -> Result<(Vec<u8>, vfs::Path), Errno> {
    let found = cwd.walk(path, Walk::Follow, credentials)?;
    let metadata = found.metadata();
    if !metadata.is_regular() {
        return Err(EACCES);
    }
    may_access(&metadata, credentials, Access::EXECUTE)?;
    let inode = found.inode();
    let cache = inode.page_cache().ok_or(EACCES)?;
    let size = usize::try_from(metadata.size).map_err(|_| ENOMEM)?;
    let mut file = alloc::vec![0; size];
    let read = cache.read(inode.as_ref(), 0, &mut file, metadata.size)?;
    file.truncate(read);
    vfs::touch_accessed(&found);
    Ok((file, found))
}

/// Strings, each followed by its NUL, one after another (`/proc/<pid>/cmdline`).
fn joined(strings: &[&[u8]]) -> Vec<u8> {
    let mut all = Vec::new();
    for text in strings {
        all.extend_from_slice(text);
        all.push(0);
    }
    all
}

/// For a script (`#!interpreter [argument]` on its first line): the interpreter and the
/// argument, which is the rest of the line (spaces inside included). `None` for any other
/// file.
fn script_interpreter(file: &[u8]) -> Result<Option<(String, Option<String>)>, Errno> {
    let Some(rest) = file.strip_prefix(b"#!") else {
        return Ok(None);
    };
    let head = &rest[..rest.len().min(SCRIPT_LINE - 2)];
    // A line longer than the buffer would cut the interpreter's name: refused, as Linux.
    let line = match head.iter().position(|&byte| byte == b'\n') {
        Some(end) => &head[..end],
        None if head.len() < rest.len() => return Err(ENOEXEC),
        None => head,
    };
    let line = core::str::from_utf8(line).map_err(|_| ENOEXEC)?;
    let line = line.trim_matches([' ', '\t']);
    let (interpreter, argument) = match line.split_once([' ', '\t']) {
        Some((interpreter, argument)) => (interpreter, Some(argument.trim_matches([' ', '\t']))),
        None => (line, None),
    };
    if interpreter.is_empty() {
        return Err(ENOEXEC);
    }
    let argument = argument.filter(|argument| !argument.is_empty());
    Ok(Some((
        String::from(interpreter),
        argument.map(String::from),
    )))
}

/// Loads the ELF program `file`; `path` is what `execve` was given (for `AT_EXECFN`).
fn load_elf(
    file: &[u8],
    path: &str,
    arguments: &[&[u8]],
    environment: &[&[u8]],
) -> Result<Image, Errno> {
    let elf = elf::parse(file)?;
    let space = Arc::new(UserSpace::new().ok_or(ENOMEM)?);
    let layout = Layout::random();
    let mut memory = Memory::new(layout.mmap_base);
    let loaded = load(&space, &mut memory, &elf, file, layout.pie_base)?;
    memory.set_brk_start(loaded.end + layout.brk_gap);
    let stack_pointer = build_stack(
        &space,
        &mut memory,
        layout.stack_top,
        path,
        arguments,
        environment,
        &loaded,
    )?;
    Ok(Image {
        space,
        memory,
        entry: loaded.entry,
        stack_pointer,
        executable: None,
        arguments: Vec::new(),
        environment: Vec::new(),
    })
}

/// Starts the program at `path` as a new process of the kernel (parent 0) with the given
/// open files.
pub fn spawn(
    path: &str,
    arguments: &[&[u8]],
    environment: &[&[u8]],
    files: FdTable,
) -> Result<Arc<Process>, Errno> {
    let image = load_program(
        &vfs::root(),
        &Credentials::root(),
        path,
        arguments,
        environment,
    )?;
    let process = Process::new(0, 0, 0, String::from(path), image.space.clone());
    *process.memory.lock() = image.memory;
    *process.files.lock() = files;
    *process.executable.lock() = image.executable;
    *process.arguments.lock() = image.arguments;
    *process.environment.lock() = image.environment;
    process.executed.store(true, Ordering::Relaxed);
    // A process started by the kernel leads its own session and group, as init does.
    process.group.store(process.id, Ordering::Relaxed);
    process.session.store(process.id, Ordering::Relaxed);
    let process = Arc::new(process);
    TABLE.lock().insert(process.id, process.clone());
    let task = sched::spawn_user(
        "user",
        image.space,
        image.entry,
        image.stack_pointer,
        Some(process.clone()),
    );
    *process.task.lock() = Arc::downgrade(&task);
    Ok(process)
}

/// Makes `process` the init process: orphans go to it, and its end is a kernel panic.
pub fn set_init(process: &Process) {
    INIT.store(process.id, Ordering::Relaxed);
}

/// `fork`: a copy of `parent` that continues from `context` with 0 as the result. Returns
/// the child's ID.
pub fn fork(parent: &Arc<Process>, context: &UserContext) -> Result<u32, Errno> {
    let space = Arc::new(parent.space()?.duplicate().ok_or(ENOMEM)?);
    let group = parent.group.load(Ordering::Relaxed);
    let session = parent.session.load(Ordering::Relaxed);
    let child = Process::new(parent.id, group, session, parent.name(), space.clone());
    *child.controlling_tty.lock() = parent.controlling_tty.lock().clone();
    *child.executable.lock() = parent.executable.lock().clone();
    *child.arguments.lock() = parent.arguments.lock().clone();
    *child.environment.lock() = parent.environment.lock().clone();
    *child.memory.lock() = parent.memory.lock().clone();
    *child.files.lock() = parent.files.lock().clone();
    *child.cwd.lock() = parent.cwd.lock().clone();
    *child.credentials.lock() = parent.credentials.lock().clone();
    child
        .umask
        .store(parent.umask.load(Ordering::Relaxed), Ordering::Relaxed);
    *child.signals.lock() = parent.signals.lock().for_child();
    let child = Arc::new(child);
    let id = child.id;
    TABLE.lock().insert(id, child.clone());
    let task = sched::spawn_fork(space, context, child.clone());
    *child.task.lock() = Arc::downgrade(&task);
    Ok(id)
}

/// `execve`: replaces the program of the current process. On success the caller's context
/// starts the new program; on failure nothing has changed.
pub fn exec(
    process: &Process,
    context: &mut UserContext,
    path: &str,
    arguments: &[&[u8]],
    environment: &[&[u8]],
) -> Result<(), Errno> {
    let cwd = process.cwd.lock().clone();
    let credentials = process.credentials.lock().clone();
    let image = load_program(&cwd, &credentials, path, arguments, environment)?;
    // From here on nothing can fail: the old program is gone.
    *process.memory.lock() = image.memory;
    *process.space.lock() = Some(image.space.clone());
    *process.name.lock() = String::from(path);
    *process.executable.lock() = image.executable;
    *process.arguments.lock() = image.arguments;
    *process.environment.lock() = image.environment;
    process.executed.store(true, Ordering::Relaxed);
    task::replace_space(image.space);
    task::set_fs_base(0).expect("0 is a user address");
    task::reset_fpu();
    let closed = process.files.lock().take_close_on_exec();
    drop(closed);
    process.signals.lock().reset_handlers();
    context.start_program(image.entry, image.stack_pointer);
    Ok(())
}

/// Ends the current process: releases its memory and files, hands its children to init,
/// tells its parent (`SIGCHLD`), and ends the task. The process stays a zombie until reaped,
/// unless the parent said it will not reap (`SIGCHLD` ignored, or `SA_NOCLDWAIT`).
///
/// It takes the caller's `Arc`: this function never returns, so a reference the caller kept
/// would never be dropped, and the process record would stay on the heap forever.
pub fn exit(process: Arc<Process>, exit: Exit) -> ! {
    let id = process.id;
    if INIT.load(Ordering::Relaxed) == id {
        panic!("init (process {id}) {exit}");
    }
    info!("process {id} ({}) {exit}", process.name());
    // A session leader's exit hangs up its controlling terminal (`man 2 _exit`).
    if process.session() == id
        && let Some(tty) = controlling_tty(&process)
    {
        tty.disassociate(true);
    }
    // Closing the files may wake others (a pipe's other end), so not under the lock.
    let files = core::mem::take(&mut *process.files.lock());
    drop(files);
    *process.memory.lock() = Memory::new(0);
    // The task still runs in the address space; it is freed with the task.
    let space = process.space.lock().take();
    drop(space);
    *process.exit.lock() = Some(exit);

    let init = INIT.load(Ordering::Relaxed);
    let mut orphaned = Vec::new();
    let parent = {
        let mut table = TABLE.lock();
        let mut orphans_exited = false;
        let mut reaped = Vec::new();
        let mut children = Vec::new();
        for (&child_id, child) in table.iter() {
            if child.parent() != id {
                continue;
            }
            children.push(child_id);
            child.parent.store(init, Ordering::Relaxed);
            if init == 0 {
                child.orphan.store(true, Ordering::Relaxed);
                if child.exit().is_some() {
                    reaped.push(child_id);
                }
            } else if child.exit().is_some() {
                orphans_exited = true;
            }
        }
        for child_id in reaped {
            table.remove(&child_id);
        }
        if orphans_exited && let Some(init) = table.get(&init) {
            init.children_changed.wake_all();
        }
        hang_up_orphans(&table, &process, &children, &mut orphaned);
        let parent = table.get(&process.parent()).cloned();
        if parent
            .as_ref()
            .is_some_and(|parent| parent.signals.lock().reaps_children())
        {
            process.orphan.store(true, Ordering::Relaxed);
        }
        if process.orphan.load(Ordering::Relaxed) {
            table.remove(&id);
        }
        parent
    };
    // Groups this exit orphaned, with stopped members: they would stay stopped forever, so
    // they get `SIGHUP` (which usually ends them) and `SIGCONT` (POSIX, `man 2 _exit`).
    for group in orphaned {
        signal::send_to_group(group, signal::SIGHUP, signal::from_kernel());
        signal::send_to_group(group, signal::SIGCONT, signal::from_kernel());
    }
    match parent {
        Some(parent) => {
            signal::send(&parent, SIGCHLD, signal::child_info(id, exit));
            parent.children_changed.wake_all();
        }
        None => KERNEL_CHILDREN.wake_all(),
    }
    // The record lives on while the table (a zombie) or the task's `Thread` holds it.
    drop(process);
    sched::exit_current()
}

/// Which children `wait4` waits for.
#[derive(Clone, Copy)]
pub enum Selector {
    Any,
    Process(u32),
    Group(u32),
}

/// What `wait4` reports besides exits (`WUNTRACED`, `WCONTINUED`).
#[derive(Clone, Copy, Default)]
pub struct WaitFor {
    pub stopped: bool,
    pub continued: bool,
}

/// `wait4`: waits for a child of `process` that matches `selector` to exit (or stop or
/// continue, if `also` asks), and returns its ID and status word. An exited child is removed
/// from the table. With `no_hang`, returns `None` at once if nothing happened. A signal ends
/// the wait (`ERESTARTSYS`).
pub fn wait(
    process: &Process,
    selector: Selector,
    also: WaitFor,
    no_hang: bool,
) -> Result<Option<(u32, u32)>, Errno> {
    process.children_changed.wait_interruptible(|| {
        let mut table = TABLE.lock();
        let matches = |child: &Process| {
            child.parent() == process.id
                && match selector {
                    Selector::Any => true,
                    Selector::Process(id) => child.id == id,
                    Selector::Group(group) => child.group.load(Ordering::Relaxed) == group,
                }
        };
        let mut any = false;
        let mut found = None;
        for child in table.values() {
            if !matches(child) {
                continue;
            }
            any = true;
            if let Some(exit) = child.exit() {
                found = Some((child.id, exit.wait_status(), true));
                break;
            }
            let mut job = child.job.lock();
            let status = match job.report {
                // As the `W*` macros decode it: 0x7f marks "stopped", 0xffff "continued".
                Some(Report::Stopped(signal)) if also.stopped => u32::from(signal) << 8 | 0x7f,
                Some(Report::Continued) if also.continued => 0xffff,
                _ => continue,
            };
            job.report = None;
            found = Some((child.id, status, false));
            break;
        }
        match found {
            Some((id, status, exited)) => {
                if exited {
                    table.remove(&id);
                }
                Some(Ok(Some((id, status))))
            }
            None if !any => Some(Err(ECHILD)),
            None if no_hang => Some(Ok(None)),
            None => None,
        }
    })?
}

/// Stops the current process (a stop signal with the default action) until it is continued
/// or killed. Its parent hears of it through `SIGCHLD` (unless `SA_NOCLDSTOP`) and `wait4`.
pub fn stop(process: &Arc<Process>, signal: u8) {
    {
        let mut job = process.job.lock();
        job.stopped = true;
        job.report = Some(Report::Stopped(signal));
    }
    info!(
        "process {} ({}) stopped by signal {signal}",
        process.id,
        process.name()
    );
    notify_parent(process, signal::child_stopped(process.id, Some(signal)));
    // Not interruptible: other signals wait until `SIGCONT`; only `SIGKILL` ends a stop.
    process.resumed.wait_until(|| {
        let stopped = process.job.lock().stopped;
        (!stopped || process.signals.lock().kill_pending()).then_some(())
    });
}

/// `SIGCONT` was sent: a stopped process runs again, and its parent hears of it.
pub fn resume(process: &Process) {
    let was_stopped = {
        let mut job = process.job.lock();
        let was_stopped = job.stopped;
        if was_stopped {
            job.stopped = false;
            job.report = Some(Report::Continued);
        }
        was_stopped
    };
    if was_stopped {
        info!("process {} ({}) continued", process.id, process.name());
        process.resumed.wake_all();
        notify_parent(process, signal::child_stopped(process.id, None));
    }
}

/// Tells the parent that `process` stopped or continued.
fn notify_parent(process: &Process, info: signal::Info) {
    let parent = TABLE.lock().get(&process.parent()).cloned();
    if let Some(parent) = parent {
        let quiet = parent.signals.lock().action(SIGCHLD).flags & signal::flags::SA_NOCLDSTOP;
        if quiet == 0 {
            signal::send(&parent, SIGCHLD, info);
        }
        parent.children_changed.wake_all();
    }
}

/// Whether process group `group` is orphaned (or will be once process `ignore` is gone):
/// no living member has a living parent in another group of the same session. Init does
/// not count as such a parent (Linux's `will_become_orphaned_pgrp`).
pub fn is_orphaned_group(group: u32, ignore: Option<u32>) -> bool {
    orphaned_in(&TABLE.lock(), group, ignore)
}

fn orphaned_in(table: &BTreeMap<u32, Arc<Process>>, group: u32, ignore: Option<u32>) -> bool {
    let init = INIT.load(Ordering::Relaxed);
    !table.values().any(|member| {
        Some(member.id) != ignore
            && member.exit().is_none()
            && member.group() == group
            && member.parent() != init
            && table.get(&member.parent()).is_some_and(|parent| {
                parent.exit().is_none()
                    && parent.group() != group
                    && parent.session() == member.session()
            })
    })
}

fn has_stopped_members(table: &BTreeMap<u32, Arc<Process>>, group: u32) -> bool {
    table.values().any(|member| {
        member.group() == group && member.exit().is_none() && member.job.lock().stopped
    })
}

/// The groups that `exiting`'s exit leaves orphaned with stopped members (Linux's
/// `kill_orphaned_pgrp`): its own group, if its parent was that group's link to the
/// session, and the groups of its children (already given to their new parent).
fn hang_up_orphans(
    table: &BTreeMap<u32, Arc<Process>>,
    exiting: &Process,
    children: &[u32],
    groups: &mut Vec<u32>,
) {
    let mut candidates = Vec::new();
    if let Some(parent) = table.get(&exiting.parent())
        && parent.group() != exiting.group()
        && parent.session() == exiting.session()
        && orphaned_in(table, exiting.group(), Some(exiting.id))
    {
        candidates.push(exiting.group());
    }
    for child in children.iter().filter_map(|id| table.get(id)) {
        if child.group() != exiting.group()
            && child.session() == exiting.session()
            && orphaned_in(table, child.group(), None)
        {
            candidates.push(child.group());
        }
    }
    for group in candidates {
        if !groups.contains(&group) && has_stopped_members(table, group) {
            groups.push(group);
        }
    }
}

/// The controlling terminal of `process`, if it has one that still belongs to its session.
pub fn controlling_tty(process: &Process) -> Option<Arc<Tty>> {
    let tty = process.controlling_tty.lock().clone()?;
    (tty.session() == Some(process.session())).then_some(tty)
}

/// The process with ID `id` (zombies too).
pub fn find(id: u32) -> Option<Arc<Process>> {
    TABLE.lock().get(&id).cloned()
}

/// `setsid`: `process` starts a new session and group, both with its own ID, without a
/// controlling terminal. `EPERM` if it already leads a group (its ID would then name two
/// groups).
pub fn new_session(process: &Process) -> Result<u32, Errno> {
    let table = TABLE.lock();
    if table.values().any(|other| other.group() == process.id) {
        return Err(EPERM);
    }
    process.group.store(process.id, Ordering::Relaxed);
    process.session.store(process.id, Ordering::Relaxed);
    *process.controlling_tty.lock() = None;
    Ok(process.id)
}

/// `setpgid(pid, pgid)` with both already defaulted, as Linux checks it: the target is the
/// caller or a child of it that has not called `execve`, in the same session and not a
/// session leader; the group is its own ID or an existing group of the session.
pub fn set_group(caller: &Process, target: u32, group: u32) -> Result<(), Errno> {
    let table = TABLE.lock();
    let process = table.get(&target).ok_or(ESRCH)?;
    if process.id != caller.id {
        if process.parent() != caller.id {
            return Err(ESRCH);
        }
        if process.session() != caller.session() {
            return Err(EPERM);
        }
        if process.executed.load(Ordering::Relaxed) {
            return Err(EACCES);
        }
    }
    if process.session() == process.id {
        return Err(EPERM);
    }
    if group != process.id
        && !table
            .values()
            .any(|other| other.group() == group && other.session() == caller.session())
    {
        return Err(EPERM);
    }
    process.group.store(group, Ordering::Relaxed);
    Ok(())
}

/// Whether any process in session `session` leads or belongs to group `group`.
pub fn group_in_session(group: u32, session: u32) -> Option<bool> {
    let table = TABLE.lock();
    let mut members = table.values().filter(|other| other.group() == group);
    let first = members.next()?;
    Some(first.session() == session || members.any(|other| other.session() == session))
}

/// The process group of `process`, for `wait4(0, …)`.
pub fn group(process: &Process) -> u32 {
    process.group.load(Ordering::Relaxed)
}

/// Whether `process` is init.
pub fn is_init(process: &Process) -> bool {
    INIT.load(Ordering::Relaxed) == process.id
}

/// Every process (zombies too) for which `filter` holds.
pub fn matching(filter: impl Fn(&Process) -> bool) -> Vec<Arc<Process>> {
    let table = TABLE.lock();
    table
        .values()
        .filter(|process| filter(process))
        .cloned()
        .collect()
}

/// Waits until `process`, started by the kernel, exits, and removes it from the table.
#[cfg_attr(
    not(feature = "selftest"),
    expect(
        dead_code,
        reason = "only the user-program tests wait for kernel children"
    )
)]
pub fn wait_kernel_child(process: &Process) -> Exit {
    let exit = KERNEL_CHILDREN.wait_until(|| process.exit());
    TABLE.lock().remove(&process.id);
    exit
}

/// Where things go in a new address space; random within the ranges Linux uses on x86-64,
/// so addresses look familiar in a debugger.
struct Layout {
    pie_base: u64,
    stack_top: u64,
    mmap_base: u64,
    brk_gap: u64,
}

impl Layout {
    fn random() -> Self {
        // Linux's TASK_SIZE: the last user page is never mapped.
        const TOP: u64 = USER_END - FRAME_SIZE;
        // Random page counts: 28 bits (1 TiB) for the program and mmap, 22 bits (16 GiB)
        // for the stack, as Linux's defaults (`mmap_rnd_bits`, `STACK_RND_MASK`).
        let pages = |bits: u32| (random::u64() & ((1 << bits) - 1)) * FRAME_SIZE;
        let stack_randomness = 1 << (22 + 12);
        // ELF_ET_DYN_BASE: two thirds of the way up, far from both NULL and the stack.
        let pie_base = page_down(TOP / 3 * 2) + pages(28);
        let stack_top = TOP - pages(22);
        // Room for the largest stack plus its random shift, at least 128 MiB (as Linux).
        let gap = (memory::STACK_LIMIT + stack_randomness).max(128 * 1024 * 1024);
        let mmap_base = TOP - gap - pages(28);
        // The heap starts up to 32 MiB past the program (Linux's `arch_randomize_brk`).
        let brk_gap = pages(13);
        Self {
            pie_base,
            stack_top,
            mmap_base,
            brk_gap,
        }
    }
}

/// What the loader found out, for the stack and the task.
struct Loaded {
    entry: u64,
    /// Where the program headers are in memory (0 if no segment loads them).
    header_address: u64,
    header_count: u16,
    /// The first page past the program.
    end: u64,
}

/// Maps the ELF segments into `space`, with their permissions, and copies their bytes.
fn load(
    space: &UserSpace,
    memory: &mut Memory,
    elf: &Elf,
    file: &[u8],
    pie_base: u64,
) -> Result<Loaded, Errno> {
    let bias = match elf.kind {
        Kind::Fixed => 0,
        Kind::PositionIndependent => pie_base - page_down(elf.segments[0].address),
    };
    // The end of the previous segment's last page, and its permissions: two segments may
    // share a page, which then gets both permissions.
    let mut previous: Option<(u64, UserAccess)> = None;
    for segment in &elf.segments {
        let access = UserAccess {
            readable: true,
            writable: segment.writable,
            executable: segment.executable,
        };
        let start = page_down(segment.address + bias);
        let end = page_up(segment.address + bias + segment.memory_size).ok_or(ENOEXEC)?;
        let mut first_new_page = start;
        if let Some((previous_end, previous_access)) = previous
            && start < previous_end
        {
            let shared = UserAccess {
                readable: true,
                writable: access.writable || previous_access.writable,
                executable: access.executable || previous_access.executable,
            };
            // W^X holds for shared pages too.
            if shared.writable && shared.executable {
                return Err(ENOEXEC);
            }
            space.protect(start, 1, shared);
            first_new_page = previous_end;
        }
        if first_new_page < end {
            memory
                .map_zeroed(space, first_new_page, end, access)
                .map_err(|_| ENOEXEC)?;
        }
        let bytes = &file[segment.offset as usize..][..segment.file_size as usize];
        space
            .write_bytes(segment.address + bias, bytes)
            .map_err(|_| ENOEXEC)?;
        previous = Some((end, access));
    }
    let end = previous.map_or(0, |(end, _)| end);
    Ok(Loaded {
        entry: elf.entry + bias,
        header_address: elf.header_address.map_or(0, |address| address + bias),
        header_count: elf.header_count,
        end,
    })
}

/// Auxiliary vector types (`include/uapi/linux/auxvec.h`).
mod auxv {
    pub const NULL: u64 = 0;
    pub const PHDR: u64 = 3;
    pub const PHENT: u64 = 4;
    pub const PHNUM: u64 = 5;
    pub const PAGESZ: u64 = 6;
    pub const BASE: u64 = 7;
    pub const FLAGS: u64 = 8;
    pub const ENTRY: u64 = 9;
    pub const UID: u64 = 11;
    pub const EUID: u64 = 12;
    pub const GID: u64 = 13;
    pub const EGID: u64 = 14;
    pub const CLKTCK: u64 = 17;
    pub const SECURE: u64 = 23;
    pub const RANDOM: u64 = 25;
    pub const EXECFN: u64 = 31;
}

/// The stack mapped at start, besides what the arguments take; the rest grows on demand.
const INITIAL_STACK: u64 = 64 * 1024;

/// Writes the initial stack below `top` and returns the stack pointer for the program.
fn build_stack(
    space: &UserSpace,
    memory: &mut Memory,
    top: u64,
    path: &str,
    arguments: &[&[u8]],
    environment: &[&[u8]],
    loaded: &Loaded,
) -> Result<u64, Errno> {
    let mut stack = StackBuilder {
        bytes: Vec::new(),
        top,
    };
    // Eight zero bytes at the very top, then the strings, each with its NUL.
    stack.push(&[0; 8]);
    let execfn = stack.push_string(path.as_bytes());
    let environment_addresses: Vec<u64> = environment
        .iter()
        .map(|text| stack.push_string(text))
        .collect();
    let argument_addresses: Vec<u64> = arguments
        .iter()
        .map(|text| stack.push_string(text))
        .collect();
    let mut random_bytes = [0; 16];
    random::fill(&mut random_bytes);
    let random_address = stack.push(&random_bytes);
    // A random shift of up to 8 KiB, then 16-byte alignment (Linux's `arch_align_stack`).
    let shifted = stack.push(&alloc::vec![0; (random::u64() % 8192) as usize]);
    stack.push(&alloc::vec![0; (shifted % 16) as usize]);

    let auxiliary = [
        (auxv::PHDR, loaded.header_address),
        (auxv::PHENT, u64::from(elf::PROGRAM_HEADER_SIZE)),
        (auxv::PHNUM, u64::from(loaded.header_count)),
        (auxv::PAGESZ, FRAME_SIZE),
        // No interpreter, so no base address of one.
        (auxv::BASE, 0),
        (auxv::FLAGS, 0),
        (auxv::ENTRY, loaded.entry),
        (auxv::UID, 0),
        (auxv::EUID, 0),
        (auxv::GID, 0),
        (auxv::EGID, 0),
        (auxv::CLKTCK, u64::from(crate::time::HZ)),
        (auxv::SECURE, 0),
        (auxv::RANDOM, random_address),
        (auxv::EXECFN, execfn),
        (auxv::NULL, 0),
    ];
    let mut words: Vec<u64> = Vec::new();
    words.push(arguments.len() as u64);
    words.extend(&argument_addresses);
    words.push(0);
    words.extend(&environment_addresses);
    words.push(0);
    for (kind, value) in auxiliary {
        words.extend([kind, value]);
    }
    // The stack pointer must be 16-byte aligned at the entry point (where `argc` is).
    if words.len() % 2 == 1 {
        stack.push(&[0; 8]);
    }
    let word_bytes: Vec<u8> = words.iter().flat_map(|word| word.to_le_bytes()).collect();
    let stack_pointer = stack.push(&word_bytes);

    // Linux allows a quarter of the stack limit for arguments and environment.
    if top - stack_pointer > memory::STACK_LIMIT / 4 {
        return Err(E2BIG);
    }
    let bottom = page_down(stack_pointer) - INITIAL_STACK;
    memory.map_stack(space, bottom, top)?;
    space
        .write_bytes(stack_pointer, &stack.bytes)
        .map_err(|_| ENOMEM)?;
    Ok(stack_pointer)
}

/// The initial stack's contents, built downwards from `top`.
struct StackBuilder {
    /// What goes at `top - bytes.len()..top`.
    bytes: Vec<u8>,
    top: u64,
}

impl StackBuilder {
    /// Puts `data` below everything pushed so far; returns its user address.
    fn push(&mut self, data: &[u8]) -> u64 {
        self.bytes.splice(0..0, data.iter().copied());
        self.top - self.bytes.len() as u64
    }

    fn push_string(&mut self, text: &[u8]) -> u64 {
        self.push(&[0]);
        self.push(text)
    }
}
