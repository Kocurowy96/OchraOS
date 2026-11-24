// SPDX-License-Identifier: GPL-2.0-only

//! Pseudo-terminals (`man 7 pty`): a pair of a **master** and a **slave**. The slave is an
//! ordinary terminal (`/dev/pts/<n>`, with a line discipline) that a shell runs on; the
//! master (from opening `/dev/ptmx`) is the program playing the keyboard and the screen — a
//! terminal emulator, `script`, `ssh`. What the master writes is typed into the slave; what
//! programs write to the slave (and its echo) the master reads.
//!
//! As in Linux (`drivers/tty/pty.c`, `fs/devpts`): the slave starts locked until the master
//! unlocks it (`unlockpt`, `TIOCSPTLCK`); its number comes from `TIOCGPTN` (`ptsname`);
//! it shows in `/dev/pts` while the master is open; closing the master hangs the slave up;
//! and the master reads `EIO` once every slave file that was opened is closed.

use alloc::boxed::Box;
use alloc::collections::{BTreeMap, VecDeque};
use alloc::format;
use alloc::string::ToString;
use alloc::sync::{Arc, Weak};
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, Ordering};

use ochra_frame::sync::IrqMutex;

use super::termios::{Termios, WindowSize};
use super::{Driver, Tty, request};
use crate::errno::{EAGAIN, EINVAL, EIO, ENOSPC, ENOTTY, Errno};
use crate::file::{File, device, events, flags};
use crate::fs::synthetic::{self, Attributes, Entry, Listing};
use crate::process;
use crate::sched::WaitQueue;
use crate::syscall::{read_user, write_user};
use crate::vfs::{InodeRef, Metadata, anonymous_device, mode};

/// The major number of slaves (`UNIX98_PTY_SLAVE_MAJOR`).
const SLAVE_MAJOR: u64 = 136;
/// How many pseudo-terminals may exist at once (Linux's default `kernel.pty.max`).
const MAX: u32 = 4096;
/// How much slave output waits for the master to read it.
const OUTPUT_SIZE: usize = 64 * 1024;
/// The group that owns slaves (`tty`, as devpts is mounted with `gid=5,mode=620`).
const TTY_GROUP: u32 = 5;

/// Slave output on its way to the master.
struct Output {
    buffer: IrqMutex<VecDeque<u8>>,
    /// Woken when there is something to read.
    readable: WaitQueue,
    /// Woken when the master read something, and when it closed.
    room: WaitQueue,
    master_closed: AtomicBool,
}

impl Output {
    /// Takes as much of `data` as fits; returns how much.
    fn push(&self, data: &[u8]) -> usize {
        let count = {
            let mut buffer = self.buffer.lock();
            let count = data.len().min(OUTPUT_SIZE - buffer.len());
            buffer.extend(&data[..count]);
            count
        };
        if count > 0 {
            self.readable.wake_all();
        }
        count
    }
}

/// The driver below a slave: the master's side.
struct SlaveDriver(Arc<Output>);

impl Driver for SlaveDriver {
    fn write(&self, data: &[u8], nonblocking: bool) -> Result<(), Errno> {
        let output = &self.0;
        if nonblocking {
            // All or nothing, so the line discipline's count stays right.
            let mut buffer = output.buffer.lock();
            if output.master_closed.load(Ordering::Relaxed) {
                return Err(EIO);
            }
            if OUTPUT_SIZE - buffer.len() < data.len() {
                return Err(EAGAIN);
            }
            buffer.extend(data);
            drop(buffer);
            output.readable.wake_all();
            return Ok(());
        }
        let mut rest = data;
        while !rest.is_empty() {
            let taken = output.room.wait_interruptible(|| {
                if output.master_closed.load(Ordering::Relaxed) {
                    return Some(Err(EIO));
                }
                let taken = output.push(rest);
                (taken > 0).then_some(Ok(taken))
            })??;
            rest = &rest[taken..];
        }
        Ok(())
    }

    fn echo(&self, data: &[u8]) {
        if !self.0.master_closed.load(Ordering::Relaxed) {
            self.0.push(data);
        }
    }

    fn writable(&self) -> bool {
        self.0.buffer.lock().len() < OUTPUT_SIZE
    }

    fn write_queue(&self) -> Option<&WaitQueue> {
        Some(&self.0.room)
    }
}

/// A pseudo-terminal pair.
struct Pty {
    index: u32,
    slave: Arc<Tty>,
    output: Arc<Output>,
    /// `TIOCSPTLCK`: the slave cannot be opened yet.
    locked: AtomicBool,
    /// Who opened the master: the slave node's owner.
    owner: u32,
}

/// The live pairs by number. A pair lives while its master is open.
static PTYS: IrqMutex<BTreeMap<u32, Weak<Pty>>> = IrqMutex::new(BTreeMap::new());

fn find(index: u32) -> Option<Arc<Pty>> {
    PTYS.lock().get(&index).and_then(Weak::upgrade)
}

/// Opens `/dev/ptmx`: a new pair, the lowest free number.
pub fn open_master() -> Result<Arc<dyn File>, Errno> {
    let owner = process::current().map_or(0, |process| process.credentials.lock().effective_user);
    let mut ptys = PTYS.lock();
    ptys.retain(|_, pty| pty.strong_count() > 0);
    let index = (0..MAX)
        .find(|index| !ptys.contains_key(index))
        .ok_or(ENOSPC)?;
    let output = Arc::new(Output {
        buffer: IrqMutex::new(VecDeque::new()),
        readable: WaitQueue::new(),
        room: WaitQueue::new(),
        master_closed: AtomicBool::new(false),
    });
    let slave = Tty::new(
        format!("pts/{index}"),
        device(SLAVE_MAJOR, u64::from(index)),
        Arc::new(SlaveDriver(output.clone())),
        Termios::pty_slave(),
    );
    let pty = Arc::new(Pty {
        index,
        slave,
        output,
        locked: AtomicBool::new(true),
        owner,
    });
    ptys.insert(index, Arc::downgrade(&pty));
    Ok(Arc::new(Master { pty }))
}

/// The slave terminal number `index`, to open: `EIO` while it is locked or its master is
/// gone.
pub fn slave(index: u64) -> Result<Arc<Tty>, Errno> {
    let index = u32::try_from(index).map_err(|_| EIO)?;
    let pty = find(index).ok_or(EIO)?;
    if pty.locked.load(Ordering::Relaxed) {
        return Err(EIO);
    }
    Ok(pty.slave.clone())
}

/// A file of terminal `tty` was closed: if it is a slave, its master may now see the
/// hangup.
pub fn slave_closed(tty: &Tty) {
    let (major, minor) = crate::file::major_minor(tty.device);
    if major != SLAVE_MAJOR {
        return;
    }
    if let Some(pty) = u32::try_from(minor).ok().and_then(find) {
        pty.output.readable.wake_all();
    }
}

/// The open master side.
struct Master {
    pty: Arc<Pty>,
}

impl Master {
    fn events(&self) -> u32 {
        let pty = &self.pty;
        let mut ready = 0;
        if !pty.output.buffer.lock().is_empty() {
            ready |= events::IN | events::RDNORM;
        }
        if pty.slave.all_closed() {
            ready |= events::HUP;
        }
        if pty.slave.has_room() {
            ready |= events::OUT | events::WRNORM;
        }
        ready
    }

    /// The ioctls of the master (Linux's `pty_unix98_ioctl`, then the terminal ones that
    /// act on the slave).
    fn ioctl(&self, request: u64, argument: u64) -> Result<u64, Errno> {
        use request::*;
        let pty = &self.pty;
        let put = |value: u32| write_user(argument, &value.to_le_bytes()).map(|()| 0);
        match request {
            TIOCGPTN => put(pty.index),
            TIOCSPTLCK => {
                let mut bytes = [0; 4];
                read_user(&mut bytes, argument)?;
                pty.locked
                    .store(i32::from_le_bytes(bytes) != 0, Ordering::Relaxed);
                Ok(0)
            }
            TIOCGPTLCK => put(u32::from(pty.locked.load(Ordering::Relaxed))),
            TIOCGWINSZ => {
                let window = pty.slave.state.lock().window;
                write_user(argument, &window.to_bytes())?;
                Ok(0)
            }
            TIOCSWINSZ => {
                let mut bytes = [0; 8];
                read_user(&mut bytes, argument)?;
                pty.slave.resize(WindowSize::from_bytes(&bytes));
                Ok(0)
            }
            // The master asks about the slave's session without being in it; no foreground
            // group reads as 0 (Linux's `pid_vnr(NULL)`).
            TIOCGPGRP => put(pty.slave.foreground().unwrap_or(0)),
            TIOCGSID => put(pty.slave.session().ok_or(ENOTTY)?),
            FIONREAD => put(pty.output.buffer.lock().len() as u32),
            TIOCOUTQ => put(0),
            // The settings are the slave's: Linux's `tty_mode_ioctl` works on the other side
            // of a master (the master itself has no line discipline to set).
            TCGETS | TCGETS2 | TCSETS | TCSETSW | TCSETSF | TCSETS2 | TCSETSW2 | TCSETSF2 => {
                pty.slave.ioctl(request, argument)
            }
            TCFLSH => {
                const TCIFLUSH: u64 = 0;
                const TCOFLUSH: u64 = 1;
                const TCIOFLUSH: u64 = 2;
                match argument {
                    // The master's input is the slave's output, and the other way round.
                    TCIFLUSH => pty.output.buffer.lock().clear(),
                    TCOFLUSH => pty.slave.state.lock().flush_input(),
                    TCIOFLUSH => {
                        pty.output.buffer.lock().clear();
                        pty.slave.state.lock().flush_input();
                    }
                    _ => return Err(EINVAL),
                }
                pty.output.room.wake_all();
                pty.slave.room.wake_all();
                Ok(0)
            }
            TCSBRK | TCSBRKP => Ok(0),
            TIOCSTI => Err(EIO),
            _ => Err(ENOTTY),
        }
    }
}

impl File for Master {
    fn read(&self, buffer: &mut [u8], nonblocking: bool) -> Result<usize, Errno> {
        if buffer.is_empty() {
            return Ok(0);
        }
        let output = &self.pty.output;
        let count = output.readable.wait_interruptible(|| {
            let mut queued = output.buffer.lock();
            if !queued.is_empty() {
                let count = buffer.len().min(queued.len());
                for (slot, byte) in buffer.iter_mut().zip(queued.drain(..count)) {
                    *slot = byte;
                }
                return Some(Ok(count));
            }
            if self.pty.slave.all_closed() {
                return Some(Err(EIO));
            }
            nonblocking.then_some(Err(EAGAIN))
        })??;
        output.room.wake_all();
        Ok(count)
    }

    fn write(&self, data: &[u8], nonblocking: bool) -> Result<usize, Errno> {
        let slave = &self.pty.slave;
        let mut written = 0;
        while written < data.len() {
            let taken = slave.room.wait_interruptible(|| {
                let taken = slave.receive(&data[written..]);
                if taken > 0 {
                    return Some(Ok(taken));
                }
                nonblocking.then_some(Err(EAGAIN))
            })?;
            match taken {
                Ok(taken) => written += taken,
                Err(_) if written > 0 => break,
                Err(error) => return Err(error),
            }
        }
        Ok(written)
    }

    fn access_mode(&self) -> u32 {
        flags::O_RDWR
    }

    fn stat(&self) -> Metadata {
        crate::file::anonymous_metadata(mode::CHARACTER_DEVICE | 0o666, device(5, 2), 0, 0)
    }

    fn ioctl(&self, request: u64, argument: u64) -> Result<u64, Errno> {
        Master::ioctl(self, request, argument)
    }

    fn poll(&self) -> u32 {
        self.events()
    }

    fn wait_queues<'a>(&'a self, queues: &mut Vec<&'a WaitQueue>) {
        queues.extend([&self.pty.output.readable, &self.pty.slave.room]);
    }
}

impl Drop for Master {
    fn drop(&mut self) {
        self.pty.output.master_closed.store(true, Ordering::Relaxed);
        self.pty.output.room.wake_all();
        self.pty.slave.hang_up();
    }
}

/// The device number of the devpts filesystem, made once.
fn devpts_device() -> u64 {
    static DEVICE: IrqMutex<Option<u64>> = IrqMutex::new(None);
    *DEVICE.lock().get_or_insert_with(anonymous_device)
}

/// `/dev/pts`: `ptmx` and a node for every pair whose master is open (inode numbers as in
/// Linux: the root 1, `ptmx` 2, slave `n` n + 3).
struct Devpts;

impl Listing for Devpts {
    fn entries(&self) -> Result<Vec<Entry>, Errno> {
        let device = devpts_device();
        let ptmx = synthetic::character_device(
            Attributes {
                device,
                inode: 2,
                permissions: 0,
                owner: (0, 0),
            },
            crate::file::device(5, 2),
        );
        let mut entries = alloc::vec![Entry {
            cookie: 3,
            name: "ptmx".to_string(),
            inode: ptmx,
        }];
        let live: Vec<Arc<Pty>> = PTYS.lock().values().filter_map(Weak::upgrade).collect();
        for pty in live {
            let inode = synthetic::character_device(
                Attributes {
                    device,
                    inode: u64::from(pty.index) + 3,
                    permissions: 0o620,
                    owner: (pty.owner, TTY_GROUP),
                },
                pty.slave.device,
            );
            entries.push(Entry {
                cookie: u64::from(pty.index) + 4,
                name: pty.index.to_string(),
                inode,
            });
        }
        Ok(entries)
    }
}

/// The root of devpts, to mount on `/dev/pts`.
pub fn devpts_root() -> InodeRef {
    synthetic::directory(
        Attributes {
            device: devpts_device(),
            inode: 1,
            permissions: 0o755,
            owner: (0, 0),
        },
        Box::new(Devpts),
    )
}
