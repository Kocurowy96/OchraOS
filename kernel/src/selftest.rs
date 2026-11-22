// SPDX-License-Identifier: GPL-2.0-only

//! Built-in self-tests, selected with `test=<name>` on the kernel command line.
//!
//! They deliberately crash the kernel in a known way, so `cargo xtask test` can check that the
//! crash is reported correctly. Such switches have no place in a kernel people actually run,
//! so they exist only with the `selftest` feature, and that feature is refused in release
//! builds (here and in `ochra-frame`).
//!
//! | name          | what happens                                     |
//! |---------------|--------------------------------------------------|
//! | `pagefault`   | write to an unmapped address -> page fault        |
//! | `doublefault` | stack pointer to unmapped memory -> double fault |

#[cfg(all(feature = "selftest", not(debug_assertions)))]
compile_error!("the `selftest` feature must never be enabled in a release build");

/// Runs the self-test `name`. Without the `selftest` feature it only logs a warning.
#[cfg(feature = "selftest")]
pub fn run(name: &str) {
    use ochra_frame::arch::exceptions;

    log::info!("running self-test {name:?}");
    match name {
        "pagefault" => exceptions::trigger_page_fault(),
        "doublefault" => exceptions::trigger_double_fault(),
        _ => log::error!("unknown self-test {name:?}"),
    }
}

/// Runs the self-test `name`. Without the `selftest` feature it only logs a warning.
#[cfg(not(feature = "selftest"))]
pub fn run(name: &str) {
    log::warn!("ignoring test={name}: this kernel is built without self-tests");
}
