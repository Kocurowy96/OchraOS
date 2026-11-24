// SPDX-License-Identifier: GPL-2.0-only

//! Open files and file descriptors, as in Unix:
//!
//! - a [`File`] is the thing itself (the console, one end of a pipe…);
//! - an [`OpenFile`] is one opening of it, with its own flags (`O_NONBLOCK`); `dup` and
//!   `fork` share it;
//! - a file descriptor is a small number in a process's [`FdTable`] that refers to an
//!   `OpenFile`, plus its own "close on exec" flag.
//!
//! A file is closed for real when the last descriptor referring to it, in any process, is
//! closed: then its `Arc` is dropped (a pipe end, for example, then tells the other end).

use alloc::collections::VecDeque;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicU32, Ordering};

use ochra_frame::sync::IrqMutex;

use crate::errno::{EAGAIN, EBADF, EINVAL, EMFILE, ENOSPC, ENOTDIR, ENOTTY, EPIPE, ESPIPE, Errno};
use crate::sched::WaitQueue;
use crate::vfs::{Credentials, Metadata, NewNode, Path, Timestamp, mode};

/// Open flags (`include/uapi/asm-generic/fcntl.h`), in octal as there.
pub mod flags {
    pub const O_RDONLY: u32 = 0;
    pub const O_WRONLY: u32 = 1;
    pub const O_RDWR: u32 = 2;
    pub const O_APPEND: u32 = 0o2000;
    pub const O_NONBLOCK: u32 = 0o4000;
    pub const O_CLOEXEC: u32 = 0o2_000_000;
}

pub trait File: Send + Sync {
    /// Reads into `buffer`; 0 means end of file. Blocks until something can be read, unless
    /// `nonblocking` (then `EAGAIN`).
    fn read(&self, _buffer: &mut [u8], _nonblocking: bool) -> Result<usize, Errno> {
        Err(EBADF)
    }

    /// Writes from `data`; returns how much was taken.
    fn write(&self, _data: &[u8], _nonblocking: bool) -> Result<usize, Errno> {
        Err(EBADF)
    }

    /// `O_RDONLY`, `O_WRONLY` or `O_RDWR`.
    fn access_mode(&self) -> u32;

    /// An `ioctl` the syscall layer does not answer itself; `ENOTTY` ("not a terminal": the
    /// request does not apply to this kind of file) by default.
    fn ioctl(&self, _request: u64, _argument: u64) -> Result<u64, Errno> {
        Err(ENOTTY)
    }

    /// What `poll` would report now (`events::*`). Files that never block (regular files,
    /// `/dev/null`) are always readable and writable.
    fn poll(&self) -> u32 {
        events::IN | events::RDNORM | events::OUT | events::WRNORM
    }

    /// The queues woken when [`poll`](Self::poll)'s answer may change.
    fn wait_queues<'a>(&'a self, _queues: &mut Vec<&'a WaitQueue>) {}

    /// Whether writes go to the end of the file (`O_APPEND`).
    fn append(&self) -> bool {
        false
    }

    /// What `fstat` reports.
    fn stat(&self) -> Metadata;

    /// Where the file is in the tree, if it was opened through a path (for `fchmod`,
    /// `fchown`, `futimens`, `fchdir`).
    fn path(&self) -> Option<Path> {
        None
    }

    /// Moves the file position (`lseek`); files without one (pipes, terminals) refuse.
    fn seek(&self, _offset: i64, _whence: u32) -> Result<u64, Errno> {
        Err(ESPIPE)
    }

    /// Reads at `offset` without moving the file position (`pread64`).
    fn read_at(&self, _buffer: &mut [u8], _offset: u64) -> Result<usize, Errno> {
        Err(ESPIPE)
    }

    /// Writes at `offset` without moving the file position (`pwrite64`).
    fn write_at(&self, _data: &[u8], _offset: u64) -> Result<usize, Errno> {
        Err(ESPIPE)
    }

    /// For a directory: where it is (`openat` relative to it, `fchdir`).
    fn directory(&self) -> Option<Path> {
        None
    }

    /// For a directory: `linux_dirent64` records that fit in `size` bytes (`getdents64`).
    fn read_directory(&self, _size: usize) -> Result<Vec<u8>, Errno> {
        Err(ENOTDIR)
    }

    /// Sets the size of a regular file opened for writing (`ftruncate`).
    fn truncate(&self, _size: u64) -> Result<(), Errno> {
        Err(EINVAL)
    }
}

/// `poll` events (`include/uapi/asm-generic/poll.h`).
pub mod events {
    pub const IN: u32 = 0x1;
    pub const PRI: u32 = 0x2;
    pub const OUT: u32 = 0x4;
    pub const ERR: u32 = 0x8;
    pub const HUP: u32 = 0x10;
    pub const NVAL: u32 = 0x20;
    pub const RDNORM: u32 = 0x40;
    pub const WRNORM: u32 = 0x100;
}

/// The metadata of a file that is not in any filesystem (a pipe, the console before
/// `/dev` existed): only what is given, the rest zero.
pub fn anonymous_metadata(mode: u32, rdev: u64, device: u64, inode: u64) -> Metadata {
    Metadata {
        inode,
        device,
        mode,
        links: 1,
        rdev,
        ..Metadata::default()
    }
}

/// The device a character device node stands for, by its number (Linux's numbers,
/// `Documentation/admin-guide/devices.txt`), opened with `open_flags`; `ENXIO` for one that
/// does not exist.
pub fn open_device(rdev: u64, open_flags: u32) -> Result<Arc<dyn File>, Errno> {
    match major_minor(rdev) {
        (1, 3) => Ok(Arc::new(Null)),
        (1, 5) => Ok(Arc::new(Zero { full: false })),
        (1, 7) => Ok(Arc::new(Zero { full: true })),
        (1, 8 | 9) => Ok(Arc::new(Random)),
        (major, minor) => crate::tty::open_device(major, minor, open_flags),
    }
}

/// Creates the device nodes in the directory `dev`, with Linux's numbers and permissions.
pub fn create_device_nodes(dev: &Path) {
    let credentials = Credentials::root();
    let nodes = [
        ("null", device(1, 3), 0o666),
        ("zero", device(1, 5), 0o666),
        ("full", device(1, 7), 0o666),
        ("random", device(1, 8), 0o666),
        ("urandom", device(1, 9), 0o666),
        ("tty", device(5, 0), 0o666),
        ("console", device(5, 1), 0o600),
        ("ptmx", device(5, 2), 0o666),
        ("ttyS0", device(4, 64), 0o660),
    ];
    for (name, rdev, permissions) in nodes {
        if let Err(error) = dev.create(
            name,
            NewNode::CharacterDevice(rdev),
            permissions,
            &credentials,
        ) {
            log::error!("cannot create /dev/{name} (errno {error})");
        }
    }
}

/// The device number (`st_dev`) of the internal filesystem of pipes, as Linux's pipefs.
pub const PIPE_DEVICE: u64 = 0x0d;

/// A device number from its major and minor numbers, as glibc's `makedev` packs them: the
/// low 8 bits of the minor, 12 bits of the major, then the rest of the minor and the major.
pub const fn device(major: u64, minor: u64) -> u64 {
    (minor & 0xff) | (major & 0xfff) << 8 | (minor & !0xff) << 12 | (major & !0xfff) << 32
}

/// The major and minor numbers of a device number (`major()`, `minor()`).
pub const fn major_minor(device: u64) -> (u64, u64) {
    let major = (device >> 8) & 0xfff | (device >> 32) & !0xfff;
    let minor = device & 0xff | (device >> 12) & !0xff;
    (major, minor)
}

/// One opening of a file.
pub struct OpenFile {
    pub file: Arc<dyn File>,
    /// Status flags that `fcntl(F_SETFL)` may change (only `O_NONBLOCK` for now).
    status: AtomicU32,
}

impl OpenFile {
    pub fn new(file: Arc<dyn File>, status: u32) -> Arc<Self> {
        Arc::new(Self {
            file,
            status: AtomicU32::new(status & flags::O_NONBLOCK),
        })
    }

    pub fn nonblocking(&self) -> bool {
        self.status.load(Ordering::Relaxed) & flags::O_NONBLOCK != 0
    }

    /// `fcntl(F_GETFL)`: access mode and status flags.
    pub fn status_flags(&self) -> u32 {
        let append = if self.file.append() {
            flags::O_APPEND
        } else {
            0
        };
        self.file.access_mode() | append | self.status.load(Ordering::Relaxed)
    }

    /// `fcntl(F_SETFL)`: other bits are ignored, as Linux ignores the ones it cannot change.
    /// `O_APPEND` too, for now: it is fixed when the file is opened.
    pub fn set_status_flags(&self, value: u32) {
        self.status
            .store(value & flags::O_NONBLOCK, Ordering::Relaxed);
    }
}

#[derive(Clone)]
pub struct Descriptor {
    pub file: Arc<OpenFile>,
    pub close_on_exec: bool,
}

/// Most descriptors a process may have: Linux's default `RLIMIT_NOFILE` soft limit.
pub const MAX_DESCRIPTORS: usize = 1024;

/// A process's file descriptors. `fork` copies the table (the open files are shared).
#[derive(Clone, Default)]
pub struct FdTable {
    slots: Vec<Option<Descriptor>>,
}

impl FdTable {
    pub fn get(&self, fd: u64) -> Result<&Descriptor, Errno> {
        usize::try_from(fd)
            .ok()
            .and_then(|fd| self.slots.get(fd))
            .and_then(Option::as_ref)
            .ok_or(EBADF)
    }

    pub fn get_mut(&mut self, fd: u64) -> Result<&mut Descriptor, Errno> {
        usize::try_from(fd)
            .ok()
            .and_then(|fd| self.slots.get_mut(fd))
            .and_then(Option::as_mut)
            .ok_or(EBADF)
    }

    pub fn file(&self, fd: u64) -> Result<Arc<OpenFile>, Errno> {
        self.get(fd).map(|descriptor| descriptor.file.clone())
    }

    /// Puts `descriptor` at the lowest free number that is at least `minimum`.
    pub fn insert(&mut self, descriptor: Descriptor, minimum: usize) -> Result<u64, Errno> {
        let free = (minimum..MAX_DESCRIPTORS)
            .find(|&fd| self.slots.get(fd).is_none_or(Option::is_none))
            .ok_or(EMFILE)?;
        self.place(free, descriptor);
        Ok(free as u64)
    }

    /// Puts `descriptor` at `fd`, returning what was there (for the caller to drop outside
    /// the table's lock: closing a file may wake other tasks).
    pub fn insert_at(
        &mut self,
        fd: u64,
        descriptor: Descriptor,
    ) -> Result<Option<Descriptor>, Errno> {
        let fd = usize::try_from(fd)
            .ok()
            .filter(|&fd| fd < MAX_DESCRIPTORS)
            .ok_or(EBADF)?;
        let old = self.slots.get_mut(fd).and_then(Option::take);
        self.place(fd, descriptor);
        Ok(old)
    }

    pub fn remove(&mut self, fd: u64) -> Result<Descriptor, Errno> {
        usize::try_from(fd)
            .ok()
            .and_then(|fd| self.slots.get_mut(fd))
            .and_then(Option::take)
            .ok_or(EBADF)
    }

    /// The open descriptors, by number.
    pub fn descriptors(&self) -> impl Iterator<Item = (u64, &Descriptor)> {
        self.slots
            .iter()
            .enumerate()
            .filter_map(|(fd, slot)| Some((fd as u64, slot.as_ref()?)))
    }

    /// Removes the descriptors marked close-on-exec (for `execve`) and returns them.
    pub fn take_close_on_exec(&mut self) -> Vec<Descriptor> {
        self.slots
            .iter_mut()
            .filter(|slot| slot.as_ref().is_some_and(|d| d.close_on_exec))
            .filter_map(Option::take)
            .collect()
    }

    fn place(&mut self, fd: usize, descriptor: Descriptor) {
        if self.slots.len() <= fd {
            self.slots.resize(fd + 1, None);
        }
        self.slots[fd] = Some(descriptor);
    }
}

/// Reads nothing (end of file at once) and swallows writes: `/dev/null`.
pub struct Null;

impl File for Null {
    fn stat(&self) -> Metadata {
        anonymous_metadata(mode::CHARACTER_DEVICE | 0o666, device(1, 3), 0, 0)
    }

    fn read(&self, _buffer: &mut [u8], _nonblocking: bool) -> Result<usize, Errno> {
        Ok(0)
    }

    fn write(&self, data: &[u8], _nonblocking: bool) -> Result<usize, Errno> {
        Ok(data.len())
    }

    /// Any position is fine and stays 0, as Linux's `/dev/null`.
    fn seek(&self, _offset: i64, _whence: u32) -> Result<u64, Errno> {
        Ok(0)
    }

    fn access_mode(&self) -> u32 {
        flags::O_RDWR
    }
}

/// `/dev/zero` reads zeros and swallows writes; `/dev/full` reads zeros and is always full.
struct Zero {
    full: bool,
}

impl File for Zero {
    fn stat(&self) -> Metadata {
        let minor = if self.full { 7 } else { 5 };
        anonymous_metadata(mode::CHARACTER_DEVICE | 0o666, device(1, minor), 0, 0)
    }

    fn read(&self, buffer: &mut [u8], _nonblocking: bool) -> Result<usize, Errno> {
        buffer.fill(0);
        Ok(buffer.len())
    }

    fn write(&self, data: &[u8], _nonblocking: bool) -> Result<usize, Errno> {
        if self.full && !data.is_empty() {
            return Err(ENOSPC);
        }
        Ok(data.len())
    }

    fn seek(&self, _offset: i64, _whence: u32) -> Result<u64, Errno> {
        Ok(0)
    }

    fn access_mode(&self) -> u32 {
        flags::O_RDWR
    }
}

/// `/dev/random` and `/dev/urandom`: the kernel's random generator, as `getrandom` (since
/// Linux 5.6 both never block once the generator is seeded). Writes are taken and ignored:
/// Linux mixes them in without crediting entropy.
struct Random;

impl File for Random {
    fn stat(&self) -> Metadata {
        anonymous_metadata(mode::CHARACTER_DEVICE | 0o666, device(1, 9), 0, 0)
    }

    fn read(&self, buffer: &mut [u8], _nonblocking: bool) -> Result<usize, Errno> {
        ochra_frame::random::fill(buffer);
        Ok(buffer.len())
    }

    fn write(&self, data: &[u8], _nonblocking: bool) -> Result<usize, Errno> {
        Ok(data.len())
    }

    fn seek(&self, _offset: i64, _whence: u32) -> Result<u64, Errno> {
        Ok(0)
    }

    fn access_mode(&self) -> u32 {
        flags::O_RDWR
    }
}

/// How much a pipe holds: Linux's default, 16 pages.
pub const PIPE_CAPACITY: usize = 64 * 1024;
/// Writes up to this size are atomic: never mixed with other writers' bytes (POSIX).
pub const PIPE_BUF: usize = 4096;

/// A pipe: bytes written to one end come out of the other, in order.
struct Pipe {
    state: IrqMutex<PipeState>,
    /// Readers wait here for data (or for the last writer to go).
    readable: WaitQueue,
    /// Writers wait here for room (or for the last reader to go).
    writable: WaitQueue,
    created: Timestamp,
}

struct PipeState {
    buffer: VecDeque<u8>,
    readers: usize,
    writers: usize,
}

/// One end of a pipe. When the last `Arc` of an end is dropped, the other side learns it.
pub struct PipeEnd {
    pipe: Arc<Pipe>,
    writer: bool,
}

/// Creates a pipe and returns its read end and write end.
pub fn pipe() -> (Arc<dyn File>, Arc<dyn File>) {
    let pipe = Arc::new(Pipe {
        state: IrqMutex::new(PipeState {
            buffer: VecDeque::new(),
            readers: 1,
            writers: 1,
        }),
        readable: WaitQueue::new(),
        writable: WaitQueue::new(),
        created: Timestamp::now(),
    });
    let reader = Arc::new(PipeEnd {
        pipe: pipe.clone(),
        writer: false,
    });
    let writer = Arc::new(PipeEnd { pipe, writer: true });
    (reader, writer)
}

impl File for PipeEnd {
    fn read(&self, buffer: &mut [u8], nonblocking: bool) -> Result<usize, Errno> {
        if self.writer {
            return Err(EBADF);
        }
        if buffer.is_empty() {
            return Ok(0);
        }
        let pipe = &self.pipe;
        let result = pipe.readable.wait_interruptible(|| {
            let mut state = pipe.state.lock();
            if !state.buffer.is_empty() {
                let count = buffer.len().min(state.buffer.len());
                for (slot, byte) in buffer.iter_mut().zip(state.buffer.drain(..count)) {
                    *slot = byte;
                }
                Some(Ok(count))
            } else if state.writers == 0 {
                Some(Ok(0))
            } else if nonblocking {
                Some(Err(EAGAIN))
            } else {
                None
            }
        })?;
        if matches!(result, Ok(count) if count > 0) {
            pipe.writable.wake_all();
        }
        result
    }

    fn write(&self, data: &[u8], nonblocking: bool) -> Result<usize, Errno> {
        if !self.writer {
            return Err(EBADF);
        }
        let pipe = &self.pipe;
        let mut written = 0;
        while written < data.len() {
            let rest = &data[written..];
            let step = pipe.writable.wait_interruptible(|| {
                let mut state = pipe.state.lock();
                if state.readers == 0 {
                    // The syscall layer adds `SIGPIPE`.
                    return Some(Err(EPIPE));
                }
                let room = PIPE_CAPACITY - state.buffer.len();
                // A small write goes in whole or waits; a large one goes in pieces.
                let needed = if rest.len() <= PIPE_BUF {
                    rest.len()
                } else {
                    1
                };
                if room >= needed {
                    let count = room.min(rest.len());
                    state.buffer.extend(&rest[..count]);
                    Some(Ok(count))
                } else if nonblocking {
                    Some(Err(EAGAIN))
                } else {
                    None
                }
            });
            match step.and_then(|step| step) {
                Ok(count) => {
                    written += count;
                    pipe.readable.wake_all();
                }
                // What was written before counts; the error shows on the next call.
                Err(_) if written > 0 => break,
                Err(error) => return Err(error),
            }
        }
        Ok(written)
    }

    fn access_mode(&self) -> u32 {
        if self.writer {
            flags::O_WRONLY
        } else {
            flags::O_RDONLY
        }
    }

    fn poll(&self) -> u32 {
        self.events()
    }

    fn wait_queues<'a>(&'a self, queues: &mut Vec<&'a WaitQueue>) {
        queues.push(if self.writer {
            &self.pipe.writable
        } else {
            &self.pipe.readable
        });
    }

    fn stat(&self) -> Metadata {
        // One number per pipe, as Linux's pipefs gives each its own inode.
        let inode = Arc::as_ptr(&self.pipe) as u64;
        let mut metadata = anonymous_metadata(mode::FIFO | 0o600, 0, PIPE_DEVICE, inode);
        metadata.accessed = self.pipe.created;
        metadata.modified = self.pipe.created;
        metadata.changed = self.pipe.created;
        metadata
    }
}

impl PipeEnd {
    /// `poll` for a pipe end, as Linux's `pipe_poll`: the read end is readable with data and
    /// hung up without writers; the write end is writable with room for an atomic write and
    /// in error without readers.
    fn events(&self) -> u32 {
        let state = self.pipe.state.lock();
        let mut ready = 0;
        if self.writer {
            if PIPE_CAPACITY - state.buffer.len() >= PIPE_BUF {
                ready |= events::OUT | events::WRNORM;
            }
            if state.readers == 0 {
                ready |= events::ERR;
            }
        } else {
            if !state.buffer.is_empty() {
                ready |= events::IN | events::RDNORM;
            }
            if state.writers == 0 {
                ready |= events::HUP;
            }
        }
        ready
    }
}

impl Drop for PipeEnd {
    fn drop(&mut self) {
        {
            let mut state = self.pipe.state.lock();
            if self.writer {
                state.writers -= 1;
            } else {
                state.readers -= 1;
            }
        }
        // Readers now see the end of file, or writers `EPIPE`.
        self.pipe.readable.wake_all();
        self.pipe.writable.wake_all();
    }
}
