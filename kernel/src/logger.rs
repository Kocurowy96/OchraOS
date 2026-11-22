// SPDX-License-Identifier: GPL-2.0-only

//! Kernel log on the serial port, through the `log` crate.
//!
//! `log` is the standard logging facade of the Rust ecosystem: code calls `log::info!(…)`
//! and friends, and one "logger" chosen at start-up decides where the text goes. Here it goes
//! to COM1, one coloured line per message:
//!
//! ```text
//! [INFO ] kernel::boot: message
//! ```

use log::{Level, LevelFilter, Log, Metadata, Record};

use ochra_frame::color::{GREY, OCHRE, PINE, RED, RESET, YELLOW};
use ochra_frame::println;

struct SerialLogger;

static LOGGER: SerialLogger = SerialLogger;

/// Installs the serial logger. Call once, after `serial::init`.
pub fn init() {
    // `set_logger` only fails if a logger is already set, i.e. if `init` is called twice.
    if log::set_logger(&LOGGER).is_ok() {
        // Debug builds show `debug!` too; `trace!` (very detailed, e.g. the whole memory map)
        // stays off until we can switch it on from the kernel command line.
        let level = if cfg!(debug_assertions) {
            LevelFilter::Debug
        } else {
            LevelFilter::Info
        };
        log::set_max_level(level);
    }
}

impl Log for SerialLogger {
    fn enabled(&self, metadata: &Metadata) -> bool {
        metadata.level() <= log::max_level()
    }

    fn log(&self, record: &Record) {
        if !self.enabled(record.metadata()) {
            return;
        }
        let color = match record.level() {
            Level::Error => RED,
            Level::Warn => YELLOW,
            Level::Info => PINE,
            Level::Debug => OCHRE,
            Level::Trace => GREY,
        };
        // `{:<5}` pads the level name to 5 characters, so the messages line up.
        println!(
            "{color}[{:<5}]{RESET} {GREY}{}:{RESET} {}",
            record.level(),
            record.target(),
            record.args()
        );
    }

    fn flush(&self) {}
}
