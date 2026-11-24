// SPDX-License-Identifier: GPL-2.0-only

//! Terminals (TTYs), as in Linux.
//!
//! A terminal is a driver (the serial port, or the master side of a pseudo-terminal) with a
//! **line discipline** above it: the part that turns typed bytes into what programs read.
//! Its settings are `termios` (`man 3 termios`):
//!
//! - in canonical mode (`ICANON`) input comes in lines, which can be edited (ERASE, KILL,
//!   WERASE…) before Enter hands them over; otherwise (raw mode, as editors and BusyBox's
//!   line editor use) bytes come as they arrive, as `VMIN`/`VTIME` say;
//! - typed characters are echoed (`ECHO`…), and INTR (Ctrl-C), QUIT (Ctrl-\) and SUSP
//!   (Ctrl-Z) become signals for the terminal's foreground process group (`ISIG`);
//! - output is processed too (`OPOST`: NL becomes CR NL).
//!
//! A terminal can be the **controlling terminal** of one session. Only its foreground
//! process group may read from it: a background group gets `SIGTTIN` (and `SIGTTOU` for
//! writes with `TOSTOP`, and for changing the settings), which stops it — job control.
//!
//! `/dev/console` and `/dev/ttyS0` are the serial port (`serial`); `/dev/ptmx` and
//! `/dev/pts/<n>` are pseudo-terminals (`pty`); `/dev/tty` is the caller's controlling
//! terminal.

mod pty;
mod serial;
pub mod termios;

use alloc::collections::VecDeque;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, AtomicU32, Ordering};

use log::debug;
use ochra_frame::sync::IrqMutex;

use crate::errno::{EAGAIN, EBUSY, EINVAL, EIO, ENOTTY, ENXIO, EPERM, ERESTARTSYS, ESRCH, Errno};
use crate::file::{self, File, events, flags};
use crate::process::{self, Process};
use crate::sched::{self, WaitQueue};
use crate::signal::{self, SIGCONT, SIGHUP, SIGINT, SIGQUIT, SIGTSTP, SIGTTIN, SIGTTOU, SIGWINCH};
use crate::syscall::{read_user, write_user};
use crate::time;
use crate::vfs::Metadata;
use termios::index::*;
use termios::{Termios, WindowSize, input, local, output};

pub use pty::devpts_root;
pub use serial::poll as poll_serial;

/// How many bytes of input a terminal holds (Linux's `N_TTY_BUF_SIZE`).
const INPUT_SIZE: usize = 4096;

/// `ioctl` requests (`include/uapi/asm-generic/ioctls.h`).
mod request {
    pub const TCGETS: u64 = 0x5401;
    pub const TCSETS: u64 = 0x5402;
    pub const TCSETSW: u64 = 0x5403;
    pub const TCSETSF: u64 = 0x5404;
    pub const TCSBRK: u64 = 0x5409;
    pub const TCXONC: u64 = 0x540a;
    pub const TCFLSH: u64 = 0x540b;
    pub const TIOCEXCL: u64 = 0x540c;
    pub const TIOCNXCL: u64 = 0x540d;
    pub const TIOCSCTTY: u64 = 0x540e;
    pub const TIOCGPGRP: u64 = 0x540f;
    pub const TIOCSPGRP: u64 = 0x5410;
    pub const TIOCOUTQ: u64 = 0x5411;
    pub const TIOCSTI: u64 = 0x5412;
    pub const TIOCGWINSZ: u64 = 0x5413;
    pub const TIOCSWINSZ: u64 = 0x5414;
    pub const FIONREAD: u64 = 0x541b;
    pub const TIOCNOTTY: u64 = 0x5422;
    pub const TCSBRKP: u64 = 0x5425;
    pub const TIOCGSID: u64 = 0x5429;
    pub const TCGETS2: u64 = 0x802c_542a;
    pub const TCSETS2: u64 = 0x402c_542b;
    pub const TCSETSW2: u64 = 0x402c_542c;
    pub const TCSETSF2: u64 = 0x402c_542d;
    pub const TIOCGPTN: u64 = 0x8004_5430;
    pub const TIOCSPTLCK: u64 = 0x4004_5431;
    pub const TIOCGDEV: u64 = 0x8004_5432;
    pub const TIOCGPTLCK: u64 = 0x8004_5439;
}

/// What is below a line discipline.
pub trait Driver: Send + Sync {
    /// Sends processed output; returns when all of it is taken (a pseudo-terminal's master
    /// may be full: then it waits, or fails with `EAGAIN` if `nonblocking`).
    fn write(&self, data: &[u8], nonblocking: bool) -> Result<(), Errno>;

    /// Sends echo. Called while input is processed, maybe in an interrupt: never waits,
    /// drops what does not fit.
    fn echo(&self, data: &[u8]);

    /// Whether `write` would take something now.
    fn writable(&self) -> bool {
        true
    }

    /// Woken when `writable` may have become true.
    fn write_queue(&self) -> Option<&WaitQueue> {
        None
    }
}

/// A terminal: a driver and the line discipline above it.
pub struct Tty {
    /// The name under `/dev` (`ttyS0`, `pts/3`).
    pub name: String,
    /// Device number (`st_rdev` of its node).
    pub device: u64,
    driver: Arc<dyn Driver>,
    state: IrqMutex<State>,
    /// Woken when input arrives, when the settings change and on hangup.
    readable: WaitQueue,
    /// Woken when a reader made room in the input (a pseudo-terminal's master waits here).
    room: WaitQueue,
    /// Woken when stopped output (Ctrl-S) starts again.
    output_started: WaitQueue,
    /// Open files of this terminal, and whether there ever was one (a pseudo-terminal's
    /// master sees "hung up" once the last one closes).
    opens: AtomicU32,
    ever_opened: AtomicBool,
}

struct State {
    termios: Termios,
    window: WindowSize,
    /// Canonical mode: the line being typed, and finished lines (an empty one is an end of
    /// file, from EOF on an empty line).
    line: Vec<u8>,
    lines: VecDeque<Vec<u8>>,
    /// Raw mode: bytes as they arrived.
    raw: VecDeque<u8>,
    /// LNEXT was typed: the next character is taken literally.
    literal_next: bool,
    /// Output stopped by STOP (Ctrl-S) or `TCOOFF`.
    output_stopped: bool,
    /// When the last byte arrived (monotonic), for `VTIME` between bytes.
    last_input: u64,
    /// The session this is the controlling terminal of, and its foreground group.
    session: Option<u32>,
    foreground: Option<u32>,
    /// The other side went away (a pseudo-terminal's master closed): reads and writes fail.
    hung_up: bool,
    /// `TIOCEXCL`: only root may open it again.
    exclusive: bool,
    /// Changes with every new setting, so a waiting reader re-reads them.
    generation: u64,
}

/// What processing input asks for once the state lock is released.
#[derive(Default)]
struct Effects {
    echo: Vec<u8>,
    /// Signals for the foreground group.
    signals: Vec<(u8, u32)>,
    readable: bool,
    output_started: bool,
}

/// Whether `byte` is a control character (`iscntrl`).
fn is_control(byte: u8) -> bool {
    byte < 0x20 || byte == 0x7f
}

impl State {
    fn new(termios: Termios) -> Self {
        Self {
            termios,
            window: WindowSize::default(),
            line: Vec::new(),
            lines: VecDeque::new(),
            raw: VecDeque::new(),
            literal_next: false,
            output_stopped: false,
            last_input: 0,
            session: None,
            foreground: None,
            hung_up: false,
            exclusive: false,
            generation: 0,
        }
    }

    fn canonical(&self) -> bool {
        self.termios.has_local(local::ICANON)
    }

    fn buffered(&self) -> usize {
        self.line.len() + self.raw.len() + self.lines.iter().map(Vec::len).sum::<usize>()
    }

    /// Bytes a read could take now (`FIONREAD`): complete lines, or everything in raw mode.
    fn available(&self) -> usize {
        if self.canonical() {
            self.lines.iter().map(Vec::len).sum()
        } else {
            self.raw.len()
        }
    }

    fn input_ready(&self) -> bool {
        if self.canonical() {
            !self.lines.is_empty()
        } else {
            !self.raw.is_empty()
        }
    }

    fn flush_input(&mut self) {
        self.line.clear();
        self.lines.clear();
        self.raw.clear();
        self.literal_next = false;
    }

    /// Output processing (`OPOST`) of one byte.
    fn output_byte(&self, byte: u8, out: &mut Vec<u8>) {
        let oflag = self.termios.oflag;
        if oflag & output::OPOST == 0 {
            out.push(byte);
            return;
        }
        match byte {
            b'\n' if oflag & output::ONLCR != 0 => out.extend_from_slice(b"\r\n"),
            b'\r' if oflag & output::OCRNL != 0 => out.push(b'\n'),
            byte if oflag & output::OLCUC != 0 => out.push(byte.to_ascii_uppercase()),
            byte => out.push(byte),
        }
    }

    /// Echoes one input byte: a control character as `^X` with `ECHOCTL` (Linux's
    /// `echo_char`).
    fn echo_char(&self, byte: u8, effects: &mut Effects) {
        if !self.termios.has_local(local::ECHO) {
            return;
        }
        if self.termios.has_local(local::ECHOCTL)
            && is_control(byte)
            && byte != b'\t'
            && byte != b'\n'
        {
            effects.echo.extend_from_slice(&[b'^', byte ^ 0x40]);
        } else {
            self.output_byte(byte, &mut effects.echo);
        }
    }

    /// Erases the last character on the screen: `^X` takes two columns.
    fn echo_erase(&self, erased: u8, effects: &mut Effects) {
        let columns =
            if self.termios.has_local(local::ECHOCTL) && is_control(erased) && erased != b'\t' {
                2
            } else {
                1
            };
        for _ in 0..columns {
            effects.echo.extend_from_slice(b"\x08 \x08");
        }
    }

    /// Canonical editing: ERASE (one character), WERASE (a word) and KILL (the line), as
    /// Linux's `eraser`.
    fn erase(&mut self, kind: usize, typed: u8, effects: &mut Effects) {
        if self.line.is_empty() {
            return;
        }
        let echo = self.termios.has_local(local::ECHO);
        let echo_e = self.termios.has_local(local::ECHOE);
        if kind == VKILL {
            let visual = echo_e
                && self.termios.has_local(local::ECHOK)
                && self.termios.has_local(local::ECHOKE);
            if echo && !visual {
                self.line.clear();
                self.echo_char(typed, effects);
                if self.termios.has_local(local::ECHOK) {
                    self.output_byte(b'\n', &mut effects.echo);
                }
                return;
            }
        }
        let mut seen_word = false;
        while let Some(&last) = self.line.last() {
            if kind == VWERASE {
                let word = last.is_ascii_alphanumeric() || last == b'_';
                if word {
                    seen_word = true;
                } else if seen_word {
                    break;
                }
            }
            self.line.pop();
            if echo {
                if kind == VERASE && !echo_e {
                    self.echo_char(typed, effects);
                } else {
                    self.echo_erase(last, effects);
                }
            }
            if kind == VERASE {
                break;
            }
        }
    }

    /// The line is complete: readers may take it.
    fn finish_line(&mut self, effects: &mut Effects) {
        let line = core::mem::take(&mut self.line);
        self.lines.push_back(line);
        effects.readable = true;
    }

    /// Processes input bytes as Linux's `n_tty_receive_char_special`; returns how many were
    /// taken (fewer only when the raw input is full).
    fn receive(&mut self, bytes: &[u8], effects: &mut Effects) -> usize {
        for (taken, &received) in bytes.iter().enumerate() {
            if !self.canonical() && self.raw.len() >= INPUT_SIZE - 1 {
                return taken;
            }
            self.receive_byte(received, effects);
        }
        self.last_input = time::monotonic();
        bytes.len()
    }

    fn receive_byte(&mut self, received: u8, effects: &mut Effects) {
        let termios = self.termios;
        let mut byte = received;
        if termios.has_input(input::ISTRIP) {
            byte &= 0x7f;
        }
        if self.literal_next {
            self.literal_next = false;
            self.store(byte, effects);
            return;
        }
        let is = |index: usize| termios.character(index) == Some(byte);
        if termios.has_input(input::IXON) {
            if is(VSTART) {
                self.output_stopped = false;
                effects.output_started = true;
                return;
            }
            if is(VSTOP) {
                self.output_stopped = true;
                return;
            }
        }
        if termios.has_local(local::ISIG) {
            let signal = [(VINTR, SIGINT), (VQUIT, SIGQUIT), (VSUSP, SIGTSTP)]
                .into_iter()
                .find(|&(index, _)| is(index));
            if let Some((_, signal)) = signal {
                if let Some(group) = self.foreground {
                    effects.signals.push((signal, group));
                }
                if !termios.has_local(local::NOFLSH) {
                    self.flush_input();
                }
                if termios.has_input(input::IXON) {
                    self.output_stopped = false;
                    effects.output_started = true;
                }
                self.echo_char(byte, effects);
                return;
            }
        }
        if self.output_stopped && termios.has_input(input::IXANY) {
            self.output_stopped = false;
            effects.output_started = true;
        }
        if byte == b'\r' {
            if termios.has_input(input::IGNCR) {
                return;
            }
            if termios.has_input(input::ICRNL) {
                byte = b'\n';
            }
        } else if byte == b'\n' && termios.has_input(input::INLCR) {
            byte = b'\r';
        }
        if !self.canonical() {
            self.store(byte, effects);
            return;
        }
        let extended = termios.has_local(local::IEXTEN);
        let is = |index: usize| termios.character(index) == Some(byte);
        if is(VERASE) || is(VKILL) || (extended && is(VWERASE)) {
            let kind = [VERASE, VKILL, VWERASE]
                .into_iter()
                .find(|&index| is(index))
                .unwrap_or(VERASE);
            self.erase(kind, byte, effects);
            return;
        }
        if extended && is(VLNEXT) {
            self.literal_next = true;
            if termios.has_local(local::ECHO) && termios.has_local(local::ECHOCTL) {
                effects.echo.extend_from_slice(b"^\x08");
            }
            return;
        }
        if extended && is(VREPRINT) && termios.has_local(local::ECHO) {
            self.echo_char(byte, effects);
            self.output_byte(b'\n', &mut effects.echo);
            for index in 0..self.line.len() {
                self.echo_char(self.line[index], effects);
            }
            return;
        }
        if byte == b'\n' {
            if termios.has_local(local::ECHO) || termios.has_local(local::ECHONL) {
                self.output_byte(b'\n', &mut effects.echo);
            }
            self.line.push(byte);
            self.finish_line(effects);
            return;
        }
        if is(VEOF) {
            // The line goes as it is, without the EOF character; empty, it is an end of file.
            self.finish_line(effects);
            return;
        }
        if is(VEOL) || (extended && is(VEOL2)) {
            self.echo_char(byte, effects);
            self.line.push(byte);
            self.finish_line(effects);
            return;
        }
        self.store(byte, effects);
    }

    /// An ordinary character: echoed and kept.
    fn store(&mut self, byte: u8, effects: &mut Effects) {
        if self.canonical() {
            // A full buffer drops characters (Linux leaves room only for the newline).
            if self.buffered() >= INPUT_SIZE - 1 {
                return;
            }
            self.echo_char(byte, effects);
            self.line.push(byte);
        } else {
            self.echo_char(byte, effects);
            self.raw.push_back(byte);
            effects.readable = true;
        }
    }

    /// New settings. Switching between canonical and raw mode keeps the input, as Linux's
    /// `n_tty_set_termios`: raw bytes become a finished line (without a newline), finished
    /// lines and the typed line become raw bytes.
    fn set_termios(&mut self, termios: Termios) {
        let was_canonical = self.canonical();
        self.termios = termios;
        let canonical = self.canonical();
        if was_canonical && !canonical {
            let lines: Vec<u8> = self.lines.drain(..).flatten().collect();
            self.raw.extend(lines);
            self.raw.extend(self.line.drain(..));
        } else if !was_canonical && canonical && !self.raw.is_empty() {
            let pending: Vec<u8> = self.raw.drain(..).collect();
            self.lines.push_back(pending);
        }
        self.generation += 1;
    }
}

impl Tty {
    pub fn new(name: String, device: u64, driver: Arc<dyn Driver>, termios: Termios) -> Arc<Self> {
        Arc::new(Self {
            name,
            device,
            driver,
            state: IrqMutex::new(State::new(termios)),
            readable: WaitQueue::new(),
            room: WaitQueue::new(),
            output_started: WaitQueue::new(),
            opens: AtomicU32::new(0),
            ever_opened: AtomicBool::new(false),
        })
    }

    /// The session this is the controlling terminal of.
    pub fn session(&self) -> Option<u32> {
        self.state.lock().session
    }

    /// The foreground process group.
    pub fn foreground(&self) -> Option<u32> {
        self.state.lock().foreground
    }

    /// Input from the driver (typed keys, or a pseudo-terminal's master writing). Returns how
    /// many bytes were taken. May run in an interrupt handler.
    pub fn receive(&self, bytes: &[u8]) -> usize {
        let mut effects = Effects::default();
        let taken = self.state.lock().receive(bytes, &mut effects);
        if !effects.echo.is_empty() {
            self.driver.echo(&effects.echo);
        }
        for (signal, group) in effects.signals {
            signal::send_to_group(group, signal, signal::from_kernel());
        }
        if effects.readable {
            self.readable.wake_all();
        }
        if effects.output_started {
            self.output_started.wake_all();
        }
        taken
    }

    /// Whether `receive` would take more raw input now.
    fn has_room(&self) -> bool {
        let state = self.state.lock();
        state.canonical() || state.raw.len() < INPUT_SIZE - 1
    }

    /// Job control for the caller (Linux's `__tty_check_change`): a process in the background
    /// of its controlling terminal gets `signal` (and the syscall waits for it to be
    /// continued), unless it blocks or ignores the signal (then `SIGTTIN` fails with `EIO`,
    /// `SIGTTOU` lets it through) or its group is orphaned (`EIO`).
    fn job_check(self: &Arc<Self>, signal: u8) -> Result<(), Errno> {
        let Some(process) = process::current() else {
            return Ok(());
        };
        if !process::controlling_tty(&process).is_some_and(|tty| Arc::ptr_eq(&tty, self)) {
            return Ok(());
        }
        let group = process.group();
        match self.foreground() {
            Some(foreground) if foreground != group => {}
            _ => return Ok(()),
        }
        if process.signals.lock().blocks_or_ignores(signal) {
            return if signal == SIGTTIN { Err(EIO) } else { Ok(()) };
        }
        if process::is_orphaned_group(group, None) {
            return Err(EIO);
        }
        signal::send_to_group(group, signal, signal::from_kernel());
        Err(ERESTARTSYS)
    }

    pub fn read(self: &Arc<Self>, buffer: &mut [u8], nonblocking: bool) -> Result<usize, Errno> {
        self.job_check(SIGTTIN)?;
        if buffer.is_empty() {
            return Ok(0);
        }
        let started = time::monotonic();
        loop {
            let (generation, timed) = {
                let state = self.state.lock();
                (
                    state.generation,
                    !state.canonical() && state.termios.cc[VTIME] != 0,
                )
            };
            let mut queues = alloc::vec![&self.readable];
            if timed {
                queues.push(time::ticks_queue());
            }
            let result = sched::wait_any_interruptible(&queues, || {
                let mut state = self.state.lock();
                if state.generation != generation {
                    return Some(None);
                }
                let result = if state.canonical() {
                    read_line(&mut state, buffer)
                } else {
                    read_raw(&mut state, buffer, started)
                };
                match result {
                    Some(count) => Some(Some(Ok(count))),
                    // A hung-up terminal reads as an end of file (Linux's `hung_up_tty_read`).
                    None if state.hung_up => Some(Some(Ok(0))),
                    None if nonblocking => Some(Some(Err(EAGAIN))),
                    None => None,
                }
            })?;
            if let Some(result) = result {
                if matches!(result, Ok(count) if count > 0) {
                    self.room.wake_all();
                }
                return result;
            }
        }
    }

    pub fn write(self: &Arc<Self>, data: &[u8], nonblocking: bool) -> Result<usize, Errno> {
        if self.state.lock().termios.has_local(local::TOSTOP) {
            self.job_check(SIGTTOU)?;
        }
        let mut written = 0;
        for chunk in data.chunks(256) {
            let processed = self.output_started.wait_interruptible(|| {
                let state = self.state.lock();
                if state.hung_up {
                    return Some(Err(EIO));
                }
                if state.output_stopped {
                    return nonblocking.then_some(Err(EAGAIN));
                }
                let mut out = Vec::with_capacity(chunk.len() + 8);
                for &byte in chunk {
                    state.output_byte(byte, &mut out);
                }
                Some(Ok(out))
            });
            let result = processed
                .and_then(|out| out)
                .and_then(|out| self.driver.write(&out, nonblocking));
            match result {
                Ok(()) => written += chunk.len(),
                // What went through counts; the error shows on the next call.
                Err(_) if written > 0 => break,
                Err(error) => return Err(error),
            }
        }
        Ok(written)
    }

    pub fn poll(&self) -> u32 {
        let state = self.state.lock();
        if state.hung_up {
            // Linux's `hung_up_tty_poll`: everything at once, so nobody waits.
            return events::IN
                | events::OUT
                | events::ERR
                | events::HUP
                | events::RDNORM
                | events::WRNORM;
        }
        let mut ready = 0;
        if state.input_ready() {
            ready |= events::IN | events::RDNORM;
        }
        if !state.output_stopped && self.driver.writable() {
            ready |= events::OUT | events::WRNORM;
        }
        ready
    }

    pub fn wait_queues<'a>(&'a self, queues: &mut Vec<&'a WaitQueue>) {
        queues.extend([&self.readable, &self.output_started]);
        queues.extend(self.driver.write_queue());
    }

    /// The other side is gone (a pseudo-terminal's master closed): reads and writes fail
    /// from now on, and the session hears of it: its leader gets `SIGHUP` and `SIGCONT`,
    /// the foreground group `SIGHUP` (Linux's `__tty_hangup`).
    pub fn hang_up(&self) {
        let (session, foreground) = {
            let mut state = self.state.lock();
            state.hung_up = true;
            (state.session.take(), state.foreground.take())
        };
        self.readable.wake_all();
        self.room.wake_all();
        self.output_started.wake_all();
        if let Some(leader) = session.and_then(process::find) {
            signal::send(&leader, SIGHUP, signal::from_kernel());
            signal::send(&leader, SIGCONT, signal::from_kernel());
        }
        if let Some(group) = foreground {
            signal::send_to_group(group, SIGHUP, signal::from_kernel());
        }
    }

    /// The session gives up this terminal (its leader exited, or called `TIOCNOTTY`): the
    /// foreground group gets `SIGHUP`, and `SIGCONT` unless the leader exited (Linux's
    /// `disassociate_ctty`).
    pub fn disassociate(&self, on_exit: bool) {
        let foreground = {
            let mut state = self.state.lock();
            state.session = None;
            state.foreground.take()
        };
        if let Some(group) = foreground {
            signal::send_to_group(group, SIGHUP, signal::from_kernel());
            if !on_exit {
                signal::send_to_group(group, SIGCONT, signal::from_kernel());
            }
        }
    }

    /// Makes this the controlling terminal of `process`'s session.
    fn become_controlling(self: &Arc<Self>, state: &mut State, process: &Process) {
        state.session = Some(process.session());
        state.foreground = Some(process.group());
        *process.controlling_tty.lock() = Some(self.clone());
    }

    /// An `open` of the terminal: a session leader without a controlling terminal gets this
    /// one, if it is free and `O_NOCTTY` was not given (`man 4 tty`).
    fn opened(self: &Arc<Self>, open_flags: u32, may_control: bool) -> Result<(), Errno> {
        const O_NOCTTY: u32 = 0o400;
        let process = process::current();
        let is_root = process
            .as_ref()
            .is_none_or(|process| process.credentials.lock().is_root());
        let mut state = self.state.lock();
        if state.exclusive && !is_root {
            return Err(EBUSY);
        }
        if state.hung_up {
            return Err(EIO);
        }
        if let Some(process) = process
            && may_control
            && open_flags & O_NOCTTY == 0
            && process.session() == process.id
            && process::controlling_tty(&process).is_none()
            && state.session.is_none()
        {
            self.become_controlling(&mut state, &process);
        }
        Ok(())
    }

    /// The terminal's own `ioctl`s.
    fn ioctl(self: &Arc<Self>, request: u64, argument: u64) -> Result<u64, Errno> {
        use request::*;
        if self.state.lock().hung_up {
            // Linux's `hung_up_tty_ioctl`.
            return Err(if request == TIOCSPGRP { ENOTTY } else { EIO });
        }
        match request {
            TCGETS => {
                let termios = self.state.lock().termios;
                write_user(argument, &termios.to_bytes())?;
                Ok(0)
            }
            TCGETS2 => {
                let termios = self.state.lock().termios;
                write_user(argument, &termios.to_bytes2())?;
                Ok(0)
            }
            TCSETS | TCSETSW | TCSETSF | TCSETS2 | TCSETSW2 | TCSETSF2 => {
                self.job_check(SIGTTOU)?;
                let mut bytes = [0; termios::SIZE];
                read_user(&mut bytes, argument)?;
                {
                    let mut state = self.state.lock();
                    if matches!(request, TCSETSF | TCSETSF2) {
                        state.flush_input();
                    }
                    state.set_termios(Termios::from_bytes(&bytes));
                }
                // Readers re-read the settings; output is never queued, so `W` waits for
                // nothing.
                self.readable.wake_all();
                self.room.wake_all();
                Ok(0)
            }
            TIOCGWINSZ => {
                let window = self.state.lock().window;
                write_user(argument, &window.to_bytes())?;
                Ok(0)
            }
            TIOCSWINSZ => {
                let mut bytes = [0; 8];
                read_user(&mut bytes, argument)?;
                self.resize(WindowSize::from_bytes(&bytes));
                Ok(0)
            }
            TIOCGPGRP => {
                self.check_controlling()?;
                let group = self.foreground().ok_or(ENOTTY)?;
                write_user(argument, &group.to_le_bytes())?;
                Ok(0)
            }
            TIOCSPGRP => self.set_foreground(argument),
            TIOCGSID => {
                self.check_controlling()?;
                let session = self.session().ok_or(ENOTTY)?;
                write_user(argument, &session.to_le_bytes())?;
                Ok(0)
            }
            TIOCSCTTY => self.make_controlling(argument),
            TIOCNOTTY => {
                let process = process::current().ok_or(ENOTTY)?;
                self.check_controlling()?;
                if process.session() == process.id {
                    self.disassociate(false);
                }
                *process.controlling_tty.lock() = None;
                Ok(0)
            }
            FIONREAD => {
                let available = self.state.lock().available() as u32;
                write_user(argument, &available.to_le_bytes())?;
                Ok(0)
            }
            TIOCOUTQ => {
                // Output is never queued in the terminal.
                write_user(argument, &0u32.to_le_bytes())?;
                Ok(0)
            }
            TCFLSH => {
                const TCIFLUSH: u64 = 0;
                const TCOFLUSH: u64 = 1;
                const TCIOFLUSH: u64 = 2;
                self.job_check(SIGTTOU)?;
                match argument {
                    TCIFLUSH | TCIOFLUSH => self.state.lock().flush_input(),
                    TCOFLUSH => {}
                    _ => return Err(EINVAL),
                }
                self.room.wake_all();
                Ok(0)
            }
            TCXONC => {
                const TCOOFF: u64 = 0;
                const TCOON: u64 = 1;
                const TCIOFF: u64 = 2;
                const TCION: u64 = 3;
                self.job_check(SIGTTOU)?;
                match argument {
                    TCOOFF => self.state.lock().output_stopped = true,
                    TCOON => {
                        self.state.lock().output_stopped = false;
                        self.output_started.wake_all();
                    }
                    // Asking the other side to stop sending: no flow control here.
                    TCIOFF | TCION => {}
                    _ => return Err(EINVAL),
                }
                Ok(0)
            }
            // A break on a virtual line is nothing; output is never queued.
            TCSBRK | TCSBRKP => Ok(0),
            // Pushing input as if typed: never (docs/security.md).
            TIOCSTI => Err(EIO),
            TIOCEXCL | TIOCNXCL => {
                self.state.lock().exclusive = request == TIOCEXCL;
                Ok(0)
            }
            TIOCGDEV => {
                let (major, minor) = file::major_minor(self.device);
                // Linux's `new_encode_dev`, a 32-bit number.
                let encoded = (minor & 0xff) | (major << 8) | ((minor & !0xff) << 12);
                write_user(argument, &(encoded as u32).to_le_bytes())?;
                Ok(0)
            }
            _ => {
                debug!("tty {}: ioctl {request:#x} is not implemented", self.name);
                Err(ENOTTY)
            }
        }
    }

    /// A new window size; the foreground group gets `SIGWINCH` if it changed.
    fn resize(&self, window: WindowSize) {
        let foreground = {
            let mut state = self.state.lock();
            let changed = state.window != window;
            state.window = window;
            changed.then_some(state.foreground).flatten()
        };
        if let Some(group) = foreground {
            signal::send_to_group(group, SIGWINCH, signal::from_kernel());
        }
    }

    /// `ENOTTY` unless this is the caller's controlling terminal.
    fn check_controlling(self: &Arc<Self>) -> Result<(), Errno> {
        let process = process::current().ok_or(ENOTTY)?;
        match process::controlling_tty(&process) {
            Some(tty) if Arc::ptr_eq(&tty, self) => Ok(()),
            _ => Err(ENOTTY),
        }
    }

    /// `TIOCSPGRP`: a group of the caller's session becomes the foreground.
    fn set_foreground(self: &Arc<Self>, argument: u64) -> Result<u64, Errno> {
        self.check_controlling()?;
        match self.job_check(SIGTTOU) {
            // Linux answers an orphaned background group this way.
            Err(EIO) => return Err(ENOTTY),
            result => result?,
        }
        let mut bytes = [0; 4];
        read_user(&mut bytes, argument)?;
        let group = i32::from_le_bytes(bytes);
        if group < 0 {
            return Err(EINVAL);
        }
        let process = process::current().ok_or(ENOTTY)?;
        match process::group_in_session(group as u32, process.session()) {
            None => Err(ESRCH),
            Some(false) => Err(EPERM),
            Some(true) => {
                self.state.lock().foreground = Some(group as u32);
                Ok(0)
            }
        }
    }

    /// `TIOCSCTTY`: the caller, a session leader without a controlling terminal, takes this
    /// one. A terminal another session controls is taken only by root with `argument` 1.
    fn make_controlling(self: &Arc<Self>, argument: u64) -> Result<u64, Errno> {
        let process = process::current().ok_or(EPERM)?;
        let mut state = self.state.lock();
        if process.session() == process.id && state.session == Some(process.session()) {
            return Ok(0);
        }
        if process.session() != process.id || process::controlling_tty(&process).is_some() {
            return Err(EPERM);
        }
        if state.session.is_some() {
            let root = process.credentials.lock().is_root();
            if argument != 1 || !root {
                return Err(EPERM);
            }
        }
        self.become_controlling(&mut state, &process);
        Ok(0)
    }

    /// An open file of this terminal.
    pub fn file(self: &Arc<Self>) -> Arc<TtyFile> {
        self.opens.fetch_add(1, Ordering::Relaxed);
        self.ever_opened.store(true, Ordering::Relaxed);
        Arc::new(TtyFile { tty: self.clone() })
    }

    /// Whether files of this terminal were open and all are closed now.
    fn all_closed(&self) -> bool {
        self.ever_opened.load(Ordering::Relaxed) && self.opens.load(Ordering::Relaxed) == 0
    }
}

/// A canonical read: at most one line.
fn read_line(state: &mut State, buffer: &mut [u8]) -> Option<usize> {
    let line = state.lines.front_mut()?;
    let count = buffer.len().min(line.len());
    buffer[..count].copy_from_slice(&line[..count]);
    line.drain(..count);
    if line.is_empty() {
        state.lines.pop_front();
    }
    Some(count)
}

/// A raw read, as `VMIN` and `VTIME` say (`man 3 termios`): both 0 return at once; MIN
/// alone waits for that many bytes; TIME alone waits that long (tenths of a second) for any;
/// both wait for MIN bytes or TIME after the last byte.
fn read_raw(state: &mut State, buffer: &mut [u8], started: u64) -> Option<usize> {
    const TENTH: u64 = time::NANOSECONDS_PER_SECOND / 10;
    let minimum = usize::from(state.termios.cc[VMIN]).min(buffer.len());
    let timeout = u64::from(state.termios.cc[VTIME]) * TENTH;
    let available = state.raw.len();
    let now = time::monotonic();
    let ready = match (minimum, timeout) {
        (0, 0) => true,
        (0, timeout) => available > 0 || now >= started + timeout,
        (minimum, 0) => available >= minimum,
        (minimum, timeout) => {
            available >= minimum || (available > 0 && now >= state.last_input + timeout)
        }
    };
    if !ready {
        return None;
    }
    let count = buffer.len().min(available);
    for (slot, byte) in buffer.iter_mut().zip(state.raw.drain(..count)) {
        *slot = byte;
    }
    Some(count)
}

/// An open terminal (not a pseudo-terminal's master).
pub struct TtyFile {
    tty: Arc<Tty>,
}

impl File for TtyFile {
    fn read(&self, buffer: &mut [u8], nonblocking: bool) -> Result<usize, Errno> {
        self.tty.read(buffer, nonblocking)
    }

    fn write(&self, data: &[u8], nonblocking: bool) -> Result<usize, Errno> {
        self.tty.write(data, nonblocking)
    }

    fn access_mode(&self) -> u32 {
        flags::O_RDWR
    }

    fn stat(&self) -> Metadata {
        file::anonymous_metadata(
            crate::vfs::mode::CHARACTER_DEVICE | 0o600,
            self.tty.device,
            0,
            0,
        )
    }

    fn ioctl(&self, request: u64, argument: u64) -> Result<u64, Errno> {
        self.tty.ioctl(request, argument)
    }

    fn poll(&self) -> u32 {
        self.tty.poll()
    }

    fn wait_queues<'a>(&'a self, queues: &mut Vec<&'a WaitQueue>) {
        self.tty.wait_queues(queues);
    }
}

impl Drop for TtyFile {
    fn drop(&mut self) {
        self.tty.opens.fetch_sub(1, Ordering::Relaxed);
        // A pseudo-terminal's master may be waiting to see the slave closed.
        self.tty.room.wake_all();
        pty::slave_closed(&self.tty);
    }
}

/// Opens a terminal device by number: `/dev/tty` (5:0), `/dev/console` (5:1), `/dev/ptmx`
/// (5:2), `/dev/ttyS0` (4:64) and `/dev/pts/<n>` (136:n).
pub fn open_device(major: u64, minor: u64, open_flags: u32) -> Result<Arc<dyn File>, Errno> {
    match (major, minor) {
        (5, 0) => {
            let process = process::current().ok_or(ENXIO)?;
            let tty = process::controlling_tty(&process).ok_or(ENXIO)?;
            Ok(tty.file())
        }
        // `/dev/console` never becomes a controlling terminal by `open` (as in Linux; the
        // shell takes it with `TIOCSCTTY`).
        (5, 1) => {
            let tty = serial::console();
            tty.opened(open_flags, false)?;
            Ok(tty.file())
        }
        (4, 64) => {
            let tty = serial::console();
            tty.opened(open_flags, true)?;
            Ok(tty.file())
        }
        (5, 2) => pty::open_master(),
        (136, index) => {
            let tty = pty::slave(index)?;
            tty.opened(open_flags, true)?;
            Ok(tty.file())
        }
        _ => Err(ENXIO),
    }
}

/// The console as an open file, for init's descriptors 0, 1 and 2 (and the self-tests).
pub fn console_file() -> Arc<dyn File> {
    serial::console().file()
}

/// Sets up the serial console. Call once at boot.
pub fn init() {
    serial::init();
}
