// SPDX-License-Identifier: GPL-2.0-only

//! Signals, as in Linux (`man 7 signal`).
//!
//! A signal is a small number sent to a process: by another process (`kill`), by the kernel
//! (`SIGCHLD` when a child exits, `SIGPIPE` on a pipe nobody reads), or by the CPU through
//! the kernel (`SIGSEGV` for a bad memory access). For every signal a process chooses an
//! action (`rt_sigaction`): the default (end the process, or ignore), ignore, or call a
//! handler function. It may also block signals (`rt_sigprocmask`): they then stay pending
//! until unblocked.
//!
//! Delivery happens on the way back to user mode (`on_return`, called by `ochra-frame` after
//! every syscall, interrupt and exception). To call a handler, the kernel saves the
//! program's registers in a "signal frame" on the user stack, in Linux's layout
//! (`struct rt_sigframe`), and makes the program continue in the handler. The handler
//! returns to `sa_restorer`, a few instructions of the C library that call `rt_sigreturn`,
//! which loads the saved registers back.
//!
//! A task blocked in a syscall (reading an empty pipe, `wait4`…) is woken by a signal; the
//! syscall then returns `ERESTARTSYS` or similar, and `on_return` decides: run it again
//! (no handler, or the handler has `SA_RESTART`) or fail with `EINTR`.
//!
//! Simplifications for now: standard signals and real-time signals (32–64) are both
//! "one pending at a time" (Linux queues real-time ones); there is no job control (stop
//! signals are ignored by default), no `sigaltstack`, and every process may signal every
//! other (there are no users yet).

use alloc::boxed::Box;
use alloc::sync::Arc;
use alloc::vec;

use ochra_frame::task;
use ochra_frame::user_mode::{Registers, UserContext, UserFault};

use crate::errno::{EFAULT, EINTR, ERESTARTNOHAND, ERESTARTNOINTR, ERESTARTSYS, Errno};
use crate::process::{self, Exit, Process};
use crate::syscall::{read_user, write_user};

pub const SIGQUIT: u8 = 3;
pub const SIGILL: u8 = 4;
pub const SIGTRAP: u8 = 5;
pub const SIGABRT: u8 = 6;
pub const SIGBUS: u8 = 7;
pub const SIGFPE: u8 = 8;
pub const SIGKILL: u8 = 9;
pub const SIGSEGV: u8 = 11;
pub const SIGPIPE: u8 = 13;
pub const SIGCHLD: u8 = 17;
pub const SIGCONT: u8 = 18;
pub const SIGSTOP: u8 = 19;
pub const SIGTSTP: u8 = 20;
pub const SIGTTIN: u8 = 21;
pub const SIGTTOU: u8 = 22;
pub const SIGURG: u8 = 23;
pub const SIGXCPU: u8 = 24;
pub const SIGXFSZ: u8 = 25;
pub const SIGWINCH: u8 = 28;
pub const SIGSYS: u8 = 31;
/// The highest signal number (Linux's `_NSIG`).
pub const MAX_SIGNAL: u8 = 64;

/// A signal's bit in a signal set (`sigset_t`): bit 0 is signal 1.
pub fn bit(signal: u8) -> u64 {
    1 << (signal - 1)
}

/// `SIGKILL` and `SIGSTOP` can be neither blocked, ignored nor handled.
pub const UNBLOCKABLE: u64 = 1 << (SIGKILL - 1) | 1 << (SIGSTOP - 1);

/// Special handler values.
pub const SIG_DFL: u64 = 0;
pub const SIG_IGN: u64 = 1;

/// `sa_flags` bits (`include/uapi/asm-generic/signal-defs.h`, x86's `SA_RESTORER`).
pub mod flags {
    pub const SA_NOCLDWAIT: u64 = 2;
    pub const SA_RESTORER: u64 = 0x0400_0000;
    pub const SA_RESTART: u64 = 0x1000_0000;
    pub const SA_NODEFER: u64 = 0x4000_0000;
    pub const SA_RESETHAND: u64 = 0x8000_0000;
}

/// What a process does with one signal (`struct sigaction` as the kernel sees it).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Action {
    pub handler: u64,
    pub flags: u64,
    /// Where the handler returns to: code that calls `rt_sigreturn`.
    pub restorer: u64,
    /// Signals blocked while the handler runs (besides the signal itself).
    pub mask: u64,
}

/// What the default action does with a signal.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Default {
    Terminate,
    /// Terminate with a core dump in Linux; there are no core dumps here.
    Core,
    Ignore,
    /// Job control: stop the process. Ignored until there is job control.
    Stop,
}

fn default_action(signal: u8) -> Default {
    match signal {
        SIGQUIT | SIGILL | SIGTRAP | SIGABRT | SIGBUS | SIGFPE | SIGSEGV | SIGXCPU | SIGXFSZ
        | SIGSYS => Default::Core,
        SIGCHLD | SIGCONT | SIGURG | SIGWINCH => Default::Ignore,
        SIGSTOP | SIGTSTP | SIGTTIN | SIGTTOU => Default::Stop,
        _ => Default::Terminate,
    }
}

/// Where a signal came from, for the handler (`siginfo_t`).
#[derive(Clone, Copy, Debug)]
pub enum Info {
    /// Sent by a process (`kill`: `SI_USER`, `tkill`: `SI_TKILL`) or by the kernel.
    Sent { code: i32, sender: u32 },
    /// A child exited (`SIGCHLD`).
    Child { code: i32, child: u32, status: i32 },
    /// A CPU exception.
    Fault {
        code: i32,
        address: u64,
        vector: u8,
        error_code: u64,
    },
}

/// `si_code` values.
pub mod code {
    pub const SI_USER: i32 = 0;
    pub const SI_TKILL: i32 = -6;
    pub const SI_KERNEL: i32 = 0x80;
    pub const CLD_EXITED: i32 = 1;
    pub const CLD_KILLED: i32 = 2;
    pub const SEGV_MAPERR: i32 = 1;
    pub const SEGV_ACCERR: i32 = 2;
}

impl Info {
    /// `siginfo_t`: 128 bytes; the number, `errno` and code, then fields by kind.
    fn to_bytes(self, signal: u8) -> [u8; 128] {
        let mut bytes = [0; 128];
        bytes[0..4].copy_from_slice(&i32::from(signal).to_le_bytes());
        let code = match self {
            Self::Sent { code, sender } => {
                // `si_pid`, then `si_uid` (0, root: there are no users yet).
                bytes[16..20].copy_from_slice(&sender.to_le_bytes());
                code
            }
            Self::Child {
                code,
                child,
                status,
            } => {
                bytes[16..20].copy_from_slice(&child.to_le_bytes());
                bytes[24..28].copy_from_slice(&status.to_le_bytes());
                // `si_utime` and `si_stime` stay 0: no time accounting yet.
                code
            }
            Self::Fault { code, address, .. } => {
                bytes[16..24].copy_from_slice(&address.to_le_bytes());
                code
            }
        };
        bytes[8..12].copy_from_slice(&code.to_le_bytes());
        bytes
    }
}

/// A process's signal state. The tables are on the heap: kernel stacks are small, and the
/// state is moved around when a process is created.
pub struct Signals {
    /// By signal number minus 1.
    actions: Box<[Action]>,
    /// Signals that wait while blocked.
    pub blocked: u64,
    pending: u64,
    /// Where each pending signal came from.
    info: Box<[Option<Info>]>,
    /// The syscall that just returned a restart code (`ERESTARTSYS`…), by number; set by the
    /// syscall dispatcher, taken by `on_return` before going back to the program.
    pub restart: Option<u64>,
    /// The mask to restore after a handler started by `rt_sigsuspend` (which runs with a
    /// temporary mask).
    pub saved_mask: Option<u64>,
}

impl Signals {
    pub fn new() -> Self {
        Self {
            actions: vec![Action::default(); usize::from(MAX_SIGNAL)].into_boxed_slice(),
            blocked: 0,
            pending: 0,
            info: vec![None; usize::from(MAX_SIGNAL)].into_boxed_slice(),
            restart: None,
            saved_mask: None,
        }
    }

    /// The state of a `fork` child: the same actions and mask, nothing pending.
    pub fn for_child(&self) -> Self {
        Self {
            actions: self.actions.clone(),
            blocked: self.blocked,
            ..Self::new()
        }
    }

    /// `execve`: handlers belong to the old program, so they go back to the default;
    /// ignored signals stay ignored. The mask and pending signals stay.
    pub fn reset_handlers(&mut self) {
        for action in self.actions.iter_mut() {
            let ignored = action.handler == SIG_IGN;
            *action = Action::default();
            if ignored {
                action.handler = SIG_IGN;
            }
        }
    }

    pub fn action(&self, signal: u8) -> Action {
        self.actions[usize::from(signal - 1)]
    }

    /// Sets the action. A pending signal that is now ignored is dropped (as in Linux).
    pub fn set_action(&mut self, signal: u8, action: Action) {
        self.actions[usize::from(signal - 1)] = action;
        if self.ignores(signal) {
            self.discard(signal);
        }
    }

    /// Signals pending and blocked (`rt_sigpending`).
    pub fn pending_blocked(&self) -> u64 {
        self.pending & self.blocked
    }

    /// Whether the action for `signal` is to ignore it.
    fn ignores(&self, signal: u8) -> bool {
        match self.action(signal).handler {
            SIG_IGN => true,
            SIG_DFL => matches!(default_action(signal), Default::Ignore | Default::Stop),
            _ => false,
        }
    }

    /// Whether a child's exit should leave no zombie: `SIGCHLD` ignored explicitly, or
    /// `SA_NOCLDWAIT` (`man 2 wait`, "NOTES").
    pub fn reaps_children(&self) -> bool {
        let action = self.action(SIGCHLD);
        action.handler == SIG_IGN || action.flags & flags::SA_NOCLDWAIT != 0
    }

    fn discard(&mut self, signal: u8) {
        self.pending &= !bit(signal);
        self.info[usize::from(signal - 1)] = None;
    }

    fn deliverable(&self) -> u64 {
        self.pending & !self.blocked
    }

    /// Takes the lowest deliverable signal.
    fn take_next(&mut self) -> Option<(u8, Info, Action)> {
        let deliverable = self.deliverable();
        if deliverable == 0 {
            return None;
        }
        let signal = deliverable.trailing_zeros() as u8 + 1;
        let info = self.info[usize::from(signal - 1)].take();
        self.pending &= !bit(signal);
        let info = info.unwrap_or(Info::Sent {
            code: code::SI_KERNEL,
            sender: 0,
        });
        Some((signal, info, self.action(signal)))
    }
}

/// Sends `signal` to `process`. A signal it ignores is dropped at once (unless blocked:
/// the action may change before it is unblocked); a signal already pending is not queued
/// twice. Init never gets signals it has no handler for, so it cannot be killed by mistake
/// (as in Linux).
pub fn send(process: &Process, signal: u8, info: Info) {
    if process.exit().is_some() {
        return;
    }
    let wake = {
        let mut signals = process.signals.lock();
        let blocked = signals.blocked & bit(signal) != 0;
        let default = signals.action(signal).handler == SIG_DFL;
        if !blocked && (signals.ignores(signal) || default && process::is_init(process)) {
            return;
        }
        if signals.pending & bit(signal) == 0 {
            signals.pending |= bit(signal);
            signals.info[usize::from(signal - 1)] = Some(info);
        }
        !blocked
    };
    // A blocked task gives up its wait (see `interrupted`); a running one sees the signal
    // when it next returns to user mode, at the latest after one timer tick.
    if wake && let Some(task) = process.task() {
        crate::sched::wake(&task);
    }
}

/// Sends a signal the current program cannot go on without (a CPU exception): if it is
/// blocked or ignored, the default action applies, which ends the process. Delivered by
/// `on_return`.
pub fn force(process: &Process, signal: u8, info: Info) {
    let mut signals = process.signals.lock();
    let index = usize::from(signal - 1);
    if signals.blocked & bit(signal) != 0 || signals.actions[index].handler == SIG_IGN {
        signals.actions[index].handler = SIG_DFL;
        signals.blocked &= !bit(signal);
    }
    signals.pending |= bit(signal);
    signals.info[index] = Some(info);
}

/// Whether the current process has a signal to deliver: then a blocking syscall gives up
/// its wait.
pub fn interrupted() -> bool {
    process::current().is_some_and(|process| process.signals.lock().deliverable() != 0)
}

/// The signal for a CPU exception in user mode, as Linux chooses it, with its `si_code`.
pub fn for_fault(fault: &UserFault) -> (u8, Info) {
    const PAGE_FAULT: u8 = 14;
    /// Page fault error code bit 0: the page was present (a protection fault).
    const PRESENT: u64 = 1;
    let signal = match fault.vector {
        0 | 16 | 19 => SIGFPE, // divide error, x87 and SIMD floating point
        1 | 3 => SIGTRAP,      // debug, breakpoint
        6 => SIGILL,           // invalid opcode
        17 => SIGBUS,          // alignment check
        _ => SIGSEGV,          // page fault, general protection and the rest
    };
    let (code, address) = match (fault.vector, fault.address) {
        (PAGE_FAULT, Some(address)) if fault.error_code & PRESENT != 0 => {
            (code::SEGV_ACCERR, address)
        }
        (PAGE_FAULT, Some(address)) => (code::SEGV_MAPERR, address),
        // A general protection fault has no address; Linux reports it from the kernel.
        _ => (code::SI_KERNEL, 0),
    };
    let info = Info::Fault {
        code,
        address,
        vector: fault.vector,
        error_code: fault.error_code,
    };
    (signal, info)
}

/// Called before every return to user mode: finishes an interrupted syscall (run it again,
/// or `EINTR`) and delivers the pending signals that are not blocked.
pub fn on_return(context: &mut UserContext) {
    let Some(process) = process::current() else {
        return;
    };
    let mut restart = process.signals.lock().restart.take();
    loop {
        let Some((signal, info, action)) = process.signals.lock().take_next() else {
            break;
        };
        match action.handler {
            SIG_IGN => continue,
            SIG_DFL => match default_action(signal) {
                Default::Ignore | Default::Stop => continue,
                Default::Terminate | Default::Core => process::exit(process, Exit::Signal(signal)),
            },
            _ => {}
        }
        if let Some(number) = restart.take() {
            finish_syscall(context, number, Some(action.flags));
        }
        let mask = {
            let signals = process.signals.lock();
            signals.saved_mask.unwrap_or(signals.blocked)
        };
        if setup_frame(context, signal, info, &action, mask).is_err() {
            // No room for the frame (or no restorer): the program cannot go on.
            process::exit(process, Exit::Signal(SIGSEGV));
        }
        let mut signals = process.signals.lock();
        signals.saved_mask = None;
        signals.blocked |= action.mask;
        if action.flags & flags::SA_NODEFER == 0 {
            signals.blocked |= bit(signal);
        }
        signals.blocked &= !UNBLOCKABLE;
        if action.flags & flags::SA_RESETHAND != 0 {
            signals.actions[usize::from(signal - 1)] = Action::default();
        }
    }
    // No handler ran: an interrupted syscall simply runs again, and the temporary mask of
    // `rt_sigsuspend` goes away.
    if let Some(number) = restart {
        finish_syscall(context, number, None);
    }
    let mut signals = process.signals.lock();
    if let Some(mask) = signals.saved_mask.take() {
        signals.blocked = mask;
    }
}

/// Turns the restart code in RAX of syscall `number` into what the program sees: `EINTR`,
/// or the same syscall again (RAX back to the number, RIP back over the 2-byte `syscall`).
/// `handler_flags` is `None` when no handler runs.
fn finish_syscall(context: &mut UserContext, number: u64, handler_flags: Option<u64>) {
    let code = -(context.registers().rax as i64);
    let fail = match (code, handler_flags) {
        (ERESTARTNOINTR, _) | (_, None) => false,
        (ERESTARTSYS, Some(flags)) => flags & flags::SA_RESTART == 0,
        (ERESTARTNOHAND, Some(_)) => true,
        _ => return,
    };
    if fail {
        context.set_result((-EINTR) as u64);
    } else {
        context.set_result(number);
        context.set_instruction_pointer(context.instruction_pointer() - 2);
    }
}

/// The layout of `struct rt_sigframe`: the return address (to `sa_restorer`), then
/// `struct ucontext` (`uc_flags`, `uc_link`, `uc_stack`, `uc_mcontext`, `uc_sigmask`), then
/// `siginfo_t`. The x87/SSE state goes separately, above it.
mod frame {
    pub const UCONTEXT: u64 = 8;
    /// In the `ucontext`: `uc_stack.ss_flags`, `uc_mcontext` and `uc_sigmask`.
    pub const STACK_FLAGS: usize = 16 + 8;
    pub const MCONTEXT: usize = 40;
    pub const SIGMASK: usize = 296;
    pub const UCONTEXT_SIZE: usize = 304;
    pub const INFO: u64 = UCONTEXT + UCONTEXT_SIZE as u64;
    pub const SIZE: u64 = INFO + 128;
    /// In `uc_mcontext` (`struct sigcontext`): the 18 registers, the selectors, then these.
    pub const ERROR_CODE: usize = 152;
    pub const TRAP_NUMBER: usize = 160;
    pub const OLD_MASK: usize = 168;
    pub const FAULT_ADDRESS: usize = 176;
    pub const FPU_STATE: usize = 184;
    /// `uc_stack.ss_flags`: no alternate signal stack.
    pub const SS_DISABLE: u32 = 2;
    /// Bytes below the stack pointer the program may use without moving it (the System V
    /// "red zone"); the frame goes below them.
    pub const RED_ZONE: u64 = 128;
    pub const FPU_SIZE: u64 = 512;
}

/// The order of the registers in `struct sigcontext`.
fn register_list(registers: &mut Registers) -> [&mut u64; 18] {
    let r = registers;
    [
        &mut r.r8,
        &mut r.r9,
        &mut r.r10,
        &mut r.r11,
        &mut r.r12,
        &mut r.r13,
        &mut r.r14,
        &mut r.r15,
        &mut r.rdi,
        &mut r.rsi,
        &mut r.rbp,
        &mut r.rbx,
        &mut r.rdx,
        &mut r.rax,
        &mut r.rcx,
        &mut r.rsp,
        &mut r.rip,
        &mut r.rflags,
    ]
}

/// Writes the signal frame below the program's stack pointer and makes the program continue
/// in the handler: `handler(signal, &info, &ucontext)`, returning to `sa_restorer`.
fn setup_frame(
    context: &mut UserContext,
    signal: u8,
    info: Info,
    action: &Action,
    mask: u64,
) -> Result<(), Errno> {
    // x86-64 programs must give a restorer (the C library always does).
    if action.flags & flags::SA_RESTORER == 0 {
        return Err(EFAULT);
    }
    let mut registers = context.registers();
    let below = |address: u64, size: u64| address.checked_sub(size).ok_or(EFAULT);
    // As Linux's `get_sigframe`: the FPU state 64-byte aligned, the frame so that the
    // stack pointer is 16-byte aligned plus 8, as right after a `call`.
    let fpu_address = below(below(registers.rsp, frame::RED_ZONE)?, frame::FPU_SIZE)? & !63;
    let frame_address = below(below(fpu_address, frame::SIZE)? & !15, 8)?;

    let mut bytes = [0; frame::SIZE as usize];
    bytes[..8].copy_from_slice(&action.restorer.to_le_bytes());
    let ucontext = &mut bytes[frame::UCONTEXT as usize..frame::INFO as usize];
    ucontext[frame::STACK_FLAGS..][..4].copy_from_slice(&frame::SS_DISABLE.to_le_bytes());
    let mcontext = &mut ucontext[frame::MCONTEXT..frame::SIGMASK];
    for (index, value) in register_list(&mut registers).into_iter().enumerate() {
        mcontext[index * 8..][..8].copy_from_slice(&value.to_le_bytes());
    }
    if let Info::Fault {
        address,
        vector,
        error_code,
        ..
    } = info
    {
        mcontext[frame::ERROR_CODE..][..8].copy_from_slice(&error_code.to_le_bytes());
        mcontext[frame::TRAP_NUMBER..][..8].copy_from_slice(&u64::from(vector).to_le_bytes());
        mcontext[frame::FAULT_ADDRESS..][..8].copy_from_slice(&address.to_le_bytes());
    }
    mcontext[frame::OLD_MASK..][..8].copy_from_slice(&mask.to_le_bytes());
    mcontext[frame::FPU_STATE..][..8].copy_from_slice(&fpu_address.to_le_bytes());
    ucontext[frame::SIGMASK..][..8].copy_from_slice(&mask.to_le_bytes());
    bytes[frame::INFO as usize..].copy_from_slice(&info.to_bytes(signal));

    write_user(fpu_address, &task::fpu_state())?;
    write_user(frame_address, &bytes)?;

    /// Cleared for the handler, as in Linux: single-stepping (TF) and the direction (DF).
    const TRAP_AND_DIRECTION: u64 = 1 << 8 | 1 << 10;
    context.set_registers(&Registers {
        rip: action.handler,
        rsp: frame_address,
        rdi: u64::from(signal),
        rsi: frame_address + frame::INFO,
        rdx: frame_address + frame::UCONTEXT,
        rax: 0,
        rflags: registers.rflags & !TRAP_AND_DIRECTION,
        ..registers
    });
    // The handler starts with a clean x87/SSE state; the program's is in the frame.
    task::reset_fpu();
    Ok(())
}

/// `rt_sigreturn()`: back from a handler. The handler's `ret` popped the return address, so
/// the stack pointer is at the `ucontext`. Every register comes back from it (the program
/// may have changed them; `set_registers` keeps that harmless), and so do the x87/SSE state
/// and the signal mask. The result is the restored RAX.
pub fn sigreturn(process: &Arc<Process>, context: &mut UserContext) -> Result<u64, Errno> {
    let mut ucontext = [0; frame::UCONTEXT_SIZE];
    if read_user(&mut ucontext, context.stack_pointer()).is_err() {
        return Err(bad_frame(process));
    }
    let word = |offset: usize| u64::from_le_bytes(ucontext[offset..][..8].try_into().unwrap());
    let mut registers = Registers::default();
    for (index, value) in register_list(&mut registers).into_iter().enumerate() {
        *value = word(frame::MCONTEXT + index * 8);
    }
    let fpu_address = word(frame::MCONTEXT + frame::FPU_STATE);
    if fpu_address == 0 {
        task::reset_fpu();
    } else {
        let mut fpu = [0; frame::FPU_SIZE as usize];
        if read_user(&mut fpu, fpu_address).is_err() {
            return Err(bad_frame(process));
        }
        task::set_fpu_state(&fpu);
    }
    context.set_registers(&registers);
    process.signals.lock().blocked = word(frame::SIGMASK) & !UNBLOCKABLE;
    Ok(registers.rax)
}

/// A signal frame the program broke: `SIGSEGV`, as in Linux.
fn bad_frame(process: &Process) -> Errno {
    let info = Info::Sent {
        code: code::SI_KERNEL,
        sender: 0,
    };
    force(process, SIGSEGV, info);
    EFAULT
}

/// The `si_code` of a `SIGCHLD` and the status for a child that ended with `exit`.
pub fn child_info(child: u32, exit: Exit) -> Info {
    let (code, status) = match exit {
        Exit::Status(status) => (code::CLD_EXITED, i32::from(status)),
        Exit::Signal(signal) => (code::CLD_KILLED, i32::from(signal)),
    };
    Info::Child {
        code,
        child,
        status,
    }
}
