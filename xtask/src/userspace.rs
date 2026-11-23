// SPDX-License-Identifier: MIT OR Apache-2.0

//! Builds the user programs in `userspace/` against musl and packs them into the initramfs.
//!
//! Every `.c` file is one static program. By default it is a static PIE (position-independent,
//! so the kernel can load it at a random address); files named `*_nopie.c` are built as a
//! classic static executable at a fixed address, so the kernel's loader is tested with both
//! kinds. Files named `*_nostart.c` have no C library at all: they bring their own `_start`,
//! to see exactly what the kernel hands a new program.
//!
//! Layout of the initramfs:
//!
//! ```text
//! /init           userspace/init.sh (the first program the kernel starts: a shell script)
//! /dev/null       device 1:3, as in Linux; /dev/console, 5:1
//! /bin/busybox    BusyBox (see `busybox`), with a symbolic link per applet: /bin/sh, /bin/cat…
//! /bin/<name>     userspace/<name>.c
//! /tests/<name>   userspace/tests/<name>.c (syscall tests, compared with the host Linux)
//! ```

use std::path::{Path, PathBuf};
use std::process::Command;

use crate::cpio::Archive;
use crate::{Result, build_dir, busybox, musl, root, run, step};

/// A compiled program.
pub struct Program {
    /// Path inside the initramfs, e.g. `/tests/write`.
    pub path: String,
    /// The ELF file on the host.
    pub file: PathBuf,
}

/// Builds musl and every program, writes `build/initramfs.cpio`, and returns its path and
/// the programs.
pub fn build() -> Result<(PathBuf, Vec<Program>)> {
    let musl = musl::build(&build_dir()?)?;
    step("building the user programs");
    let out = build_dir()?.join("userspace");
    std::fs::create_dir_all(&out)?;

    let mut programs = Vec::new();
    for (dir, prefix) in [("userspace", "/bin"), ("userspace/tests", "/tests")] {
        for source in c_files(&root().join(dir))? {
            let name = source
                .file_stem()
                .and_then(|stem| stem.to_str())
                .ok_or("bad file name")?
                .to_string();
            let file = out.join(format!("{}-{name}", &prefix[1..]));
            let kind = if name.ends_with("_nopie") {
                Kind::Fixed
            } else if name.ends_with("_nostart") {
                Kind::NoLibrary
            } else {
                Kind::PositionIndependent
            };
            compile(&musl, &source, &file, kind)?;
            let path = format!("{prefix}/{name}");
            programs.push(Program { path, file });
        }
    }
    let busybox = busybox::build(&build_dir()?, &musl)?;

    let mut archive = Archive::default();
    archive.directory("bin");
    archive.directory("dev");
    archive.directory("tests");
    archive.character_device("dev/null", 1, 3);
    archive.character_device("dev/console", 5, 1);
    archive.file(
        "init",
        0o755,
        &std::fs::read(root().join("userspace/init.sh"))?,
    );
    archive.file("bin/busybox", 0o755, &std::fs::read(&busybox.binary)?);
    for applet in &busybox.applets {
        if programs
            .iter()
            .any(|program| program.path[1..] == format!("bin/{applet}"))
        {
            return Err(format!("/bin/{applet} is both a program and a BusyBox applet").into());
        }
        archive.symlink(&format!("bin/{applet}"), "busybox");
    }
    for program in &programs {
        let data = std::fs::read(&program.file)?;
        archive.file(&program.path[1..], 0o755, &data);
    }
    let initramfs = build_dir()?.join("initramfs.cpio");
    std::fs::write(&initramfs, archive.finish())?;
    Ok((initramfs, programs))
}

/// The `.c` files in `dir`, sorted (so the archive is always the same).
fn c_files(dir: &Path) -> Result<Vec<PathBuf>> {
    let mut files: Vec<PathBuf> = std::fs::read_dir(dir)?
        .map(|entry| entry.map(|entry| entry.path()))
        .collect::<std::io::Result<_>>()?;
    files.retain(|path| path.extension().is_some_and(|extension| extension == "c"));
    files.sort();
    Ok(files)
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Kind {
    PositionIndependent,
    Fixed,
    NoLibrary,
}

/// Compiles and links one program with the host GCC against musl only (no host headers or
/// libraries): musl's start files, `libc.a`, and GCC's own `libgcc.a` for compiler helpers.
fn compile(musl: &Path, source: &Path, output: &Path, kind: Kind) -> Result<()> {
    let pie = kind == Kind::PositionIndependent;
    let gcc_include = gcc_output(&["-print-file-name=include"])?;
    let libgcc = gcc_output(&["-print-libgcc-file-name"])?;
    let object = output.with_extension("o");
    let lib = musl.join("lib");

    run(Command::new("gcc")
        .args([
            "-std=c11",
            "-O2",
            "-Wall",
            "-Wextra",
            "-Werror",
            "-nostdinc",
        ])
        .arg(if pie { "-fPIE" } else { "-fno-pie" })
        // Without a C library, the compiler must not call one (`strlen` for a loop, the
        // stack protector's `__stack_chk_fail`).
        .args(if kind == Kind::NoLibrary {
            &["-ffreestanding", "-fno-stack-protector"][..]
        } else {
            &[]
        })
        .arg("-isystem")
        .arg(musl.join("include"))
        .arg("-isystem")
        .arg(&gcc_include)
        .arg("-c")
        .arg(source)
        .arg("-o")
        .arg(&object))?;

    if kind == Kind::NoLibrary {
        // Only the program itself (and GCC's helpers): its own `_start`, no C library.
        return run(Command::new("gcc")
            .args(["-nostdlib", "-static", "-no-pie", "-o"])
            .arg(output)
            .arg(&object)
            .arg(&libgcc));
    }
    let start = if pie { "rcrt1.o" } else { "crt1.o" };
    let mut link = Command::new("gcc");
    link.arg("-nostdlib");
    if pie {
        // A static PIE relocates itself at start (rcrt1.o); no dynamic linker.
        link.args(["-static-pie", "-Wl,--no-dynamic-linker"]);
    } else {
        link.args(["-static", "-no-pie"]);
    }
    run(link
        .arg("-o")
        .arg(output)
        .arg(lib.join(start))
        .arg(lib.join("crti.o"))
        .arg(&object)
        .arg(lib.join("libc.a"))
        .arg(&libgcc)
        .arg(lib.join("crtn.o")))
}

pub fn gcc_output(arguments: &[&str]) -> Result<String> {
    let output = Command::new("gcc").args(arguments).output()?;
    if !output.status.success() {
        return Err(format!("gcc {} failed", arguments.join(" ")).into());
    }
    Ok(String::from_utf8(output.stdout)?.trim().to_string())
}
