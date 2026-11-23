// SPDX-License-Identifier: GPL-2.0-only

//! The RTC (real-time clock) in the CMOS chip of every PC: the battery-backed calendar clock
//! that keeps the date while the machine is off. Read once at boot for the wall clock; the
//! TSC or the timer measures time from then on.
//!
//! Its registers are read through ports 0x70 (register number) and 0x71 (value). The chip
//! updates them once a second; a read during the update can mix old and new values, so the
//! time is read until two reads in a row agree. Values may be BCD (two decimal digits per
//! byte) and the hour may be in 12-hour form; status register B says which.

use super::port::{inb, outb};
use crate::sync::IrqMutex;

const INDEX: u16 = 0x70;
const DATA: u16 = 0x71;
const SECONDS: u8 = 0x00;
const MINUTES: u8 = 0x02;
const HOURS: u8 = 0x04;
const DAY: u8 = 0x07;
const MONTH: u8 = 0x08;
const YEAR: u8 = 0x09;
const STATUS_A: u8 = 0x0A;
const STATUS_B: u8 = 0x0B;
/// Status A bit 7: an update is in progress.
const UPDATING: u8 = 1 << 7;
/// Status B bit 1: 24-hour mode; bit 2: binary (not BCD) values.
const HOURS_24: u8 = 1 << 1;
const BINARY: u8 = 1 << 2;

/// A date and time as the RTC keeps it (UTC on Linux systems and in QEMU).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DateTime {
    pub year: u16,
    pub month: u8,
    pub day: u8,
    pub hour: u8,
    pub minute: u8,
    pub second: u8,
}

/// The ports are one device shared by all CPUs: one reader at a time.
static LOCK: IrqMutex<()> = IrqMutex::new(());

fn read_register(register: u8) -> u8 {
    // SAFETY: ports 0x70/0x71 belong to the CMOS/RTC, which only this module uses (under
    // `LOCK`); selecting a register and reading it changes nothing.
    unsafe {
        outb(INDEX, register);
        inb(DATA)
    }
}

fn read_raw() -> [u8; 6] {
    while read_register(STATUS_A) & UPDATING != 0 {
        core::hint::spin_loop();
    }
    [SECONDS, MINUTES, HOURS, DAY, MONTH, YEAR].map(read_register)
}

/// Reads the date and time.
pub fn read() -> DateTime {
    let _lock = LOCK.lock();
    let mut raw = read_raw();
    loop {
        let again = read_raw();
        if again == raw {
            break;
        }
        raw = again;
    }
    let status = read_register(STATUS_B);
    let decode = |value: u8| {
        if status & BINARY != 0 {
            value
        } else {
            (value >> 4) * 10 + (value & 0x0F)
        }
    };
    let [second, minute, hour, day, month, year] = raw;
    // In 12-hour mode bit 7 of the hour means PM, and 12 o'clock is 12, not 0.
    let pm = status & HOURS_24 == 0 && hour & 0x80 != 0;
    let mut hour = decode(hour & 0x7F);
    if status & HOURS_24 == 0 {
        hour %= 12;
        if pm {
            hour += 12;
        }
    }
    DateTime {
        // The century register is not standard; two digits mean 2000–2099.
        year: 2000 + u16::from(decode(year)),
        month: decode(month),
        day: decode(day),
        hour,
        minute: decode(minute),
        second: decode(second),
    }
}
