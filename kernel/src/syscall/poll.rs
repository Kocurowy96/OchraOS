// SPDX-License-Identifier: GPL-2.0-only

//! Waiting for any of several files to be ready: `poll`, `ppoll`, `select`, `pselect6`
//! (`man 2 poll`, `man 2 select`).
//!
//! Every file names the wait queues woken when its readiness may change. The caller is put
//! on **all** of them before any file is checked ([`sched::wait_any`]), so an event that
//! comes while the list is being set up still wakes it: checking first and registering
//! after would lose such an event and sleep until the timeout.

use alloc::sync::Arc;
use alloc::vec;
use alloc::vec::Vec;

use super::{SyscallResult, current, read_user, write_user};
use crate::errno::{EBADF, EINVAL, ERESTARTNOHAND, Errno};
use crate::file::{MAX_DESCRIPTORS, OpenFile, events};
use crate::sched::{self, WaitQueue};
use crate::signal::{self, UNBLOCKABLE};
use crate::time::{self, NANOSECONDS_PER_SECOND};

/// The size of a signal set (`sigsetsize`).
const SET_SIZE: u64 = 8;

/// When to give up: never (`None`), or at this monotonic time (now, for a zero timeout:
/// check once).
type Deadline = Option<u64>;

fn deadline_after(nanoseconds: u64) -> Deadline {
    Some(time::monotonic().saturating_add(nanoseconds))
}

/// Waits until `check` counts something ready, the deadline passes (0) or a signal comes
/// (`ERESTARTNOHAND`). `files` are the files `check` looks at.
fn wait_for_events(
    files: &[Arc<OpenFile>],
    deadline: Deadline,
    mut check: impl FnMut() -> usize,
) -> Result<usize, Errno> {
    let mut queues: Vec<&WaitQueue> = Vec::new();
    for file in files {
        file.file.wait_queues(&mut queues);
    }
    if deadline.is_some() {
        queues.push(time::ticks_queue());
    }
    sched::wait_any(&queues, || {
        let count = check();
        if count > 0 {
            return Some(Ok(count));
        }
        if deadline.is_some_and(|deadline| time::monotonic() >= deadline) {
            return Some(Ok(0));
        }
        if signal::interrupted() {
            return Some(Err(ERESTARTNOHAND));
        }
        None
    })
}

/// Runs `wait` with the signal mask at `mask` (`ppoll`, `pselect6`): `mask` 0 keeps the
/// current one. If a signal interrupted the wait, the old mask comes back only after its
/// handler (as `rt_sigsuspend`), so a signal unblocked just for the wait is delivered.
fn with_mask(mask: u64, wait: impl FnOnce() -> Result<usize, Errno>) -> Result<usize, Errno> {
    if mask == 0 {
        return wait();
    }
    let mut bytes = [0; 8];
    read_user(&mut bytes, mask)?;
    let process = current()?;
    {
        let mut signals = process.signals.lock();
        signals.saved_mask = Some(signals.blocked);
        signals.blocked = u64::from_le_bytes(bytes) & !UNBLOCKABLE;
    }
    let result = wait();
    if result != Err(ERESTARTNOHAND) {
        let mut signals = process.signals.lock();
        if let Some(old) = signals.saved_mask.take() {
            signals.blocked = old;
        }
    }
    result
}

/// Reads a `struct timespec` (or a `struct timeval`, with `unit` 1000 for microseconds) as
/// nanoseconds; `EINVAL` if negative. Out-of-range fractions carry over, as Linux's
/// `select` does.
fn read_time(address: u64, unit: u64) -> Result<u64, Errno> {
    let mut bytes = [0; 16];
    read_user(&mut bytes, address)?;
    let seconds = i64::from_le_bytes(bytes[..8].try_into().expect("8 bytes"));
    let fraction = i64::from_le_bytes(bytes[8..].try_into().expect("8 bytes"));
    if seconds < 0 || fraction < 0 {
        return Err(EINVAL);
    }
    if unit == 1 && fraction >= NANOSECONDS_PER_SECOND as i64 {
        return Err(EINVAL);
    }
    Ok((seconds as u64)
        .saturating_mul(NANOSECONDS_PER_SECOND)
        .saturating_add((fraction as u64).saturating_mul(unit)))
}

/// Writes back the time left until `deadline` (Linux's `poll_select_finish`): not for no
/// timeout or a zero one.
fn write_time_left(address: u64, deadline: Deadline, zero: bool, unit: u64) {
    let Some(deadline) = deadline else {
        return;
    };
    if address == 0 || zero {
        return;
    }
    let left = deadline.saturating_sub(time::monotonic());
    let mut bytes = [0; 16];
    bytes[..8].copy_from_slice(&(left / NANOSECONDS_PER_SECOND).to_le_bytes());
    bytes[8..].copy_from_slice(&(left % NANOSECONDS_PER_SECOND / unit).to_le_bytes());
    // As in Linux, a timeout that cannot be written back is not an error.
    let _ = write_user(address, &bytes);
}

/// The open file of `fd`, or `None` if there is none.
fn lookup(fd: u64) -> Option<Arc<OpenFile>> {
    current().ok()?.files.lock().file(fd).ok()
}

/// One `struct pollfd`: `int fd; short events; short revents;`.
const POLLFD_SIZE: usize = 8;

/// The common part of `poll` and `ppoll`.
fn do_poll(address: u64, count: u64, deadline: Deadline) -> Result<usize, Errno> {
    if count > MAX_DESCRIPTORS as u64 {
        return Err(EINVAL);
    }
    let count = count as usize;
    let mut bytes = vec![0; count * POLLFD_SIZE];
    read_user(&mut bytes, address)?;
    let entries: Vec<(i32, u32)> = bytes
        .as_chunks::<POLLFD_SIZE>()
        .0
        .iter()
        .map(|entry| {
            let fd = i32::from_le_bytes(entry[..4].try_into().expect("4 bytes"));
            let wanted = u16::from_le_bytes([entry[4], entry[5]]);
            (fd, u32::from(wanted))
        })
        .collect();
    // A negative descriptor is skipped; one that is not open reports `POLLNVAL`.
    let files: Vec<Option<Arc<OpenFile>>> = entries
        .iter()
        .map(|&(fd, _)| if fd < 0 { None } else { lookup(fd as u64) })
        .collect();
    let open: Vec<Arc<OpenFile>> = files.iter().flatten().cloned().collect();
    let mut answers = vec![0u32; count];
    let ready = wait_for_events(&open, deadline, || {
        let mut ready = 0;
        for (index, (&(fd, wanted), file)) in entries.iter().zip(&files).enumerate() {
            answers[index] = match file {
                _ if fd < 0 => 0,
                None => events::NVAL,
                // Errors and hangups are reported even if not asked for.
                Some(file) => file.file.poll() & (wanted | events::ERR | events::HUP),
            };
            if answers[index] != 0 {
                ready += 1;
            }
        }
        ready
    })?;
    for (entry, answer) in bytes
        .as_chunks_mut::<POLLFD_SIZE>()
        .0
        .iter_mut()
        .zip(&answers)
    {
        entry[6..].copy_from_slice(&(*answer as u16).to_le_bytes());
    }
    write_user(address, &bytes)?;
    Ok(ready)
}

/// `poll(fds, nfds, timeout)`: `timeout` in milliseconds, negative for none.
pub fn poll(address: u64, count: u64, timeout: u64) -> SyscallResult {
    let timeout = timeout as i32;
    let deadline = if timeout < 0 {
        None
    } else {
        deadline_after(timeout as u64 * 1_000_000)
    };
    do_poll(address, count, deadline).map(|ready| ready as u64)
}

/// `ppoll(fds, nfds, tmo_p, sigmask, sigsetsize)`: the timeout as a `struct timespec` (NULL
/// for none, written back with the time left), and a signal mask for the wait.
pub fn ppoll(address: u64, count: u64, timeout: u64, mask: u64, set_size: u64) -> SyscallResult {
    if mask != 0 && set_size != SET_SIZE {
        return Err(EINVAL);
    }
    let (deadline, zero) = if timeout == 0 {
        (None, false)
    } else {
        let nanoseconds = read_time(timeout, 1)?;
        (deadline_after(nanoseconds), nanoseconds == 0)
    };
    let result = with_mask(mask, || do_poll(address, count, deadline));
    write_time_left(timeout, deadline, zero, 1);
    result.map(|ready| ready as u64)
}

/// `select`'s three sets, each a bit array of descriptors (`fd_set`), as 64-bit words.
struct Sets {
    read: Vec<u64>,
    write: Vec<u64>,
    except: Vec<u64>,
}

fn read_set(address: u64, words: usize) -> Result<Vec<u64>, Errno> {
    let mut set = vec![0u64; words];
    if address != 0 {
        let mut bytes = vec![0; words * 8];
        read_user(&mut bytes, address)?;
        for (word, chunk) in set.iter_mut().zip(bytes.as_chunks::<8>().0) {
            *word = u64::from_le_bytes(*chunk);
        }
    }
    Ok(set)
}

fn write_set(address: u64, set: &[u64]) -> Result<(), Errno> {
    if address == 0 {
        return Ok(());
    }
    let bytes: Vec<u8> = set.iter().flat_map(|word| word.to_le_bytes()).collect();
    write_user(address, &bytes)
}

/// The common part of `select` and `pselect6` (Linux's `core_sys_select`).
fn do_select(count: u64, addresses: [u64; 3], deadline: Deadline) -> Result<usize, Errno> {
    let count = (count as i32).try_into().map_err(|_| EINVAL)?;
    let count: usize = core::cmp::min(count, MAX_DESCRIPTORS);
    let words = count.div_ceil(64);
    let wanted = Sets {
        read: read_set(addresses[0], words)?,
        write: read_set(addresses[1], words)?,
        except: read_set(addresses[2], words)?,
    };
    // Every descriptor in a set must be open.
    let mut files: Vec<(usize, Arc<OpenFile>)> = Vec::new();
    for fd in 0..count {
        let bit = 1 << (fd % 64);
        let word = fd / 64;
        if (wanted.read[word] | wanted.write[word] | wanted.except[word]) & bit == 0 {
            continue;
        }
        files.push((fd, lookup(fd as u64).ok_or(EBADF)?));
    }
    let open: Vec<Arc<OpenFile>> = files.iter().map(|(_, file)| file.clone()).collect();
    const IN_SET: u32 = events::IN | events::RDNORM | events::HUP | events::ERR;
    const OUT_SET: u32 = events::OUT | events::WRNORM | events::ERR;
    const EX_SET: u32 = events::PRI;
    let mut found = Sets {
        read: vec![0; words],
        write: vec![0; words],
        except: vec![0; words],
    };
    let ready = wait_for_events(&open, deadline, || {
        let mut ready = 0;
        for set in [&mut found.read, &mut found.write, &mut found.except] {
            set.fill(0);
        }
        for (fd, file) in &files {
            let answer = file.file.poll();
            let (word, bit) = (fd / 64, 1u64 << (fd % 64));
            for (wanted, found, mask) in [
                (&wanted.read, &mut found.read, IN_SET),
                (&wanted.write, &mut found.write, OUT_SET),
                (&wanted.except, &mut found.except, EX_SET),
            ] {
                if wanted[word] & bit != 0 && answer & mask != 0 {
                    found[word] |= bit;
                    ready += 1;
                }
            }
        }
        ready
    })?;
    write_set(addresses[0], &found.read)?;
    write_set(addresses[1], &found.write)?;
    write_set(addresses[2], &found.except)?;
    Ok(ready)
}

/// `select(nfds, readfds, writefds, exceptfds, timeout)`: the timeout as a `struct timeval`
/// (NULL for none), written back with the time left.
pub fn select(count: u64, read: u64, write: u64, except: u64, timeout: u64) -> SyscallResult {
    const MICROSECOND: u64 = 1000;
    let (deadline, zero) = if timeout == 0 {
        (None, false)
    } else {
        let nanoseconds = read_time(timeout, MICROSECOND)?;
        (deadline_after(nanoseconds), nanoseconds == 0)
    };
    let result = do_select(count, [read, write, except], deadline);
    write_time_left(timeout, deadline, zero, MICROSECOND);
    result.map(|ready| ready as u64)
}

/// `pselect6(nfds, readfds, writefds, exceptfds, timeout, sigmask)`: the timeout as a
/// `struct timespec`; `sigmask` points to `{ const sigset_t *ss; size_t ss_len; }`.
pub fn pselect6(
    count: u64,
    read: u64,
    write: u64,
    except: u64,
    timeout: u64,
    mask: u64,
) -> SyscallResult {
    let mut set = 0;
    if mask != 0 {
        let mut bytes = [0; 16];
        read_user(&mut bytes, mask)?;
        set = u64::from_le_bytes(bytes[..8].try_into().expect("8 bytes"));
        let size = u64::from_le_bytes(bytes[8..].try_into().expect("8 bytes"));
        if set != 0 && size != SET_SIZE {
            return Err(EINVAL);
        }
    }
    let (deadline, zero) = if timeout == 0 {
        (None, false)
    } else {
        let nanoseconds = read_time(timeout, 1)?;
        (deadline_after(nanoseconds), nanoseconds == 0)
    };
    let result = with_mask(set, || do_select(count, [read, write, except], deadline));
    write_time_left(timeout, deadline, zero, 1);
    result.map(|ready| ready as u64)
}
