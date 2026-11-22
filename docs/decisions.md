# Decision log

Short record of what was decided and why. Newest first.

| Date | Decision | Why |
| --- | --- | --- |
| 2026-11-22 | Scheduling split: the mechanism (task stacks with guard pages, context switch, current task) in `ochra_frame::task`, the policy (round-robin run queue, switch on every timer tick) in the safe `kernel` crate | the policy can change without new `unsafe`; the context switch saves only callee-saved registers, the interrupt path saves the rest |
| 2026-11-22 | End-of-interrupt is sent before the interrupt handler runs (interrupts stay off until `iretq`) | a handler may switch tasks and not return for a while; the APIC must not wait for it |
| 2026-11-22 | After boot the kernel only runs tasks: an `idle` task (`sti; hlt` loop) replaces halting; tasks do not exit yet | the boot stack (no guard page) is left for good; task exit comes with processes in stage 2 |
| 2026-11-22 | Interrupts: legacy PIC remapped to 32–47 and masked; local APIC in x2APIC mode, xAPIC (MMIO, uncached mapping in PML4 slot 510) as fallback; timer vector 0x30, spurious 0xFF; all `frame` locks are `IrqMutex` (interrupts off while held) | modern interrupt path only; no same-CPU deadlock between a lock holder and an interrupt handler |
| 2026-11-22 | Timer: LAPIC timer in periodic mode at 100 Hz, calibrated over 10 ms with PIT channel 2 | the PIT has a known frequency on every PC; 10 ms is a reasonable first time slice; TSC-deadline can replace it later |
| 2026-11-22 | Kernel heap: own first-fit free list sorted by address, merging neighbours, 16-byte granularity, in PML4 slot 508, grows on demand; no allocator crate | simple to read and verify; size classes can come later if profiling asks for them |
| 2026-11-22 | Kernel virtual layout: image in slot 511 (KASLR), direct map at Limine's random offset, heap slot 508, kernel stacks slot 509 in 64 KiB slots (guard page below each); checked at boot | fixed, documented regions; the guard pages cost no memory |
| 2026-11-22 | Physical frames are owned `Frame` handles (no Copy, freed on drop), as in Asterinas | safe kernel code cannot double-free or use a frame after freeing it |
| 2026-11-22 | Kernel is a PIE again, `kaslr: yes` in `limine.conf` (Limine also randomises the direct map base); one switch `ochra_frame::SHOW_ADDRESSES` (debug builds) gates every address in the log and crash reports | ends the temporary static build; a single RIP in a breakpoint message or a crash report would reveal the offset just as well as the explicit base |
| 2026-11-22 | Claude runs `git commit -s` and `git push` at pitstops; each run is approved by the owner in a permission prompt (or explicitly in chat when no prompt appears) and the signature in the SSH agent | the owner's approval of that exact commit is the DCO certification; no `--amend`, force push, `--no-verify` or disabled signing unless asked |
| 2026-11-22 | Working mode from stage 1: steps without stopping, pitstops with fmt/clippy/test, report, journal and commit | the owner reads code as it lands; stops only for new design decisions, dependencies, unfixable test failures, rules/licences/security |
| 2026-11-22 | Kernel entry: `ochra-frame` owns `kmain` and calls `__ochra_kernel_main`, defined by its `entry!` macro in the kernel; `cargo xtask check-unsafe` rejects `unsafe` in code of every crate except `frame` | `kernel` can be `forbid(unsafe_code)` although the dependency points the other way; the text check backs up the lint, which skips code from external macros |
| 2026-11-22 | SMP is a separate mini-stage 1.5, after stage 1 "Done when" and before stage 2 | stage 1 stays focused on one core; everything is still designed for SMP |
| 2026-11-22 | `ochra-frame` is licensed GPL-2.0-only, like the rest of the kernel | it is part of the kernel; no plan to offer it to other projects |
| 2026-11-22 | Low-level crate named `ochra-frame`, in `frame/` | "framekernel" (as Asterinas OSTD): all low-level `unsafe` in one crate, the rest of the kernel is safe Rust |
| 2026-11-22 | DCO check in CI has no exception for bots: Dependabot commits are amended with `git commit --amend -s` by the owner before merging | only a human can certify the DCO; one strict rule is easier to trust than a list of exceptions |
| 2026-11-22 | With KASLR, kernel addresses (physical/virtual base, direct map, pointers) appear in the log only in debug/Nightly builds, or are shown only to root (like Linux `kptr_restrict` / `dmesg_restrict`) | a logged address reveals the random offset and defeats KASLR; stage 0 logs them freely until then |
| 2026-11-22 | Kernel linked as a static executable — **temporary**; back to PIE for KASLR (Limine relocates the kernel) at the latest in stage 1, together with paging | simplest start for stage 0; KASLR from `docs/security.md` needs a relocatable kernel |
| 2026-11-22 | Limine built from its pinned source release (SHA-256 checked) by `cargo xtask` | no binaries in the repository; the source tarball is reproducible and verifiable |
| 2026-11-22 | `limine` crate pinned to 0.5.0 (protocol base revision 3) | 0.6 needs nightly (`ptr_metadata`); revision 3 has everything stage 0–1 need |
| 2026-11-22 | Toolchain: pinned Rust stable, not nightly | reproducible builds; naked functions (stable since 1.88) replace the nightly `x86-interrupt` ABI |
| 2026-11-22 | Windows apps: Wine by default, Proton for games, Windows VM as fallback (stage 9) | Wine is light and GPU-friendly; the VM covers what Wine cannot |
| 2026-11-22 | Signing: Ed25519 (minisign style), root, release (YubiKey) and nightly key hierarchy | simple, and survives a lost hardware key |
| 2026-11-22 | Hosting: GitHub Pages and Releases; self-hosting later on a homelab | free and stable; no server needed yet |
| 2026-11-22 | All repository `.md` files in English | ready for going public |
| 2026-11-22 | DCO with human sign-off; `Assisted-by:` for AI help | only humans can certify the DCO (as in the Linux kernel) |
| 2026-11-22 | Releases: Stable (tested rolling snapshots), Beta, Nightly; no LTS | Arch keeps no old packages; LTS is a full-time job |
| 2026-11-22 | Versioning `year.quarter.number`, SemVer pre-releases for Beta/Nightly | sorts automatically, readable at a glance |
| 2026-11-21 | Branding: Pine primary, Ochre secondary, three cat mascots | personal character, name reflected in the look |
| 2026-11-21 | Security: Ochra Guard with per-process consent, enforced in the kernel | fixes the per-app weakness of macOS TCC |
| 2026-11-21 | Toolkit: iced | pure Rust, MIT, proven by COSMIC, full control over M3E |
| 2026-11-21 | Install modes: Stable A/B (EROFS + ext4), Classic ext4, Btrfs later | own Btrfs driver is too big; A/B is safe for everyone |
| 2026-11-21 | pacman as main package manager; Flatpak; apk/apt only in containers | several managers on one `/` cause chaos |
| 2026-11-21 | Compositor: OchraWM on Smithay, Wayland + XWayland | Wayland is a protocol; Smithay handles the plumbing |
| 2026-11-21 | Licences: kernel GPL-2.0-only, userspace MIT OR Apache-2.0, art CC BY-SA 4.0 | GPLv2 allows Linux driver code; permissive userspace suits Rust |
| 2026-11-21 | Language: Rust | owner's target language; Asterinas shows it works |
| 2026-11-21 | Linux-ABI-compatible kernel instead of a translation layer | Linux binaries run natively |
| 2026-11-21 | OxideOS archived, OchraOS started from scratch | the old code stopped being fun |
