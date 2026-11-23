// SPDX-License-Identifier: GPL-2.0-only

//! Signal syscalls: actions, the mask, sending, waiting and returning from a handler. The
//! signals themselves are in `crate::signal`.

use alloc::sync::Arc;

use ochra_frame::user_mode::UserContext;

use super::{SyscallResult, current, read_user, write_user};
use crate::errno::{EINVAL, ERESTARTNOHAND, ESRCH, Errno};
use crate::process::{self, Process};
use crate::sched::WaitQueue;
use crate::signal::{self, Action, Info, MAX_SIGNAL, SIGKILL, SIGSTOP, UNBLOCKABLE, code};

/// The size of the kernel's signal set: 64 bits. The C library passes it to every call.
const SET_SIZE: u64 = 8;

/// A signal number from a syscall: 1 to 64.
fn signal_number(value: u64) -> Result<u8, Errno> {
    u8::try_from(value)
        .ok()
        .filter(|signal| (1..=MAX_SIGNAL).contains(signal))
        .ok_or(EINVAL)
}

fn read_word(address: u64) -> Result<u64, Errno> {
    let mut bytes = [0; 8];
    read_user(&mut bytes, address)?;
    Ok(u64::from_le_bytes(bytes))
}

/// `rt_sigaction(signum, act, oldact, sigsetsize)`. The kernel's `struct sigaction` on
/// x86-64: handler, flags, restorer, mask (four 8-byte words).
pub fn rt_sigaction(number: u64, new: u64, old: u64, set_size: u64) -> SyscallResult {
    if set_size != SET_SIZE {
        return Err(EINVAL);
    }
    let signal = signal_number(number)?;
    let new_action = if new == 0 {
        None
    } else {
        if signal == SIGKILL || signal == SIGSTOP {
            return Err(EINVAL);
        }
        let mut bytes = [0; 32];
        read_user(&mut bytes, new)?;
        let word = |index: usize| u64::from_le_bytes(bytes[index * 8..][..8].try_into().unwrap());
        Some(Action {
            handler: word(0),
            flags: word(1),
            restorer: word(2),
            mask: word(3) & !UNBLOCKABLE,
        })
    };
    let process = current()?;
    let previous = {
        let mut signals = process.signals.lock();
        let previous = signals.action(signal);
        if let Some(action) = new_action {
            signals.set_action(signal, action);
        }
        previous
    };
    if old != 0 {
        let mut bytes = [0; 32];
        let words = [
            previous.handler,
            previous.flags,
            previous.restorer,
            previous.mask,
        ];
        for (index, word) in words.iter().enumerate() {
            bytes[index * 8..][..8].copy_from_slice(&word.to_le_bytes());
        }
        write_user(old, &bytes)?;
    }
    Ok(0)
}

/// `rt_sigprocmask(how, set, oldset, sigsetsize)`.
pub fn rt_sigprocmask(how: u64, set: u64, old: u64, set_size: u64) -> SyscallResult {
    const SIG_BLOCK: u64 = 0;
    const SIG_UNBLOCK: u64 = 1;
    const SIG_SETMASK: u64 = 2;
    if set_size != SET_SIZE {
        return Err(EINVAL);
    }
    let new = if set == 0 {
        None
    } else {
        if how > SIG_SETMASK {
            return Err(EINVAL);
        }
        Some(read_word(set)?)
    };
    let process = current()?;
    let previous = {
        let mut signals = process.signals.lock();
        let previous = signals.blocked;
        if let Some(new) = new {
            let blocked = match how {
                SIG_BLOCK => previous | new,
                SIG_UNBLOCK => previous & !new,
                _ => new,
            };
            signals.blocked = blocked & !UNBLOCKABLE;
        }
        previous
    };
    if old != 0 {
        write_user(old, &previous.to_le_bytes())?;
    }
    // A signal unblocked here is delivered on the way back (`signal::on_return`).
    Ok(0)
}

/// `rt_sigpending(set, sigsetsize)`: the signals that wait because they are blocked.
pub fn rt_sigpending(set: u64, set_size: u64) -> SyscallResult {
    if set_size > SET_SIZE {
        return Err(EINVAL);
    }
    let pending = current()?.signals.lock().pending_blocked();
    write_user(set, &pending.to_le_bytes()[..set_size as usize])?;
    Ok(0)
}

/// Nobody wakes this queue: a task waiting on it waits for a signal (which wakes the task
/// itself, see `signal::send`).
static NOTHING: WaitQueue = WaitQueue::new();

/// Blocks until a signal is to be delivered.
fn wait_for_signal() {
    let _interrupted = NOTHING.wait_interruptible(|| None::<()>);
}

/// `pause()`: waits for a signal that runs a handler (or ends the process). Returns
/// `EINTR` after the handler; a signal without a handler makes it wait again.
pub fn pause() -> SyscallResult {
    wait_for_signal();
    Err(ERESTARTNOHAND)
}

/// `rt_sigsuspend(mask, sigsetsize)`: `pause` with `mask` as the signal mask meanwhile. The
/// old mask comes back after the handler (it is the one saved in the signal frame).
pub fn rt_sigsuspend(mask: u64, set_size: u64) -> SyscallResult {
    if set_size != SET_SIZE {
        return Err(EINVAL);
    }
    let mask = read_word(mask)?;
    let process = current()?;
    {
        let mut signals = process.signals.lock();
        signals.saved_mask = Some(signals.blocked);
        signals.blocked = mask & !UNBLOCKABLE;
    }
    wait_for_signal();
    Err(ERESTARTNOHAND)
}

/// `rt_sigreturn()`: see `signal::sigreturn`.
pub fn rt_sigreturn(context: &mut UserContext) -> SyscallResult {
    let process = current()?;
    signal::sigreturn(&process, context)
}

/// Sends `number` (0: only check that the targets exist) to `targets`.
fn send_to(targets: &[Arc<Process>], number: u64, info: Info) -> SyscallResult {
    let signal = if number == 0 {
        None
    } else {
        Some(signal_number(number)?)
    };
    if targets.is_empty() {
        return Err(ESRCH);
    }
    if let Some(signal) = signal {
        for target in targets {
            signal::send(target, signal, info);
        }
    }
    Ok(0)
}

/// `kill(pid, sig)`: to one process (`pid` > 0), the caller's group (0), every process but
/// init and the caller (-1), or the group `-pid`.
pub fn kill(pid: i32, number: u64) -> SyscallResult {
    if number > u64::from(MAX_SIGNAL) {
        return Err(EINVAL);
    }
    let process = current()?;
    let own_group = process::group(&process);
    let targets = match pid {
        0 => process::matching(|target| process::group(target) == own_group),
        -1 => process::matching(|target| target.id != process.id && !process::is_init(target)),
        i32::MIN => return Err(ESRCH),
        pid if pid < 0 => process::matching(|target| process::group(target) == pid.unsigned_abs()),
        pid => process::matching(|target| target.id == pid as u32),
    };
    let info = Info::Sent {
        code: code::SI_USER,
        sender: process.id,
    };
    send_to(&targets, number, info)
}

/// `tkill(tid, sig)`: to one thread. Every process has one thread, with the process's ID.
pub fn tkill(tid: i32, number: u64) -> SyscallResult {
    if tid <= 0 {
        return Err(EINVAL);
    }
    let process = current()?;
    let targets = process::matching(|target| target.id == tid as u32);
    let info = Info::Sent {
        code: code::SI_TKILL,
        sender: process.id,
    };
    send_to(&targets, number, info)
}

/// `tgkill(tgid, tid, sig)`: `tkill`, checking that the thread belongs to process `tgid`.
pub fn tgkill(tgid: i32, tid: i32, number: u64) -> SyscallResult {
    if tgid <= 0 || tid <= 0 {
        return Err(EINVAL);
    }
    if tgid != tid {
        // Every thread ID is its own process's ID.
        return Err(ESRCH);
    }
    tkill(tid, number)
}
