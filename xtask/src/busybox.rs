// SPDX-License-Identifier: MIT OR Apache-2.0

//! Builds BusyBox (GPL-2.0): the shell and the small tools of the initramfs, as one static
//! binary against musl. The source comes from BusyBox's Git repository at a pinned tag (see
//! `git`); only the configuration, `userspace/busybox.config`, is ours.
//!
//! The configuration is `make allnoconfig` (everything off), then the options from our file,
//! then `make oldconfig` to settle the dependencies. The result is cached in `build/busybox/`
//! and rebuilt when the configuration file or musl changes.

use std::path::{Path, PathBuf};
use std::process::Command;

use crate::git::Pinned;
use crate::{Result, root, run_logged, step};

const BUSYBOX: Pinned = Pinned {
    name: "BusyBox",
    repository: "https://git.busybox.net/busybox",
    tag: "1_38_0",
    commit: "fc71374dfccd46448c62947269a35f1420d7ee28",
};

/// The BusyBox binary and the applets it has (`busybox --list`).
pub struct Busybox {
    pub binary: PathBuf,
    pub applets: Vec<String>,
}

/// Builds BusyBox if needed, against the musl installed at `musl`.
pub fn build(build_dir: &Path, musl: &Path) -> Result<Busybox> {
    let tag = BUSYBOX.tag;
    let dir = build_dir.join("busybox");
    let source = dir.join(format!("src-{tag}"));
    let objects = dir.join(format!("obj-{tag}"));
    let binary = objects.join("busybox");
    let config = std::fs::read_to_string(root().join("userspace/busybox.config"))?;
    // What the cached binary was built from: our configuration and the musl it links.
    let built_with = dir.join("built-with");
    let inputs = format!("{config}\n# musl: {}\n", musl.display());

    let up_to_date = binary.exists()
        && std::fs::read_to_string(&built_with).is_ok_and(|previous| previous == inputs);
    if !up_to_date {
        BUSYBOX.fetch(&source)?;
        step(&format!("building BusyBox {tag}"));
        if objects.exists() {
            std::fs::remove_dir_all(&objects)?;
        }
        std::fs::create_dir_all(&objects)?;
        let compiler = write_compiler_wrapper(&dir, musl)?;
        let log = dir.join("build.log");
        let make = |target: Option<&str>| {
            let mut command = Command::new("make");
            command
                .env("KCONFIG_NOTIMESTAMP", "1")
                .arg("-C")
                .arg(&source)
                .arg(format!("O={}", objects.display()));
            if let Some(target) = target {
                command.arg(target);
            }
            command
        };
        run_logged(&mut make(Some("allnoconfig")), &log)?;
        merge_config(&objects.join(".config"), &config)?;
        // `oldconfig` asks about options that became visible; an empty answer takes the
        // default (it gives up at the end of its input, so give it plenty).
        let answers = dir.join("oldconfig-answers");
        std::fs::write(&answers, "\n".repeat(10_000))?;
        let mut oldconfig = make(Some("oldconfig"));
        oldconfig.stdin(std::fs::File::open(&answers)?);
        run_logged(&mut oldconfig, &log)?;
        check_config(&objects.join(".config"), &config)?;
        let jobs = std::thread::available_parallelism().map_or(1, |n| n.get());
        let mut build = Command::new("make");
        build
            .current_dir(&objects)
            // No build time in the binary (`busybox --help`, the shell's banner), so the
            // same source always gives the same binary.
            .env("KCONFIG_NOTIMESTAMP", "1")
            .arg(format!("-j{jobs}"))
            .arg(format!("CC={}", compiler.display()))
            .arg("HOSTCC=gcc");
        run_logged(&mut build, &log)?;
        std::fs::write(&built_with, &inputs)?;
    }

    // A static Linux binary, so it runs on the (Linux) build host too.
    let output = Command::new(&binary).arg("--list").output()?;
    if !output.status.success() {
        return Err("busybox --list failed".into());
    }
    let applets = String::from_utf8(output.stdout)?
        .lines()
        .map(str::to_string)
        .collect();
    Ok(Busybox { binary, applets })
}

/// The `CONFIG_…=…` lines of our configuration file.
fn settings(config: &str) -> impl Iterator<Item = (&str, &str)> {
    config
        .lines()
        .filter(|line| line.starts_with("CONFIG_"))
        .filter_map(|line| line.split_once('='))
}

/// Replaces, in the `allnoconfig` result, every option our file sets.
fn merge_config(dot_config: &Path, config: &str) -> Result<()> {
    let ours: Vec<(&str, &str)> = settings(config).collect();
    let is_ours = |line: &str| {
        ours.iter().any(|(name, _)| {
            line.strip_prefix(name)
                .is_some_and(|rest| rest.starts_with('='))
                || line == format!("# {name} is not set")
        })
    };
    let mut merged: String = std::fs::read_to_string(dot_config)?
        .lines()
        .filter(|line| !is_ours(line))
        .map(|line| format!("{line}\n"))
        .collect();
    for (name, value) in ours {
        merged.push_str(&format!("{name}={value}\n"));
    }
    std::fs::write(dot_config, merged)?;
    Ok(())
}

/// Fails if `oldconfig` dropped one of our options (a missing dependency, or a typo).
fn check_config(dot_config: &Path, config: &str) -> Result<()> {
    let result = std::fs::read_to_string(dot_config)?;
    for (name, value) in settings(config) {
        if !result.lines().any(|line| line == format!("{name}={value}")) {
            return Err(format!("BusyBox option {name}={value} did not stay set").into());
        }
    }
    Ok(())
}

/// Writes `cc`, a script that runs the host GCC against musl only (as `userspace` does for
/// our programs): musl's headers when compiling; when linking a program, musl's start files
/// and `libc.a` and GCC's `libgcc.a`. BusyBox's build calls it as its compiler.
fn write_compiler_wrapper(dir: &Path, musl: &Path) -> Result<PathBuf> {
    let gcc_include = crate::userspace::gcc_output(&["-print-file-name=include"])?;
    let libgcc = crate::userspace::gcc_output(&["-print-libgcc-file-name"])?;
    let include = musl.join("include");
    let lib = musl.join("lib");
    let headers = format!(
        "-nostdinc -isystem '{}' -isystem '{gcc_include}' -fno-pie",
        include.display()
    );
    let script = format!(
        r#"#!/bin/sh
# Generated by `cargo xtask` (xtask/src/busybox.rs): GCC against musl only.
for argument in "$@"; do
    case "$argument" in
        # Compiling, preprocessing or a partial link: headers only.
        -c|-E|-S|-M|-MM|-r) exec gcc {headers} "$@" ;;
    esac
done
exec gcc {headers} -nostdlib -static -no-pie -L'{lib}' '{lib}/crt1.o' '{lib}/crti.o' "$@" \
    '{lib}/libc.a' '{libgcc}' '{lib}/crtn.o'
"#,
        lib = lib.display(),
    );
    let path = dir.join("cc");
    std::fs::write(&path, script)?;
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755))?;
    Ok(path)
}
