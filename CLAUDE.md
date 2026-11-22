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

**Stage 1.5 — SMP.** See `docs/roadmap.md`. (Stage 0 done: CI green on `a97b643`; stage 1
done: tag `stage-1`.)
Do not start work that belongs to a later stage unless the owner asks for it.

## Working mode (from stage 1)

Work through the steps of a stage without stopping after each; the owner reads the code as
it lands. Stage 1 pitstops: P1 after steps 1–2, P2 after 3–5, P3 after 6–7, P4 after 8–9
(end of the stage, tag `stage-1`).

At a pitstop: `cargo fmt`, `cargo clippy` and `cargo xtask test` green; a short report (what
was done, what differs from the plan, open questions); a journal entry; everything staged;
then commit and push following rule 9.

Between pitstops, stop right away only when:
- a design decision is needed that the docs do not cover,
- a new dependency is needed,
- a test fails and you cannot fix it,
- a change touches the rules, licences or security.

## Repository layout

```
kernel/     the kernel crate (no_std, target x86_64-unknown-none), safe Rust only
frame/      ochra-frame: boot entry, arch code, drivers' register access — all low-level unsafe
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
cargo xtask check-unsafe  # fail if `unsafe` appears in code outside frame/
```

The owner's machine is Garuda Linux (Arch-based). OVMF lives in `/usr/share/edk2/x64/`.

## Hard rules

1. **One stage at a time.** A stage is done only when its "Done when" check passes in CI.
2. **Linux ABI is the spec.** Syscall numbers and behaviour follow Linux; check the man pages,
   do not guess. Every implemented syscall gets a small test binary.
3. **`unsafe` is confined.** Low-level `unsafe` lives only in `ochra-frame` (`frame/`); every
   other crate is safe Rust (`cargo xtask check-unsafe` enforces it in CI). Every `unsafe`
   block has a `// SAFETY:` comment explaining why it is sound.
   - Macros exported from `ochra-frame` must not take arbitrary code blocks and wrap them in
     `unsafe`. Every such macro carries a `// SAFETY:` comment justifying it.
4. **Design for SMP** from the start (no unsynchronised global mutable state), even while only
   one core is used.
5. **Turn on SMEP/SMAP** as soon as paging is up; keep W^X for kernel memory.
6. **No unexplained binary files** in the repository. Build only from source.
7. **Toolchain pinned** in `rust-toolchain.toml`. `cargo fmt` clean, `cargo clippy` with
   `-D warnings` clean.
8. **Licences:** kernel `GPL-2.0-only`; userspace components `MIT OR Apache-2.0`; artwork
   `CC-BY-SA-4.0`. Put an SPDX header at the top of every source file.
9. **Commits at pitstops.** At a pitstop (see "Working mode") you run `git commit -s` with an
   `Assisted-by: Claude:<model-id>` trailer, then `git push`. Each run goes through a
   permission prompt the owner approves, and the signature is confirmed in the SSH agent:
   that approval is the owner's DCO certification.
   - Before committing, show `git status` and a short summary of the diff.
   - If no permission prompt would appear (e.g. a bypass permission mode), ask in the chat and
     wait for an explicit yes before each commit and push.
   - Never add `git commit` or `git push` to the allowed-commands list without asking.
   - Forbidden unless the owner explicitly asks: `--no-verify`, disabling `gpgsign`,
     `--amend`, force push, any other history rewrite.
   - Never write a `Signed-off-by:` line yourself (`-s` adds the owner's), and no
     `Co-Authored-By:` trailer.
   - Message: one `-m` per paragraph, each paragraph on a single line, no backticks, `$` or
     `!` inside the quotes.
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
