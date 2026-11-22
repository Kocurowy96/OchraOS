// SPDX-License-Identifier: GPL-2.0-only

//! ANSI escape sequences for coloured terminal output.
//!
//! The kernel only sends bytes; the colours are drawn by the terminal on the other end of the
//! serial line. `ESC [ 38 ; 2 ; r ; g ; b m` selects a 24-bit ("true colour") foreground
//! colour, `ESC [ 0 m` resets everything.

/// Ochre `#CC7722`, the secondary brand colour (docs/branding.md).
pub const OCHRE: &str = "\x1b[38;2;204;119;34m";
/// Pine `#047857`, the primary brand colour.
pub const PINE: &str = "\x1b[38;2;4;120;87m";
pub const RED: &str = "\x1b[38;2;220;38;38m";
pub const YELLOW: &str = "\x1b[38;2;250;204;21m";
pub const GREY: &str = "\x1b[38;2;128;128;128m";
pub const BOLD: &str = "\x1b[1m";
pub const RESET: &str = "\x1b[0m";
