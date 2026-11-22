# Roadmap

Every stage ends with something that can be run and checked. No dates — this is a hobby project.

## Stage 0 — Foundation ✅

Rust `no_std` kernel, Limine bootloader (UEFI), QEMU, serial logging, `cargo xtask`, CI.

**Done when:** the kernel boots in QEMU and prints a log (and the ASCII-art cat) on the serial
port; CI boots it headless and checks the log.

Checklist:
- [x] `rust-toolchain.toml`, workspace, target `x86_64-unknown-none`
- [x] Limine boot in QEMU with OVMF
- [x] serial logger (COM1), coloured output (ochre `#CC7722`, pine `#047857`)
- [x] GDT, IDT, readable exception handlers (page fault, double fault)
- [x] `cargo xtask build|run|test`
- [x] GitHub Actions: fmt, clippy, build, headless boot test

## Stage 1 — Core

Paging, physical frame allocator, kernel heap, interrupts, APIC, timer, scheduler, processes
and threads.

**Done when:** two kernel tasks switch on the timer interrupt.

## Stage 2 — Linux ABI

`syscall` entry with Linux numbering, ELF loader with auxv, Ring 3, signals, initramfs (cpio).

**Done when:** a static musl "hello" runs, then busybox `sh` works (`fork`, `execve`, `wait4`,
`pipe`).

## Stage 3 — Files and disks

VFS, tmpfs, ext2 then ext4, EROFS (read-only), AHCI and NVMe, minimal `/proc`, `/sys`, `/dev`.

**Done when:** the system boots from an ext4 partition on a virtual disk.

## Stage 4 — Full userspace

Dynamic glibc, `futex`, threads, file-backed `mmap`, `epoll`.

**Done when:** bash, coreutils and python run from an Alpine rootfs.

## Stage 5 — Network and init

virtio-net, TCP/IP (e.g. smoltcp), sockets, own init in Rust (`ochra-init`).

**Done when:** `apk add` downloads a package from the internet.

## Stage 6 — Graphics and sessions

Framebuffer, DRM via virtio-gpu, evdev, logind-compatible session service.

**Done when:** a Wayland compositor shows a window.

## Stage 7 — Desktop applications

Wine, audio (PipeWire), USB, Arch packages, A/B updates, installer, Ochra Welcome.

**Done when:** a simple Windows program runs through Wine.

## Stage 8 — Real hardware and GPU

Linux driver compatibility layer (LinuxKPI-style), NVIDIA open kernel modules, cgroups v2,
namespaces, seccomp.

**Done when:** Steam with Proton runs a game on the RTX 4060.

## Stage 9 — Virtualisation and Windows in a VM

KVM-style interface (`/dev/kvm`), virtual TPM and Secure Boot for the guest, Windows apps shown
as individual OchraWM windows (RDP RemoteApp).

**Done when:** a Windows application inside the VM opens as a normal window on the OchraOS
desktop.

## In parallel

OchraWM and the Settings app can be developed on the owner's Garuda Linux machine from day one:
thanks to the Linux ABI they will run unchanged on OchraOS at stage 6.
