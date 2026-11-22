// SPDX-License-Identifier: GPL-2.0-only

//! Processes: a program from the initramfs running in its own address space.
//!
//! Starting one ("exec") works like Linux's `execve`:
//! 1. read the ELF file and map its segments (at a random address if it is position
//!    independent — ASLR);
//! 2. build the initial stack: the argument and environment strings, then, from the stack
//!    pointer up: `argc`, the `argv` pointers, `NULL`, the `envp` pointers, `NULL`, and the
//!    auxiliary vector ("auxv": pairs of type and value that tell the C library about the
//!    program and the system, `man 3 getauxval`);
//! 3. start a user task at the ELF entry point with that stack pointer.
//!
//! For now a process has one task (thread), and it cannot `fork` yet (stage 2, P3).

use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicU32, Ordering};

use ochra_frame::mm::FRAME_SIZE;
use ochra_frame::mm::user::{USER_END, UserAccess, UserSpace};
use ochra_frame::random;
use ochra_frame::sync::IrqMutex;
use ochra_frame::task;

use crate::elf::{self, Elf, Kind};
use crate::errno::{E2BIG, ENOENT, ENOEXEC, ENOMEM, Errno};
use crate::initramfs;
use crate::memory::{self, Memory, page_down, page_up};

pub struct Process {
    pub id: u32,
    /// The program's path, for the log.
    pub name: String,
    pub space: Arc<UserSpace>,
    pub memory: IrqMutex<Memory>,
    /// If set, what the program writes to stdout is collected here instead of going to the
    /// console (the user-program tests compare it with the same program run on Linux).
    pub captured_output: Option<IrqMutex<Vec<u8>>>,
    /// How the process ended, once it has.
    exit: IrqMutex<Option<Exit>>,
}

/// How a process ended (what `wait4` will report).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Exit {
    /// It called `exit` with this status (0..=255).
    Status(u8),
    /// It was killed by this signal.
    Signal(u8),
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
    #[cfg_attr(
        not(feature = "selftest"),
        expect(
            dead_code,
            reason = "only the user-program tests wait for a process until wait4 (P3)"
        )
    )]
    pub fn exit(&self) -> Option<Exit> {
        *self.exit.lock()
    }

    pub fn set_exit(&self, exit: Exit) {
        *self.exit.lock() = Some(exit);
    }
}

/// The process of the running task, if it is a user task.
pub fn current() -> Option<Arc<Process>> {
    task::current()?.data()?.clone().downcast::<Process>().ok()
}

static NEXT_ID: AtomicU32 = AtomicU32::new(1);

/// Starts the program at `path` with the given arguments and environment.
pub fn spawn(
    path: &str,
    arguments: &[&str],
    environment: &[&str],
    capture_output: bool,
) -> Result<Arc<Process>, Errno> {
    let file = initramfs::lookup(path).ok_or(ENOENT)?;
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
    let process = Arc::new(Process {
        id: NEXT_ID.fetch_add(1, Ordering::Relaxed),
        name: String::from(path),
        space: space.clone(),
        memory: IrqMutex::new(memory),
        captured_output: capture_output.then(|| IrqMutex::new(Vec::new())),
        exit: IrqMutex::new(None),
    });
    crate::sched::spawn_user(
        "user",
        space,
        loaded.entry,
        stack_pointer,
        Some(process.clone()),
    );
    Ok(process)
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
    arguments: &[&str],
    environment: &[&str],
    loaded: &Loaded,
) -> Result<u64, Errno> {
    let mut stack = StackBuilder {
        bytes: Vec::new(),
        top,
    };
    // Eight zero bytes at the very top, then the strings, each with its NUL.
    stack.push(&[0; 8]);
    let execfn = stack.push_string(path);
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

    fn push_string(&mut self, text: &str) -> u64 {
        self.push(&[0]);
        self.push(text.as_bytes())
    }
}
