// SPDX-License-Identifier: GPL-2.0-only

//! Linux error numbers (`include/uapi/asm-generic/errno-base.h` and `errno.h`). A syscall
//! that fails returns `-errno` in RAX; the C library turns that into `errno`.

pub type Errno = i64;

pub const EPERM: Errno = 1;
pub const ENOENT: Errno = 2;
pub const ESRCH: Errno = 3;
pub const E2BIG: Errno = 7;
pub const ENOEXEC: Errno = 8;
pub const EBADF: Errno = 9;
pub const ECHILD: Errno = 10;
pub const EAGAIN: Errno = 11;
pub const ENOMEM: Errno = 12;
pub const EACCES: Errno = 13;
pub const EFAULT: Errno = 14;
pub const EEXIST: Errno = 17;
pub const EINVAL: Errno = 22;
pub const EMFILE: Errno = 24;
pub const ENOTTY: Errno = 25;
pub const EPIPE: Errno = 32;
pub const ENAMETOOLONG: Errno = 36;
pub const ENOSYS: Errno = 38;
