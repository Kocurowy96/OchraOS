// SPDX-License-Identifier: GPL-2.0-only

//! The PIT (8253/8254 Programmable Interval Timer), used only as a stopwatch.
//!
//! The local APIC timer counts at the CPU's bus or crystal frequency, which differs between
//! machines and is often not reported anywhere. The PIT, on the other hand, has counted at
//! exactly 1.193182 MHz since the IBM PC. So we let the PIT measure 10 ms and count how far
//! the APIC timer got meanwhile ("calibration").
//!
//! Channel 2 is used because its output can be read back through port 0x61 (it once drove
//! the PC speaker, which we keep silent).

use super::port::{inb, outb};

const FREQUENCY_HZ: u32 = 1_193_182;
const CHANNEL_2_DATA: u16 = 0x42;
const COMMAND: u16 = 0x43;
/// Bit 0: channel 2 gate; bit 1: speaker on; bit 5 (read): channel 2 output.
const SPEAKER_CONTROL: u16 = 0x61;

/// Runs `start`, waits `milliseconds` (at most 50) with the PIT, then runs `stop` and returns
/// its result.
pub fn measure<T>(milliseconds: u32, start: impl FnOnce(), stop: impl FnOnce() -> T) -> T {
    assert!(
        milliseconds <= 50,
        "the PIT counter holds at most about 55 ms"
    );
    let count = FREQUENCY_HZ * milliseconds / 1000;
    // SAFETY: ports 0x42/0x43/0x61 belong to the PIT and the speaker control, which nothing
    // else uses. Channel 2 in mode 0 ("interrupt on terminal count") counts down once and
    // raises its output at zero; it raises no interrupt (its output is not wired to the PIC).
    unsafe {
        let control = inb(SPEAKER_CONTROL);
        outb(SPEAKER_CONTROL, (control & !0b10) | 0b01); // gate on, speaker off
        outb(COMMAND, 0b1011_0000); // channel 2, low then high byte, mode 0, binary
        outb(CHANNEL_2_DATA, count as u8);
        outb(CHANNEL_2_DATA, (count >> 8) as u8);
    }
    start();
    // SAFETY: as above; reading port 0x61 has no side effects.
    while unsafe { inb(SPEAKER_CONTROL) } & 0b10_0000 == 0 {
        core::hint::spin_loop();
    }
    stop()
}
