// SPDX-License-Identifier: GPL-2.0-only

//! The console: the serial port as a terminal, the file behind descriptors 0, 1 and 2 of
//! `/init`.
//!
//! Input works like a Linux terminal in its default ("canonical") mode: typed characters are
//! echoed and collected into a line, which can be edited (Backspace, Ctrl-U) and is handed to
//! readers only when Enter is pressed. Ctrl-D on an empty line is end of file. The UART does
//! not interrupt us (decisions.md): the boot CPU checks it on every timer tick.

use alloc::collections::VecDeque;
use alloc::vec::Vec;

use ochra_frame::sync::IrqMutex;
use ochra_frame::{print, serial};

use crate::errno::{EAGAIN, Errno};
use crate::file::{File, Stat, device, flags, mode};
use crate::sched::WaitQueue;

struct Input {
    /// The line being typed.
    line: Vec<u8>,
    /// Finished lines, waiting to be read.
    ready: VecDeque<u8>,
    /// Ctrl-D presses on an empty line not yet seen by a reader (each is one end of file).
    end_of_file: usize,
}

static INPUT: IrqMutex<Input> = IrqMutex::new(Input {
    line: Vec::new(),
    ready: VecDeque::new(),
    end_of_file: 0,
});
static READERS: WaitQueue = WaitQueue::new();

/// Takes what arrived on the serial port. Called on the boot CPU's timer tick.
pub fn poll() {
    let mut woke = false;
    loop {
        // One byte at a time, so the port is not locked against printing for long.
        let Some(byte) = serial::COM1.lock().receive() else {
            break;
        };
        let mut input = INPUT.lock();
        match byte {
            // Enter: a terminal sends CR; the program sees LF.
            b'\r' | b'\n' => {
                print!("\n");
                input.line.push(b'\n');
                let line = core::mem::take(&mut input.line);
                input.ready.extend(line);
                woke = true;
            }
            // Backspace (DEL from most terminals, BS from some).
            0x7f | 0x08 => {
                if input.line.pop().is_some() {
                    print!("\x08 \x08");
                }
            }
            // Ctrl-U: forget the line.
            0x15 => {
                for _ in 0..input.line.len() {
                    print!("\x08 \x08");
                }
                input.line.clear();
            }
            // Ctrl-D: hand over the line as it is, or end of file if it is empty.
            0x04 => {
                if input.line.is_empty() {
                    input.end_of_file += 1;
                } else {
                    let line = core::mem::take(&mut input.line);
                    input.ready.extend(line);
                }
                woke = true;
            }
            byte => {
                input.line.push(byte);
                if byte.is_ascii_graphic() || byte == b' ' {
                    print!("{}", byte as char);
                }
            }
        }
    }
    if woke {
        READERS.wake_all();
    }
}

/// The console as a file.
pub struct Console;

impl File for Console {
    /// Returns at most one line, as a terminal does.
    fn read(&self, buffer: &mut [u8], nonblocking: bool) -> Result<usize, Errno> {
        if buffer.is_empty() {
            return Ok(0);
        }
        READERS.wait_interruptible(|| {
            let mut input = INPUT.lock();
            if !input.ready.is_empty() {
                let mut count = 0;
                while count < buffer.len() {
                    let Some(byte) = input.ready.pop_front() else {
                        break;
                    };
                    buffer[count] = byte;
                    count += 1;
                    if byte == b'\n' {
                        break;
                    }
                }
                Some(Ok(count))
            } else if input.end_of_file > 0 {
                input.end_of_file -= 1;
                Some(Ok(0))
            } else if nonblocking {
                Some(Err(EAGAIN))
            } else {
                None
            }
        })?
    }

    fn write(&self, data: &[u8], _nonblocking: bool) -> Result<usize, Errno> {
        // The console takes text; invalid UTF-8 shows as replacement characters.
        print!("{}", alloc::string::String::from_utf8_lossy(data));
        Ok(data.len())
    }

    fn access_mode(&self) -> u32 {
        flags::O_RDWR
    }

    fn stat(&self) -> Stat {
        // As Linux's `/dev/console`: character device 5:1.
        Stat {
            mode: mode::CHARACTER_DEVICE | 0o600,
            rdev: device(5, 1),
            ..Stat::default()
        }
    }

    fn is_terminal(&self) -> bool {
        true
    }
}
