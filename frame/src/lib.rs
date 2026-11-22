// SPDX-License-Identifier: GPL-2.0-only

//! `ochra-frame`: the part of the OchraOS kernel that is allowed to use `unsafe`.
//!
//! OchraOS is a "framekernel" (the term comes from Asterinas): everything that has to talk to
//! the hardware directly — the boot entry point, CPU tables, port I/O, later page tables and
//! context switches — lives in this one crate and is wrapped in safe APIs. The rest of the
//! kernel (`kernel/`) is `#![forbid(unsafe_code)]`, so a bug there cannot corrupt memory.
//!
//! Rules for this crate (CLAUDE.md, rule 3):
//! - every `unsafe` block has a `// SAFETY:` comment explaining why it is sound;
//! - exported macros never take arbitrary code and wrap it in `unsafe`.
//!
//! How the kernel starts: Limine jumps to [`boot`]'s `kmain`, which sets up the serial port,
//! checks the boot protocol and then calls the kernel's `main`, registered with [`entry!`].

#![no_std]

#[cfg(all(feature = "selftest", not(debug_assertions)))]
compile_error!("the `selftest` feature must never be enabled in a release build");

pub mod arch;
pub mod boot;
pub mod color;
mod panic;
pub mod serial;

pub use boot::BootInfo;

/// Whether kernel addresses may appear in the log and in crash reports.
///
/// Any kernel address gives away the random KASLR offset, so only debug builds show them
/// (docs/decisions.md); later Nightly builds will too, and root will be able to see them.
pub const SHOW_ADDRESSES: bool = cfg!(debug_assertions);
