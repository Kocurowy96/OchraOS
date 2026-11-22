// SPDX-License-Identifier: GPL-2.0-only

//! Passes the linker script to the linker.
//!
//! The script describes the memory layout that `ochra-frame` relies on (where the Limine
//! requests go), so it lives in `frame/`; but only the final binary is linked, so the
//! argument has to come from this crate's build script.

fn main() {
    let dir = std::env::var("CARGO_MANIFEST_DIR").expect("cargo sets CARGO_MANIFEST_DIR");
    let script = format!("{dir}/../frame/linker.ld");
    println!("cargo:rustc-link-arg-bins=-T{script}");
    println!("cargo:rerun-if-changed={script}");
}
