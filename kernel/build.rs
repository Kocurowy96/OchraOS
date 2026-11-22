// SPDX-License-Identifier: GPL-2.0-only

//! Passes our linker script to the linker.

fn main() {
    let dir = std::env::var("CARGO_MANIFEST_DIR").expect("cargo sets CARGO_MANIFEST_DIR");
    println!("cargo:rustc-link-arg-bins=-T{dir}/linker.ld");
    println!("cargo:rerun-if-changed=linker.ld");
}
