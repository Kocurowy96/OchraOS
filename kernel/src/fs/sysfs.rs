// SPDX-License-Identifier: GPL-2.0-only

//! `/sys`: what the kernel knows about devices, one value per file. Only the CPU lists
//! are here for now (`/sys/devices/system/cpu/{online,possible,present}`), which C
//! libraries read to count CPUs (`sysconf(_SC_NPROCESSORS_ONLN)`, `nproc`).

use alloc::boxed::Box;
use alloc::format;
use alloc::string::String;

use ochra_frame::sync::IrqMutex;

use super::synthetic::{self, Attributes, Maker};
use crate::time;
use crate::vfs::{InodeRef, anonymous_device};

/// The device number of sysfs, made once.
fn device() -> u64 {
    static DEVICE: IrqMutex<Option<u64>> = IrqMutex::new(None);
    *DEVICE.lock().get_or_insert_with(anonymous_device)
}

fn attributes(inode: u64, permissions: u32) -> Attributes {
    Attributes {
        device: device(),
        inode,
        permissions,
        owner: (0, 0),
    }
}

fn directory(inode: u64, entries: &'static [(&'static str, Maker)]) -> InodeRef {
    synthetic::directory(attributes(inode, 0o755), Box::new(entries))
}

/// A CPU list: `0-3`, or `0` for one CPU (Linux's `%*pbl` format). Every CPU that exists
/// is started, so the three lists are the same.
fn cpu_list(inode: u64) -> InodeRef {
    synthetic::file(
        attributes(inode, 0o444),
        Box::new(|| {
            let last = time::cpus().saturating_sub(1);
            let list = if last == 0 {
                String::from("0\n")
            } else {
                format!("0-{last}\n")
            };
            Ok(list.into_bytes())
        }),
    )
}

const CPU: &[(&str, Maker)] = &[
    ("online", || cpu_list(10)),
    ("possible", || cpu_list(11)),
    ("present", || cpu_list(12)),
];
const SYSTEM: &[(&str, Maker)] = &[("cpu", || directory(4, CPU))];
const DEVICES: &[(&str, Maker)] = &[("system", || directory(3, SYSTEM))];
const ROOT: &[(&str, Maker)] = &[("devices", || directory(2, DEVICES))];

/// The root of sysfs, to mount on `/sys`.
pub fn root() -> InodeRef {
    directory(1, ROOT)
}
