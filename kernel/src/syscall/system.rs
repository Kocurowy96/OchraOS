// SPDX-License-Identifier: GPL-2.0-only

//! Syscalls about the system as a whole: its name and the clocks.

use super::{SyscallResult, read_user, write_user};
use crate::errno::{EFAULT, EINTR, EINVAL, ERESTARTSYS, Errno};
use crate::time::{self, NANOSECONDS_PER_SECOND};

/// The Linux release whose syscall interface OchraOS aims at (the "ABI target", see
/// `docs/architecture.md`). Programs and C libraries read it to decide what they may use
/// (glibc refuses kernels older than it was built for), so it is a Linux version, marked as
/// ours by the suffix.
const ABI_TARGET_RELEASE: &str = "6.12.0-ochra";

/// `uname(buf)`: `struct utsname`, six NUL-terminated fields of 65 bytes. The system name is
/// "Linux", as programs expect of a Linux ABI; the version field says what really runs.
/// The host and domain names are "(none)", as in Linux until something sets them.
pub fn uname(buffer: u64) -> SyscallResult {
    const FIELD: usize = 65;
    let version = concat!("OchraOS ", env!("CARGO_PKG_VERSION"));
    let fields = [
        "Linux",
        "(none)",
        ABI_TARGET_RELEASE,
        version,
        "x86_64",
        "(none)",
    ];
    let mut bytes = [0; FIELD * 6];
    for (index, field) in fields.iter().enumerate() {
        // Every field is far shorter than 64 bytes, so a NUL always follows it.
        bytes[index * FIELD..][..field.len()].copy_from_slice(field.as_bytes());
    }
    write_user(buffer, &bytes)?;
    Ok(0)
}

/// Clock IDs (`include/uapi/linux/time.h`).
mod clock {
    pub const REALTIME: u64 = 0;
    pub const MONOTONIC: u64 = 1;
    pub const MONOTONIC_RAW: u64 = 4;
    pub const REALTIME_COARSE: u64 = 5;
    pub const MONOTONIC_COARSE: u64 = 6;
    pub const BOOTTIME: u64 = 7;
}

/// The time of `clock` in nanoseconds. The CPU-time clocks are not there yet: there is no
/// time accounting per process.
fn read_clock(id: u64) -> Result<u64, Errno> {
    match id {
        clock::REALTIME | clock::REALTIME_COARSE => Ok(time::realtime()),
        // Nothing suspends the machine yet, so boot time is monotonic time.
        clock::MONOTONIC | clock::MONOTONIC_RAW | clock::MONOTONIC_COARSE | clock::BOOTTIME => {
            Ok(time::monotonic())
        }
        _ => Err(EINVAL),
    }
}

/// A `struct timespec` (seconds, nanoseconds).
fn timespec(nanoseconds: u64) -> [u8; 16] {
    let mut bytes = [0; 16];
    bytes[..8].copy_from_slice(&(nanoseconds / NANOSECONDS_PER_SECOND).to_le_bytes());
    bytes[8..].copy_from_slice(&(nanoseconds % NANOSECONDS_PER_SECOND).to_le_bytes());
    bytes
}

/// Reads a `struct timespec` as nanoseconds; `EINVAL` for a negative time or nanoseconds out
/// of range.
fn read_timespec(address: u64) -> Result<u64, Errno> {
    let mut bytes = [0; 16];
    read_user(&mut bytes, address)?;
    let seconds = i64::from_le_bytes(bytes[..8].try_into().expect("8 bytes"));
    let nanoseconds = i64::from_le_bytes(bytes[8..].try_into().expect("8 bytes"));
    if seconds < 0 || !(0..NANOSECONDS_PER_SECOND as i64).contains(&nanoseconds) {
        return Err(EINVAL);
    }
    Ok((seconds as u64)
        .saturating_mul(NANOSECONDS_PER_SECOND)
        .saturating_add(nanoseconds as u64))
}

/// `clock_gettime(clockid, tp)`.
pub fn clock_gettime(id: u64, address: u64) -> SyscallResult {
    let now = read_clock(id)?;
    write_user(address, &timespec(now))?;
    Ok(0)
}

/// `clock_getres(clockid, res)`: 1 ns with the TSC, one tick otherwise; a tick for the
/// coarse clocks, as Linux.
pub fn clock_getres(id: u64, address: u64) -> SyscallResult {
    read_clock(id)?;
    let tick = NANOSECONDS_PER_SECOND / u64::from(time::HZ);
    let coarse = matches!(id, clock::REALTIME_COARSE | clock::MONOTONIC_COARSE);
    let resolution = if time::precise() && !coarse { 1 } else { tick };
    if address != 0 {
        write_user(address, &timespec(resolution))?;
    }
    Ok(0)
}

/// `gettimeofday(tv, tz)`: seconds and microseconds; the time zone is always UTC (zeros).
pub fn gettimeofday(address: u64, zone: u64) -> SyscallResult {
    let now = time::realtime();
    if address != 0 {
        let mut bytes = [0; 16];
        bytes[..8].copy_from_slice(&(now / NANOSECONDS_PER_SECOND).to_le_bytes());
        bytes[8..].copy_from_slice(&(now % NANOSECONDS_PER_SECOND / 1000).to_le_bytes());
        write_user(address, &bytes)?;
    }
    if zone != 0 {
        write_user(zone, &[0; 8])?;
    }
    Ok(0)
}

/// `time(tloc)`.
pub fn time(address: u64) -> SyscallResult {
    let seconds = time::realtime() / NANOSECONDS_PER_SECOND;
    if address != 0 {
        write_user(address, &seconds.to_le_bytes())?;
    }
    Ok(seconds)
}

/// `nanosleep(req, rem)`: `clock_nanosleep` on the monotonic clock, relative.
pub fn nanosleep(request: u64, remaining: u64) -> SyscallResult {
    clock_nanosleep(clock::MONOTONIC, 0, request, remaining)
}

/// `clock_nanosleep(clockid, flags, request, remain)`. A signal ends the sleep with `EINTR`
/// and, for a relative sleep, the time left in `remain`. (Linux restarts the sleep for the
/// rest of the time when no handler runs; here every signal that interrupts a sleep has a
/// handler or ends the process, since ignored signals are dropped when sent.)
pub fn clock_nanosleep(id: u64, flags: u64, request: u64, remaining: u64) -> SyscallResult {
    const TIMER_ABSTIME: u64 = 1;
    let start = read_clock(id)?;
    let duration = read_timespec(request)?;
    let absolute = flags & TIMER_ABSTIME != 0;
    // The deadline on the monotonic clock, which the sleep uses.
    let wait = if absolute {
        duration.saturating_sub(start)
    } else {
        duration
    };
    let deadline = time::monotonic().saturating_add(wait);
    match time::sleep_until(deadline) {
        Ok(()) => Ok(0),
        Err(ERESTARTSYS) => {
            if !absolute && remaining != 0 {
                let left = deadline.saturating_sub(time::monotonic());
                write_user(remaining, &timespec(left)).map_err(|_| EFAULT)?;
            }
            Err(EINTR)
        }
        Err(error) => Err(error),
    }
}
