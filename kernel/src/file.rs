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

use crate::errno::{EAGAIN, EBADF, EINVAL, EISDIR, EMFILE, ENXIO, EPIPE, ESPIPE, Errno};
use crate::initramfs::{Found, Node};
use crate::sched::WaitQueue;

/// Open flags (`include/uapi/asm-generic/fcntl.h`), in octal as there.
pub mod flags {
    pub const O_RDONLY: u32 = 0;
    pub const O_WRONLY: u32 = 1;
    pub const O_RDWR: u32 = 2;
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

    /// Whether this is a terminal (answers terminal `ioctl`s).
    fn is_terminal(&self) -> bool {
        false
    }

    /// What `fstat` reports.
    fn stat(&self) -> Stat;

    /// Moves the file position (`lseek`); files without one (pipes, terminals) refuse.
    fn seek(&self, _offset: i64, _whence: u32) -> Result<u64, Errno> {
        Err(ESPIPE)
    }

    /// Reads at `offset` without moving the file position (`pread64`).
    fn read_at(&self, _buffer: &mut [u8], _offset: u64) -> Result<usize, Errno> {
        Err(ESPIPE)
    }

    /// For a directory: its canonical path (`openat` relative to it, `fchdir`).
    fn directory(&self) -> Option<&str> {
        None
    }
}

/// What `stat` reports about a file (`struct stat`, the fields that mean something here).
#[derive(Clone, Copy, Debug, Default)]
pub struct Stat {
    /// Type and permission bits.
    pub mode: u32,
    pub inode: u64,
    pub size: u64,
    /// The filesystem's device number.
    pub device: u64,
    /// For a device file, the device's number.
    pub rdev: u64,
}

/// The device a character device node stands for, by major and minor number (Linux's
/// numbers, `Documentation/admin-guide/devices.txt`); `ENXIO` for one that does not exist.
pub fn open_device(major: u32, minor: u32) -> Result<Arc<dyn File>, Errno> {
    match (major, minor) {
        (1, 3) => Ok(Arc::new(Null)),
        (5, 1) => Ok(Arc::new(crate::console::Console)),
        _ => Err(ENXIO),
    }
}

/// File types for [`Stat::mode`] that are not in the initramfs.
pub mod mode {
    pub const FIFO: u32 = 0o010_000;
    pub const CHARACTER_DEVICE: u32 = 0o020_000;
}

/// Device numbers (`st_dev`) of the filesystems: the initramfs, and the internal one of
/// pipes. Linux numbers them similarly (0:1 is its rootfs).
pub const INITRAMFS_DEVICE: u64 = 1;
pub const PIPE_DEVICE: u64 = 0x0d;

/// A device number from its major and minor numbers (`makedev`, small numbers only).
pub const fn device(major: u64, minor: u64) -> u64 {
    major << 8 | minor
}

impl Stat {
    /// `struct stat` on x86-64: 144 bytes.
    pub fn to_bytes(self) -> [u8; 144] {
        /// The block size `st_blksize` suggests for I/O.
        const BLOCK: u64 = 4096;
        let mut bytes = [0; 144];
        let mut put = |offset: usize, value: u64| {
            bytes[offset..offset + 8].copy_from_slice(&value.to_le_bytes());
        };
        put(0, self.device);
        put(8, self.inode);
        put(16, 1); // st_nlink
        put(40, self.rdev);
        put(48, self.size);
        put(56, BLOCK);
        put(64, self.size.div_ceil(512)); // st_blocks, in 512-byte units
        // Times stay 0 (1970): the initramfs has none, and there is no clock yet.
        bytes[24..28].copy_from_slice(&self.mode.to_le_bytes());
        // st_uid and st_gid stay 0: root.
        bytes
    }
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
        self.file.access_mode() | self.status.load(Ordering::Relaxed)
    }

    /// `fcntl(F_SETFL)`: other bits are ignored, as Linux ignores the ones it cannot change.
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
    fn stat(&self) -> Stat {
        // As `/dev/null`: character device 1:3.
        Stat {
            mode: mode::CHARACTER_DEVICE | 0o666,
            rdev: device(1, 3),
            ..Stat::default()
        }
    }

    fn read(&self, _buffer: &mut [u8], _nonblocking: bool) -> Result<usize, Errno> {
        Ok(0)
    }

    fn write(&self, data: &[u8], _nonblocking: bool) -> Result<usize, Errno> {
        Ok(data.len())
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

    fn stat(&self) -> Stat {
        Stat {
            mode: mode::FIFO | 0o600,
            // One number per pipe, as Linux's pipefs gives each its own inode.
            inode: Arc::as_ptr(&self.pipe) as u64,
            size: 0,
            device: PIPE_DEVICE,
            rdev: 0,
        }
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

/// A file or directory of the initramfs, opened read-only. Each `open` makes a new one, with
/// its own position; `dup` and `fork` share it (through the `OpenFile`).
pub struct InitramfsFile {
    found: Found,
    position: IrqMutex<u64>,
}

impl InitramfsFile {
    pub fn new(found: Found) -> Self {
        Self {
            found,
            position: IrqMutex::new(0),
        }
    }

    fn data(&self) -> Result<&'static [u8], Errno> {
        match self.found.node {
            Node::File(data) => Ok(data),
            _ => Err(EISDIR),
        }
    }
}

/// The `stat` of something found in the initramfs.
pub fn initramfs_stat(found: &Found) -> Stat {
    let (size, rdev) = match found.node {
        Node::File(data) => (data.len(), 0),
        Node::Symlink(target) => (target.len(), 0),
        Node::Directory => (0, 0),
        Node::CharacterDevice(major, minor) => (0, device(major.into(), minor.into())),
    };
    Stat {
        mode: found.mode,
        inode: found.inode,
        size: size as u64,
        device: INITRAMFS_DEVICE,
        rdev,
    }
}

impl File for InitramfsFile {
    fn read(&self, buffer: &mut [u8], _nonblocking: bool) -> Result<usize, Errno> {
        let mut position = self.position.lock();
        let count = self.read_at(buffer, *position)?;
        *position += count as u64;
        Ok(count)
    }

    fn read_at(&self, buffer: &mut [u8], offset: u64) -> Result<usize, Errno> {
        let data = self.data()?;
        let start = usize::try_from(offset)
            .unwrap_or(usize::MAX)
            .min(data.len());
        let count = buffer.len().min(data.len() - start);
        buffer[..count].copy_from_slice(&data[start..start + count]);
        Ok(count)
    }

    fn access_mode(&self) -> u32 {
        flags::O_RDONLY
    }

    fn stat(&self) -> Stat {
        initramfs_stat(&self.found)
    }

    /// `whence`: `SEEK_SET` (0), `SEEK_CUR` (1) or `SEEK_END` (2). The position may go past
    /// the end (reads there give nothing), not below 0.
    fn seek(&self, offset: i64, whence: u32) -> Result<u64, Errno> {
        const SEEK_SET: u32 = 0;
        const SEEK_CUR: u32 = 1;
        const SEEK_END: u32 = 2;
        let mut position = self.position.lock();
        let base = match whence {
            SEEK_SET => 0,
            SEEK_CUR => *position,
            SEEK_END => self.stat().size,
            _ => return Err(EINVAL),
        };
        let new = base
            .checked_add_signed(offset)
            .filter(|&new| new <= i64::MAX as u64)
            .ok_or(EINVAL)?;
        *position = new;
        Ok(new)
    }

    fn directory(&self) -> Option<&str> {
        matches!(self.found.node, Node::Directory).then_some(self.found.path.as_str())
    }
}
