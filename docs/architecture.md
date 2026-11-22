# Architecture

## Vision

OchraOS is a from-scratch OS in Rust whose kernel speaks the Linux ABI. Linux binaries — and
therefore Wine and, eventually, Steam — run natively. The system aims to be beautiful,
lightweight and safe, for power users and for less technical people alike.

## Kernel

- Monolithic kernel with all low-level `unsafe` confined to one small crate ("framekernel",
  as in Asterinas). The rest of the kernel is safe Rust.
- x86_64 only, UEFI only, minimum x86-64-v2.
- Boot: Limine first; later an own UEFI bootloader (`ochra-boot`, Material 3 Expressive look)
  that speaks the Limine protocol, so the kernel needs no changes. Dual boot by chain-loading
  `bootmgfw.efi`.
- Designed for SMP from the start. SMEP/SMAP, KASLR, W^X.
- Linux driver compatibility layer (LinuxKPI-style) at stage 8.

## Hardware

| Phase | Platform |
| --- | --- |
| Start | QEMU/KVM with virtio |
| First real hardware | ThinkPad T440p |
| Next | ProDesk 600 G4, ThinkPad T460s |
| Last | gaming laptop with RTX 4060 |

## Filesystem layout

Linux FHS with merged `/usr` (as in Arch), because Linux binaries hard-code these paths.

| Path | Content | Stable mode |
| --- | --- | --- |
| `/usr` | programs and libraries (`/bin`, `/lib` link into `/usr`) | EROFS image, read-only |
| `/usr/lib/ochra`, `/usr/share/ochra` | OchraOS components, recipes, themes | in the image |
| `/etc` | system configuration | defaults from the image + changes on the data partition |
| `/var` | logs, caches, package databases, containers | data partition |
| `/home` | user files | data partition |

The file manager shows friendly locations (This computer, Documents, Downloads) and hides the
system tree behind an "advanced" option.

## Install modes and packages

| Mode | Filesystem | Updates | pacman |
| --- | --- | --- | --- |
| Stable (recommended) | A/B: EROFS system image + ext4 data partition | new image on the other slot, automatic fallback | in a default Arch container, apps exported to the menu |
| Classic | ext4, writable `/` | pacman | directly on the system |
| Btrfs (after stage 8) | Btrfs with snapshots | pacman + snapshot | directly on the system |

- Stages 0–6 use a plain writable rootfs; A/B arrives around stage 7.
- Flatpak available in every mode.
- apk and apt only inside containers (`ochra-box create debian`), never on the main `/`.
- The installer describes modes by what they give, never by calling anyone a beginner.

## Init and systemd compatibility

`ochra-init` (Rust) is PID 1 and service manager. It implements the systemd interfaces
programs actually use — API compatibility, as elogind does:

| Interface | Needed by | Difficulty |
| --- | --- | --- |
| libsystemd: `sd_notify`, journal | daemons | low |
| `.service` unit files (subset) | Arch packages that install services | medium |
| logind over D-Bus (`org.freedesktop.login1`) | Wayland compositors, sessions, GPU access | high |
| udev / libudev | device discovery (alternative: eudev) | medium |

Users may swap in real systemd later; that requires cgroups v2, namespaces and fanotify.

## Desktop

- **OchraWM:** own Wayland compositor built on Smithay, XWayland for X11 apps. Default look:
  Material 3 Expressive; other styles available.
- **Shell** (panel or dock, start menu, quick settings, notifications) are separate programs
  talking to the compositor via layer-shell. Windows-like layout by default; macOS-like
  layout selectable.
- **Toolkit:** iced (MIT) for system apps and the shell, own M3E theme. egui only for
  developer tools. Later: M3E theme for GTK and Qt, accent colour via xdg-desktop-portal.
- **Ochra Welcome:** first-login configurator — layout, style, startup apps, environments.
- **Recipes:** declarative TOML files describing an environment (e.g. Affinity via Wine: fonts,
  DXVK/vkd3d). A recipe shows its changes before running; paid installers are supplied by the
  user.
- **Widgets, desktop mascots, live wallpapers** — optional, off by default. Wallpapers are
  shaders + video + a scene manifest only (no HTML/JS), and pause when hidden or on battery.

## Windows applications

One GUI app installs and manages Windows programs and picks the route automatically:

| Route | When | Pros | Cons |
| --- | --- | --- | --- |
| Wine (one prefix per app) | default for programs | light, good GPU via DXVK | some programs fail |
| Proton | games, also outside Steam | best game compatibility | games only |
| Windows in a VM | when Wine and Proton fail (e.g. Office) | near-full compatibility, isolated | no real GPU, at least 4 GB RAM, some anti-cheats block VMs |

- Route chosen from the recipe database; unknown programs try Wine first and offer the VM on
  failure.
- The VM starts with the first Windows app and suspends when none is used.
- Folder access for the VM goes through Ochra Guard.
- Windows requires the user's own legal licence (IoT LTSC is not sold to individuals).

## Ochra SDK

Own API for applications, in userspace (as `windows.h` is userspace on top of NT): a crate
with M3E widgets on iced plus D-Bus interfaces for accent colour, quick-settings tiles,
widgets and recipes. Apps built with it also run on ordinary Linux with fallbacks.

## Kocur Link

Open, documented protocol for the owner's devices (KocurDeck, keyboard, KocurLeds): pairing
pop-up, settings inside OchraOS Settings, firmware updates with system updates, accent colour
synced to room lights. Devices authenticate with a cryptographic signature.

## Lightness budget

| State | Budget (used memory, excluding cache) |
| --- | --- |
| Kernel + init, no GUI | under 64 MB |
| Idle desktop (OchraWM + shell) | under 500 MB |

Measured in CI; a change that exceeds the budget fails the build. Services start on demand,
no telemetry, the compositor redraws only damaged regions, system apps are native Rust.

## Principles

- **Close means close.** Background apps must ask (via Ochra SDK) and are listed with a toggle.
- **Updates never interrupt.** A/B installs in the background, no forced restarts, throttled
  while a game runs.
- **Nothing pushed on the user.** No ads, no suggested apps, no forced online accounts.
- **Old hardware is a first-class citizen.**
- **One Settings app,** searchable, plain language, every option links to the manual.
- **Open instead of closed ecosystems.**

## Components

Write our own only what gives OchraOS its character; reuse proven pieces.

| Group | Own components |
| --- | --- |
| Boot and kernel | `ochra-kernel`, `ochra-boot` |
| System services | `ochra-init`, `ochra-journal`, libsystemd compatibility, `ochra-logind`, `ochra-guard`, `ochra-portal`, `ochra-update`, `ochra-box`, `kocur-link` |
| Desktop | OchraWM, `ochra-shell`, `ochra-greeter` |
| Applications | installer, Ochra Welcome, Settings, file manager, terminal, task manager, OchraStore (optional), Windows apps manager, text editor, `ochra-crash`, recovery mode |
| Developer tools | `xtask`, syscall tests, RAM budget check, `ochra-build`, release script, Ochra SDK, recipe format, docs (mdBook) |

Reused: NetworkManager or iwd, PipeWire, greetd, `sudo-rs`, polkit, Flatpak, Wine, Proton,
FreeRDP, QEMU, mpv, Firefox or Chromium, Smithay, iced.

## First boot and crash screen

- First serial output: ASCII-art cat in ochre and pine colours, not "Hello, world".
- Stable crash screen: Selenit the Devon Rex, a QR code, full log saved in the background.
- Beta/Nightly crash screen: registers, symbolised backtrace, last syscalls, commit hash.
