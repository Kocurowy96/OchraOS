# Decision log

Short record of what was decided and why. Newest first.

| Date | Decision | Why |
| --- | --- | --- |
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
