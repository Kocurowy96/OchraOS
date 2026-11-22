// SPDX-License-Identifier: GPL-2.0-only

//! Driver for the 16550 UART, the classic PC serial port.
//!
//! A UART sends bytes one bit at a time over a wire. Every PC-compatible machine (and QEMU)
//! has one at I/O port 0x3F8, called COM1. It works before anything else in the kernel does
//! — no memory allocation, no interrupts, no graphics — which makes it the ideal first output
//! for a kernel. QEMU forwards it to the terminal (`-serial stdio`) or a file.

use core::fmt;

use crate::sync::IrqMutex;

use crate::arch::port::{inb, outb};

/// I/O port base address of COM1.
const COM1_BASE: u16 = 0x3F8;

/// The shared COM1 port. The lock makes sure two CPUs (later) never mix their bytes in the
/// middle of a line.
pub static COM1: IrqMutex<SerialPort> = IrqMutex::new(
    // SAFETY: COM1 is the standard PC serial port and this is the only regular owner of it;
    // the panic handler's emergency writer is documented in `SerialPort::new`.
    unsafe { SerialPort::new(COM1_BASE) },
);

// Register offsets from the base port. The same offset can mean two registers, depending on
// whether we read or write, and on the DLAB bit in the line control register.
const DATA: u16 = 0; // data in/out; with DLAB = 1: divisor, low byte
const INTERRUPT_ENABLE: u16 = 1; // with DLAB = 1: divisor, high byte
const FIFO_CONTROL: u16 = 2;
const LINE_CONTROL: u16 = 3;
const MODEM_CONTROL: u16 = 4;
const LINE_STATUS: u16 = 5;

/// Line status bit: the transmit holding register is empty, a new byte can be written.
const TRANSMIT_EMPTY: u8 = 1 << 5;

/// One 16550 UART.
pub struct SerialPort {
    base: u16,
}

/// The UART could not be set up.
#[derive(Debug)]
pub struct NoSerialPort;

impl SerialPort {
    /// # Safety
    ///
    /// `base` must be the I/O base of a 16550-compatible UART that nothing else drives.
    /// The one exception is the panic handler, which may create a second writer for COM1:
    /// the worst outcome is mixed-up characters, the machine stops right after.
    pub const unsafe fn new(base: u16) -> Self {
        Self { base }
    }

    /// Configures the port for 115200 baud, 8 data bits, no parity, 1 stop bit ("8N1"),
    /// and checks that a UART is really there.
    pub fn init(&mut self) -> Result<(), NoSerialPort> {
        self.write_reg(INTERRUPT_ENABLE, 0x00); // we poll; no interrupts
        // Baud rate = 115200 / divisor. Setting DLAB exposes the two divisor registers.
        self.write_reg(LINE_CONTROL, 0x80); // DLAB = 1
        self.write_reg(DATA, 1); // divisor low byte: 1 -> 115200 baud
        self.write_reg(INTERRUPT_ENABLE, 0); // divisor high byte
        self.write_reg(LINE_CONTROL, 0x03); // DLAB = 0, 8 bits, no parity, 1 stop bit
        self.write_reg(FIFO_CONTROL, 0xC7); // enable and clear the FIFOs, 14-byte threshold

        // Loopback self-test: in loopback mode the UART receives what it sends. If the byte
        // does not come back, there is no working UART at this address.
        self.write_reg(MODEM_CONTROL, 0x1E); // loopback, RTS, OUT1, OUT2
        self.write_reg(DATA, 0xAE);
        if self.read_reg(DATA) != 0xAE {
            return Err(NoSerialPort);
        }

        self.write_reg(MODEM_CONTROL, 0x0F); // normal operation: DTR, RTS, OUT1, OUT2
        Ok(())
    }

    /// Sends one byte, waiting until the UART can take it.
    pub fn send(&mut self, byte: u8) {
        while self.read_reg(LINE_STATUS) & TRANSMIT_EMPTY == 0 {
            core::hint::spin_loop();
        }
        self.write_reg(DATA, byte);
    }

    fn write_reg(&mut self, offset: u16, value: u8) {
        // SAFETY: `new` guarantees that we own the UART at `base`, and every offset used in
        // this file is one of its registers.
        unsafe { outb(self.base + offset, value) }
    }

    fn read_reg(&mut self, offset: u16) -> u8 {
        // SAFETY: as in `write_reg`.
        unsafe { inb(self.base + offset) }
    }
}

impl fmt::Write for SerialPort {
    fn write_str(&mut self, text: &str) -> fmt::Result {
        for byte in text.bytes() {
            // Terminals expect "\r\n": carriage return (back to column 0), then line feed.
            if byte == b'\n' {
                self.send(b'\r');
            }
            self.send(byte);
        }
        Ok(())
    }
}

/// Writer for the panic handler: it does not take the `COM1` lock, which may be held by the
/// code that panicked (e.g. a panic inside `println!`) and would never be released.
pub fn emergency_writer() -> SerialPort {
    // SAFETY: this is the documented exception in `SerialPort::new`.
    unsafe { SerialPort::new(COM1_BASE) }
}

/// Used by `print!` and `println!`; not meant to be called directly.
#[doc(hidden)]
pub fn _print(args: fmt::Arguments) {
    use fmt::Write;
    // Writing to the UART cannot fail, so the `fmt::Result` carries no information.
    let _ = COM1.lock().write_fmt(args);
}

/// Prints to the serial port, like `std::print!`.
#[macro_export]
macro_rules! print {
    ($($arg:tt)*) => {
        $crate::serial::_print(format_args!($($arg)*))
    };
}

/// Prints to the serial port with a newline, like `std::println!`.
#[macro_export]
macro_rules! println {
    () => {
        $crate::print!("\n")
    };
    ($($arg:tt)*) => {
        $crate::serial::_print(format_args!("{}\n", format_args!($($arg)*)))
    };
}
