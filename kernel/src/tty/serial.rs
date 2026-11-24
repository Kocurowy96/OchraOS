// SPDX-License-Identifier: GPL-2.0-only

//! The serial port COM1 as a terminal: `/dev/ttyS0`, also `/dev/console`.
//!
//! The UART does not interrupt us (decisions.md): the boot CPU takes what arrived on every
//! timer tick ([`poll`]) and gives it to the line discipline. Output goes straight to the
//! port, byte by byte; the kernel's own log shares the port.

use alloc::string::String;
use alloc::sync::Arc;

use ochra_frame::serial;
use ochra_frame::sync::IrqMutex;

use super::termios::Termios;
use super::{Driver, Tty};
use crate::errno::Errno;
use crate::file::device;

static CONSOLE: IrqMutex<Option<Arc<Tty>>> = IrqMutex::new(None);

struct Serial;

impl Driver for Serial {
    fn write(&self, data: &[u8], _nonblocking: bool) -> Result<(), Errno> {
        self.echo(data);
        Ok(())
    }

    fn echo(&self, data: &[u8]) {
        let mut port = serial::COM1.lock();
        for &byte in data {
            port.send(byte);
        }
    }
}

pub fn init() {
    let tty = Tty::new(
        String::from("ttyS0"),
        device(4, 64),
        Arc::new(Serial),
        Termios::standard(),
    );
    *CONSOLE.lock() = Some(tty);
}

/// The console terminal.
pub fn console() -> Arc<Tty> {
    CONSOLE.lock().clone().expect("tty::init runs at boot")
}

/// Takes what arrived on the serial port. Called on the boot CPU's timer tick.
pub fn poll() {
    let Some(console) = CONSOLE.lock().clone() else {
        return;
    };
    loop {
        // One byte at a time, so the port is not locked against printing for long.
        let Some(byte) = serial::COM1.lock().receive() else {
            break;
        };
        // A full raw buffer drops the byte, as a UART overrun would.
        console.receive(&[byte]);
    }
}
