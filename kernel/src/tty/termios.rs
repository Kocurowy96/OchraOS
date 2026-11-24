// SPDX-License-Identifier: GPL-2.0-only

//! Terminal settings as Linux keeps them (`include/uapi/asm-generic/termbits.h`,
//! `man 3 termios`): four flag words, the line discipline number and the control
//! characters. `TCGETS` and `TCSETS` move the kernel's `struct termios` (36 bytes), `TCGETS2`
//! and `TCSETS2` the same plus the two speeds (`struct termios2`, 44 bytes).

/// `c_iflag`: input processing.
pub mod input {
    pub const ISTRIP: u32 = 0o40;
    pub const INLCR: u32 = 0o100;
    pub const IGNCR: u32 = 0o200;
    pub const ICRNL: u32 = 0o400;
    /// Ctrl-S stops output, Ctrl-Q starts it again.
    pub const IXON: u32 = 0o2000;
    /// Any character starts stopped output again.
    pub const IXANY: u32 = 0o4000;
}

/// `c_oflag`: output processing.
pub mod output {
    pub const OPOST: u32 = 0o1;
    pub const OLCUC: u32 = 0o2;
    pub const ONLCR: u32 = 0o4;
    pub const OCRNL: u32 = 0o10;
}

/// `c_cflag`: the line (speed, character size) — stored and reported, nothing to set on a
/// virtual line.
pub mod control {
    pub const CBAUD: u32 = 0o10017;
    pub const B38400: u32 = 0o17;
    pub const CS8: u32 = 0o60;
    pub const CREAD: u32 = 0o200;
    pub const HUPCL: u32 = 0o2000;
}

/// `c_lflag`: the line discipline.
pub mod local {
    /// INTR, QUIT and SUSP send signals.
    pub const ISIG: u32 = 0o1;
    /// Canonical mode: input in lines, with editing.
    pub const ICANON: u32 = 0o2;
    pub const ECHO: u32 = 0o10;
    /// ERASE and WERASE erase on the screen.
    pub const ECHOE: u32 = 0o20;
    /// KILL echoes a newline (or erases the line, with ECHOKE).
    pub const ECHOK: u32 = 0o40;
    /// Echo NL even without ECHO.
    pub const ECHONL: u32 = 0o100;
    /// Signals do not flush the input.
    pub const NOFLSH: u32 = 0o200;
    /// Background writes get `SIGTTOU`.
    pub const TOSTOP: u32 = 0o400;
    /// Control characters echo as `^X`.
    pub const ECHOCTL: u32 = 0o1000;
    pub const ECHOKE: u32 = 0o4000;
    /// WERASE, LNEXT, REPRINT, EOL2.
    pub const IEXTEN: u32 = 0o100000;
}

/// Indexes into `c_cc`.
pub mod index {
    pub const VINTR: usize = 0;
    pub const VQUIT: usize = 1;
    pub const VERASE: usize = 2;
    pub const VKILL: usize = 3;
    pub const VEOF: usize = 4;
    pub const VTIME: usize = 5;
    pub const VMIN: usize = 6;
    pub const VSTART: usize = 8;
    pub const VSTOP: usize = 9;
    pub const VSUSP: usize = 10;
    pub const VEOL: usize = 11;
    pub const VREPRINT: usize = 12;
    pub const VWERASE: usize = 14;
    pub const VLNEXT: usize = 15;
    pub const VEOL2: usize = 16;
}

/// The kernel's `NCCS`.
const CONTROL_CHARACTERS: usize = 19;

/// The size of the kernel's `struct termios` and `struct termios2`.
pub const SIZE: usize = 36;
pub const SIZE2: usize = 44;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Termios {
    pub iflag: u32,
    pub oflag: u32,
    pub cflag: u32,
    pub lflag: u32,
    pub line: u8,
    pub cc: [u8; CONTROL_CHARACTERS],
}

/// Linux's `INIT_C_CC`: ^C, ^\, DEL, ^U, ^D, TIME 0, MIN 1, -, ^Q, ^S, ^Z, -, ^R, ^O, ^W,
/// ^V.
const DEFAULT_CC: [u8; CONTROL_CHARACTERS] = [
    0x03, 0x1c, 0x7f, 0x15, 0x04, 0, 1, 0, 0x11, 0x13, 0x1a, 0, 0x12, 0x0f, 0x17, 0x16, 0, 0, 0,
];

impl Termios {
    /// Linux's `tty_std_termios`: a cooked terminal with echo and signals.
    pub const fn standard() -> Self {
        use local::*;
        Self {
            iflag: input::ICRNL | input::IXON,
            oflag: output::OPOST | output::ONLCR,
            cflag: control::B38400 | control::CS8 | control::CREAD | control::HUPCL,
            lflag: ISIG | ICANON | ECHO | ECHOE | ECHOK | ECHOCTL | ECHOKE | IEXTEN,
            line: 0,
            cc: DEFAULT_CC,
        }
    }

    /// A pseudo-terminal's slave side (`pts_driver->init_termios` in `drivers/tty/pty.c`).
    pub const fn pty_slave() -> Self {
        let mut termios = Self::standard();
        termios.cflag = control::B38400 | control::CS8 | control::CREAD;
        termios
    }

    pub fn has_local(&self, flag: u32) -> bool {
        self.lflag & flag != 0
    }

    pub fn has_input(&self, flag: u32) -> bool {
        self.iflag & flag != 0
    }

    /// A control character, if it is set (0 disables one: Linux's `_POSIX_VDISABLE`).
    pub fn character(&self, index: usize) -> Option<u8> {
        Some(self.cc[index]).filter(|&character| character != 0)
    }

    pub fn to_bytes(self) -> [u8; SIZE] {
        let mut bytes = [0; SIZE];
        for (index, flag) in [self.iflag, self.oflag, self.cflag, self.lflag]
            .into_iter()
            .enumerate()
        {
            bytes[index * 4..][..4].copy_from_slice(&flag.to_le_bytes());
        }
        bytes[16] = self.line;
        bytes[17..].copy_from_slice(&self.cc);
        bytes
    }

    pub fn from_bytes(bytes: &[u8]) -> Self {
        let flag = |index: usize| u32::from_le_bytes(bytes[index * 4..][..4].try_into().unwrap());
        let mut cc = [0; CONTROL_CHARACTERS];
        cc.copy_from_slice(&bytes[17..SIZE]);
        Self {
            iflag: flag(0),
            oflag: flag(1),
            cflag: flag(2),
            lflag: flag(3),
            line: bytes[16],
            cc,
        }
    }

    /// `struct termios2`: the same, then the input and output speeds.
    pub fn to_bytes2(self) -> [u8; SIZE2] {
        let mut bytes = [0; SIZE2];
        bytes[..SIZE].copy_from_slice(&self.to_bytes());
        let speed = self.speed();
        bytes[SIZE..][..4].copy_from_slice(&speed.to_le_bytes());
        bytes[SIZE + 4..].copy_from_slice(&speed.to_le_bytes());
        bytes
    }

    /// The line speed the `CBAUD` bits name (only the common ones; others read as 38400).
    fn speed(&self) -> u32 {
        match self.cflag & control::CBAUD {
            0o15 => 9600,
            0o16 => 19200,
            0o10002 => 115_200,
            _ => 38400,
        }
    }
}

/// `struct winsize`: rows, columns, then the size in pixels (unused).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct WindowSize {
    pub rows: u16,
    pub columns: u16,
    pub width: u16,
    pub height: u16,
}

impl WindowSize {
    pub fn to_bytes(self) -> [u8; 8] {
        let mut bytes = [0; 8];
        for (index, value) in [self.rows, self.columns, self.width, self.height]
            .into_iter()
            .enumerate()
        {
            bytes[index * 2..][..2].copy_from_slice(&value.to_le_bytes());
        }
        bytes
    }

    pub fn from_bytes(bytes: &[u8; 8]) -> Self {
        let value = |index: usize| u16::from_le_bytes([bytes[index * 2], bytes[index * 2 + 1]]);
        Self {
            rows: value(0),
            columns: value(1),
            width: value(2),
            height: value(3),
        }
    }
}
