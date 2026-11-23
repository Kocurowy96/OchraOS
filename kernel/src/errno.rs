// SPDX-License-Identifier: GPL-2.0-only

//! Linux error numbers (`include/uapi/asm-generic/errno-base.h` and `errno.h`). A syscall
//! that fails returns `-errno` in RAX; the C library turns that into `errno`.

pub type Errno = i64;

pub const EPERM: Errno = 1;
pub const ENOENT: Errno = 2;
pub const ESRCH: Errno = 3;
pub const EINTR: Errno = 4;
pub const ENXIO: Errno = 6;
pub const E2BIG: Errno = 7;
pub const ENOEXEC: Errno = 8;
pub const EBADF: Errno = 9;
pub const ECHILD: Errno = 10;
pub const EAGAIN: Errno = 11;
pub const ENOMEM: Errno = 12;
pub const EACCES: Errno = 13;
pub const EFAULT: Errno = 14;
pub const EEXIST: Errno = 17;
pub const ENOTDIR: Errno = 20;
pub const EISDIR: Errno = 21;
pub const EINVAL: Errno = 22;
pub const EMFILE: Errno = 24;
pub const ENOTTY: Errno = 25;
pub const ESPIPE: Errno = 29;
pub const EROFS: Errno = 30;
pub const EPIPE: Errno = 32;
pub const ERANGE: Errno = 34;
pub const ENAMETOOLONG: Errno = 36;
pub const ENOSYS: Errno = 38;
pub const ELOOP: Errno = 40;

// Kernel-internal codes (`include/linux/errno.h`): a syscall interrupted by a signal returns
// one of them, and the signal code decides, before going back to the program, whether the
// syscall runs again or fails with `EINTR`. A program never sees them.

/// Restart if the handler has `SA_RESTART` (or there is no handler); else `EINTR`.
pub const ERESTARTSYS: Errno = 512;
/// Always restart.
pub const ERESTARTNOINTR: Errno = 513;
/// Restart only if no handler runs; else `EINTR` (`pause`, `rt_sigsuspend`).
pub const ERESTARTNOHAND: Errno = 514;
