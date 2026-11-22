# OchraOS — working guide for Claude Code

## What this project is

OchraOS is a hobby operating system written from scratch in Rust. Its kernel implements the
Linux syscall ABI, so unmodified Linux binaries run on it natively (no translation layer).
Long term: an Arch-based userspace, an own Wayland compositor (OchraWM, Material 3 Expressive
style), Wine/Proton, and Steam games on an RTX 4060.

It is the successor of OxideOS (an archived C++ hobby OS). Owner: Kocurowy96.

The owner reads code very well but writes it from scratch less fluently, and is learning Rust
through this project. So:
- prefer clear, readable code over clever code,
- comment *why*, not *what*,
- when you introduce a non-obvious concept (paging, APIC, auxv…), explain it briefly in the
  chat (in Polish) and in a short doc comment.

## Current stage

**Stage 0 — Foundation.** See `docs/roadmap.md`.
Do not start work that belongs to a later stage unless the owner asks for it.

## Repository layout

```
kernel/     the kernel crate (no_std, target x86_64-unknown-none)
xtask/      build / run / test helper, used as `cargo xtask <cmd>`
docs/       design documents — read the relevant one before a larger change
journal/    session journal, one file per day (journal/YYYY-MM-DD.md)
```

## Commands

Created during stage 0; keep this list up to date:

```
cargo xtask build    # build the kernel and the bootable image
cargo xtask run      # boot in QEMU (UEFI via OVMF), serial output in the terminal
cargo xtask test     # boot in QEMU headless and check the serial log
```

The owner's machine is Garuda Linux (Arch-based). OVMF lives in `/usr/share/edk2/x64/`.

## Hard rules

1. **One stage at a time.** A stage is done only when its "Done when" check passes in CI.
2. **Linux ABI is the spec.** Syscall numbers and behaviour follow Linux; check the man pages,
   do not guess. Every implemented syscall gets a small test binary.
3. **`unsafe` is confined.** Low-level `unsafe` lives in one dedicated crate once it exists.
   Every `unsafe` block has a `// SAFETY:` comment explaining why it is sound.
4. **Design for SMP** from the start (no unsynchronised global mutable state), even while only
   one core is used.
5. **Turn on SMEP/SMAP** as soon as paging is up; keep W^X for kernel memory.
6. **No unexplained binary files** in the repository. Build only from source.
7. **Toolchain pinned** in `rust-toolchain.toml`. `cargo fmt` clean, `cargo clippy` with
   `-D warnings` clean.
8. **Licences:** kernel `GPL-2.0-only`; userspace components `MIT OR Apache-2.0`; artwork
   `CC-BY-SA-4.0`. Put an SPDX header at the top of every source file.
9. **Commits are made by the owner, not by you.** Stage the changes and propose a commit
   message. The owner reviews and commits with `git commit -s` (DCO sign-off).
   - Never add a `Signed-off-by:` line yourself — only a human can certify the DCO.
   - Do not add a `Co-Authored-By:` trailer. Instead end the proposed message with
     `Assisted-by: Claude:<model-id>`.
10. **Journal.** Before the session ends, add an entry to `journal/YYYY-MM-DD.md`:
    what was done, what is broken, what is next.
11. **Language:** every file in the repository is in English. Talk to the owner in Polish.

## Read more

- `docs/roadmap.md` — stages 0–9 and their "Done when" checks
- `docs/architecture.md` — kernel, filesystem layout, init, desktop, components
- `docs/security.md` — Ochra Guard, USB protection, keys, supply chain
- `docs/releases.md` — channels, versioning, codenames
- `docs/decisions.md` — log of design decisions
- `CONTRIBUTING.md` — DCO, AI-assisted contributions, code style
