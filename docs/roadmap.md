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

## Stage 1 — Core ✅

Paging, physical frame allocator, kernel heap, interrupts, APIC, timer, scheduler, processes
and threads.

**Done when:** two kernel tasks switch on the timer interrupt.

Checklist:
- [x] `ochra-frame` crate (`frame/`) holds all low-level `unsafe`; `kernel` is
      `#![forbid(unsafe_code)]`
- [x] PIE kernel with KASLR; SMEP/SMAP/UMIP enabled when CPUID reports them
- [x] physical frame allocator (+ debug self-test)
- [x] own page tables: W^X, NX, guard pages under every kernel stack (IST and task stacks);
      SMEP/SMAP self-test with a user page
- [x] kernel heap with an own simple allocator, no external allocator crate (+ debug self-test)
- [x] interrupts: legacy PIC masked, local APIC (x2APIC, falling back to xAPIC), IRQ-safe locks
- [x] LAPIC timer, calibrated
- [x] scheduler: kernel tasks with their own stacks, context switch, round-robin on the timer
- [x] boot test checks that tasks A and B interleave (order only, never timing: TCG in CI is
      not stable in time)

## Stage 1.5 — SMP ✅

Start the other CPU cores (application processors), per-CPU GDT/TSS/IST and data, scheduler
on every core.

**Done when:** tasks run on more than one core in QEMU with `-smp 4`.

Checklist:
- [x] APs started through the Limine MP request; each loads the kernel page tables and
      switches on SMEP/SMAP/UMIP/WP
- [x] per-CPU data through the GS base (CPU index, APIC ID, current task) + self-test
- [x] own GDT, TSS and double fault stack (with a guard page) on every CPU + crash test
- [x] local APIC and timer on every CPU (calibrated once)
- [x] a panic or fatal exception stops every CPU
- [x] scheduler on every CPU (one global run queue, an idle task per CPU)
- [x] boot test: tasks run on at least two CPUs (order and set only, never timing); a
      single-CPU boot still works

## Stage 2 — Linux ABI

`syscall` entry with Linux numbering, ELF loader with auxv, Ring 3, signals, initramfs (cpio).

**Done when:** a static musl "hello" runs, then busybox `sh` works (`fork`, `execve`, `wait4`,
`pipe`).

Checklist:
- [x] P1: per-process address spaces with TLB shootdown; ring 3 and the `syscall` entry
      (`swapgs`, `TSS.RSP0`, `sysret` hardening); `copy_from_user`/`copy_to_user` with SMAP
      and exception fixup (`EFAULT`, never a panic); syscall table with Linux numbers
- [x] P2: initramfs (cpio, Limine module), ELF loader with auxv, `brk`/`mmap`/`munmap`/
      `mprotect`, TLS, ChaCha20 CSPRNG in `frame` (seeded from RDRAND, RDSEED and the TSC,
      with CPUID and sanity checks) for `AT_RANDOM`, ASLR and `getrandom`; musl "hello"
- [x] P3: wait queues, blocking and wake-up from another CPU, task exit; processes (`fork`,
      `execve`, `exit`, `wait4`), zombies until `wait4`, orphans to PID 1, panic when init
      dies, address space freed with TLB shootdown; file descriptors, pipes, console;
      test that a new process starts with a clean FPU/SSE state (FCW 0x37F, MXCSR 0x1F80,
      XMM zero) and never sees another process's registers; a gap of 256 pages below the
      stack like Linux's `stack_guard_gap`, so `mmap` cannot place a mapping right under it
      (Stack Clash), with a test
- [x] P4: signals (`rt_sigaction`, `rt_sigprocmask`, `kill`, delivery, `rt_sigreturn`),
      `EINTR` and `SA_RESTART`, `SIGPIPE`, `SIGCHLD`; busybox `sh` as init (`#!` scripts,
      symbolic links, `/dev/null`, `open`/`stat` on the initramfs); boot test with a shell
      script and an interactive shell on the console
- [x] every syscall has a small C test program; its deterministic output is compared with
      the same program run on the host Linux

## Stage 3 — Files and disks

VFS, tmpfs, ext2 then ext4, EROFS (read-only), AHCI and NVMe, minimal `/proc`, `/sys`, `/dev`;
directory reading (`getdents64`); terminal control (termios, line editing, Ctrl-C and other
signals from the terminal) and job control (sessions, process groups, `setpgid`,
`tcsetpgrp`), moved here from stage 2.

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
