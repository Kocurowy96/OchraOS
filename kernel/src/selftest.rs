// SPDX-License-Identifier: GPL-2.0-only

//! Built-in self-tests, compiled in only with the `selftest` feature (refused in release
//! builds, here and in `ochra-frame`). `cargo xtask test` builds the kernel with it.
//!
//! - **Checks** ([`run_checks`]) run at every boot and must not disturb the system: they test
//!   the kernel's own services (frame allocator…) and log `self-test <name>: ok`, or panic.
//! - **Crash tests** ([`run`]) are selected with `test=<name>` on the kernel command line. They
//!   deliberately crash the kernel in a known way, so the test can check the crash report:
//!
//! | name          | what happens                                          |
//! |---------------|-------------------------------------------------------|
//! | `pagefault`   | write to an unmapped address -> page fault             |
//! | `doublefault` | stack pointer to unmapped memory -> double fault      |
//! | `wx`          | write to kernel code -> page fault (W^X)              |
//! | `nx`          | execute kernel data -> page fault (NX)                |
//! | `guardpage`   | write below a kernel stack -> page fault (guard page) |
//! | `smep`        | execute a user page -> page fault (SMEP)              |
//! | `smap`        | read a user page -> page fault (SMAP)                 |
//!
//! `test=usertests` instead runs every program in the initramfs's `/tests` (see
//! [`run_user_tests`]), in place of `/init`.

#[cfg(all(feature = "selftest", not(debug_assertions)))]
compile_error!("the `selftest` feature must never be enabled in a release build");

/// Runs the self-test `name`. Without the `selftest` feature it only logs a warning.
#[cfg(feature = "selftest")]
pub fn run(name: &str) {
    use ochra_frame::selftest as crash;

    log::info!("running self-test {name:?}");
    match name {
        "pagefault" => crash::page_fault(),
        "doublefault" => crash::double_fault(),
        "wx" => crash::write_to_code(),
        "nx" => crash::execute_data(),
        "guardpage" => crash::stack_guard_page(),
        "smep" => crash::smep(),
        "smap" => crash::smap(),
        // Runs as a task once the scheduler starts.
        "taskstack" => crate::sched::spawn("overflow", overflow_task_stack),
        // User programs that must be killed, while the kernel carries on.
        "userkernel" => spawn_user_program("kernel"),
        "userhlt" => spawn_user_program("hlt"),
        "usertests" => {
            USER_TESTS_RUNNING.store(true, core::sync::atomic::Ordering::Relaxed);
            crate::sched::spawn("usertests", run_user_tests);
        }
        // Cpu 1 is waiting for this signal, see `on_ap`.
        "apdoublefault" => AP_DOUBLE_FAULT_GO.store(true, core::sync::atomic::Ordering::Release),
        _ => log::error!("unknown self-test {name:?}"),
    }
}

#[cfg(feature = "selftest")]
static USER_TESTS_RUNNING: core::sync::atomic::AtomicBool =
    core::sync::atomic::AtomicBool::new(false);

/// Whether `test=usertests` is on the command line; then `/init` is not started.
pub fn runs_user_tests(command_line: &str) -> bool {
    cfg!(feature = "selftest")
        && command_line
            .split_whitespace()
            .any(|argument| argument == "test=usertests")
}

/// Runs the test programs from `/tests`, one at a time, and logs one line per program: how
/// it ended, and its output in hexadecimal (so the line stays one line whatever the program
/// printed). `cargo xtask test` runs the same programs on the host Linux and compares.
///
/// As on the host, stdin is empty (`/dev/null`), stdout is a pipe read until its end (so
/// output from children that outlive the program counts too), stderr is the console.
#[cfg(feature = "selftest")]
fn run_user_tests() -> ! {
    use alloc::vec::Vec;

    let mut names: Vec<&str> = crate::initramfs::files()
        .filter_map(|(path, _)| path.strip_prefix("tests/"))
        .collect();
    names.sort_unstable();
    for name in &names {
        let started = crate::time::monotonic();
        let (ending, output) = run_user_test(name);
        let took = (crate::time::monotonic() - started) / 1_000_000;
        log::debug!("user test {name} took {took} ms");
        let hex: alloc::string::String = output
            .iter()
            .map(|byte| alloc::format!("{byte:02x}"))
            .collect();
        log::info!("user test {name}: {ending}, output {hex}");
    }
    log::info!("user tests: {} programs finished", names.len());

    // Everything a process had (pages, page tables, kernel stack, its records on the heap)
    // must come back. The first round also grew the kernel heap and its tables, so compare
    // two more rounds with each other.
    let mut free = [0; 2];
    let mut heap_used = [0; 2];
    for (round, used) in free.iter_mut().zip(&mut heap_used) {
        for name in &names {
            run_user_test(name);
        }
        // Exited tasks are freed by the next switch on their CPU: a few ticks are plenty.
        let start = crate::time::ticks();
        while crate::time::ticks() < start + 3 {
            ochra_frame::arch::interrupts::wait();
        }
        *round = ochra_frame::mm::frames::stats().free;
        *used = ochra_frame::mm::heap::stats().used;
    }
    let leaked = free[0].saturating_sub(free[1]);
    assert_eq!(leaked, 0, "user tests: {leaked} frames leaked in one round");
    let heap_leaked = heap_used[1].saturating_sub(heap_used[0]);
    assert_eq!(
        heap_leaked, 0,
        "user tests: {heap_leaked} heap bytes leaked in one round"
    );
    log::info!("user tests: no frames or heap memory leaked over two more rounds");
    USER_TESTS_RUNNING.store(false, core::sync::atomic::Ordering::Release);
    crate::sched::exit_current()
}

/// Runs one test program from `/tests` and returns how it ended (`exit N` or `signal N`) and
/// what it wrote to stdout.
#[cfg(feature = "selftest")]
fn run_user_test(name: &str) -> (alloc::string::String, alloc::vec::Vec<u8>) {
    use alloc::sync::Arc;
    use alloc::vec::Vec;

    use crate::file::{self, Descriptor, FdTable, Null, OpenFile};

    let path = alloc::format!("/tests/{name}");
    let (reader, writer) = file::pipe();
    let mut files = FdTable::default();
    let descriptor = |file, mode| Descriptor {
        file: OpenFile::new(file, mode),
        close_on_exec: false,
    };
    for (fd, file) in [
        (0, Arc::new(Null) as Arc<dyn file::File>),
        (1, writer),
        (2, crate::tty::console_file()),
    ] {
        let _ = files.insert_at(fd, descriptor(file, 0));
    }
    let process = match crate::process::spawn(&path, &[path.as_bytes()], &[], files) {
        Ok(process) => process,
        Err(errno) => panic!("user test {name}: cannot start it (errno {errno})"),
    };
    let mut output = Vec::new();
    let mut buffer = [0; 4096];
    loop {
        match reader.read(&mut buffer, false) {
            Ok(0) => break,
            Ok(count) => output.extend_from_slice(&buffer[..count]),
            Err(errno) => panic!("user test {name}: reading its output failed ({errno})"),
        }
    }
    let ending = match crate::process::wait_kernel_child(&process) {
        crate::process::Exit::Status(status) => alloc::format!("exit {status}"),
        crate::process::Exit::Signal(signal) => alloc::format!("signal {signal}"),
    };
    (ending, output)
}

/// Recurses until the task's stack runs into its guard page. The page fault handler then
/// has no stack to run on either, so the CPU raises a double fault, which has its own stack.
#[cfg(feature = "selftest")]
fn overflow_task_stack() -> ! {
    fn recurse(depth: u64) -> u64 {
        // A way out the compiler cannot see through, so it keeps the recursion.
        if core::hint::black_box(depth) == u64::MAX {
            return 0;
        }
        let buffer = core::hint::black_box([depth; 64]);
        recurse(depth + 1).wrapping_add(buffer[0])
    }
    recurse(0);
    unreachable!("the task stack never overflowed");
}

/// Runs the self-test `name`. Without the `selftest` feature it only logs a warning.
#[cfg(not(feature = "selftest"))]
pub fn run(name: &str) {
    log::warn!("ignoring test={name}: this kernel is built without self-tests");
}

/// Runs every check. Without the `selftest` feature it does nothing.
#[cfg(feature = "selftest")]
pub fn run_checks() {
    check_random();
    check_frames();
    check_heap();
    check_cpus();
    check_interrupts();
    check_timer();
    check_user_space();
    check_terminal();
}

/// Terminals: `TIOCSTI` (pushing bytes into a terminal's input, as if typed) is refused
/// with `EIO`, always (docs/security.md). Tested here rather than against the host, whose
/// answer depends on its `dev.tty.legacy_tiocsti` setting.
#[cfg(feature = "selftest")]
fn check_terminal() {
    const TIOCSTI: u64 = 0x5412;
    let console = crate::tty::console_file();
    assert_eq!(
        console.ioctl(TIOCSTI, 0),
        Err(crate::errno::EIO),
        "self-test terminal: TIOCSTI was not refused"
    );
    log::info!("self-test terminal: ok");
}

/// Runs every check. Without the `selftest` feature it does nothing.
#[cfg(not(feature = "selftest"))]
pub fn run_checks() {}

/// Frame allocator: frames are aligned, distinct, really separate memory, and come back
/// when dropped.
#[cfg(feature = "selftest")]
fn check_frames() {
    use ochra_frame::mm::FRAME_SIZE;
    use ochra_frame::mm::frames::{self, Frame};

    const COUNT: usize = 64;
    const PATTERN: u64 = 0x0C4A_7A05_0C4A_7A05;
    const LAST_WORD: usize = FRAME_SIZE as usize / 8 - 1;

    let before = frames::stats();
    let mut taken: [Option<Frame>; COUNT] = [const { None }; COUNT];
    for slot in taken.iter_mut() {
        let mut frame = frames::allocate().expect("self-test frames: out of memory");
        let address = frame.address().0;
        assert!(address != 0, "self-test frames: frame at address 0");
        assert!(
            address.is_multiple_of(FRAME_SIZE),
            "self-test frames: {address:#x} not aligned"
        );
        // Mark the first and last word with the frame's own address.
        frame.write_u64(0, address ^ PATTERN);
        frame.write_u64(LAST_WORD, address ^ PATTERN);
        *slot = Some(frame);
    }
    assert_eq!(
        frames::stats().free,
        before.free - COUNT as u64,
        "self-test frames: wrong free count after allocating"
    );

    // If two frames overlapped, one of them would now hold the other's mark.
    for (i, frame) in taken.iter().flatten().enumerate() {
        let address = frame.address().0;
        assert_eq!(
            frame.read_u64(0),
            address ^ PATTERN,
            "self-test frames: overwritten"
        );
        assert_eq!(
            frame.read_u64(LAST_WORD),
            address ^ PATTERN,
            "self-test frames: overwritten"
        );
        for other in taken.iter().flatten().skip(i + 1) {
            assert!(
                other.address().0 != address,
                "self-test frames: {address:#x} twice"
            );
        }
    }

    // Dropping the frames gives them back.
    taken = [const { None }; COUNT];
    drop(taken);
    assert_eq!(
        frames::stats(),
        before,
        "self-test frames: frames lost after freeing"
    );
    let again = frames::allocate().expect("self-test frames: cannot allocate after freeing");
    drop(again);

    log::info!("self-test frames: ok ({COUNT} frames)");
}

/// Heap: `Box`, `Vec` and `String` work, alignment is honoured, blocks do not overlap, the heap
/// grows when needed, and freeing everything gives all memory back in one piece.
#[cfg(feature = "selftest")]
fn check_heap() {
    use alloc::boxed::Box;
    use alloc::format;
    use alloc::vec::Vec;
    use ochra_frame::mm::heap;

    let before = heap::stats();
    {
        let answer = Box::new(42_u64);
        assert_eq!(*answer, 42, "self-test heap: Box");

        // Growing a Vec moves it to larger blocks again and again.
        let numbers: Vec<u64> = (0..10_000).collect();
        assert_eq!(
            numbers.iter().sum::<u64>(),
            49_995_000,
            "self-test heap: Vec"
        );

        let text = format!("{} {}", "Bursztyn", 0xCC7722);
        assert_eq!(text, "Bursztyn 13399842", "self-test heap: String");

        #[repr(align(4096))]
        struct PageAligned([u8; 4096]);
        let page = Box::new(PageAligned([7; 4096]));
        let address = &raw const *page as usize;
        assert!(address.is_multiple_of(4096), "self-test heap: alignment");
        assert!(
            page.0.iter().all(|&byte| byte == 7),
            "self-test heap: aligned block"
        );

        // Many blocks of different sizes, each filled with its own byte. Free every other one,
        // allocate again, and check that no block was overwritten.
        let mut blocks: Vec<Vec<u8>> = (1..200).map(|n| alloc::vec![n as u8; n * 7]).collect();
        for block in blocks.iter_mut().step_by(2) {
            *block = Vec::new();
        }
        let refill: Vec<Vec<u8>> = (1..100).map(|n| alloc::vec![0xEE; n * 5]).collect();
        for (n, block) in blocks.iter().enumerate().skip(1).step_by(2) {
            let mark = (n + 1) as u8;
            assert!(
                block.iter().all(|&byte| byte == mark),
                "self-test heap: overlap"
            );
        }
        assert!(
            refill.iter().flatten().all(|&byte| byte == 0xEE),
            "self-test heap: overlap"
        );

        // More than the first mapping: the heap has to grow.
        let big: Vec<u8> = alloc::vec![1; 4 * 1024 * 1024];
        assert_eq!(big.len(), 4 * 1024 * 1024, "self-test heap: growth");
        assert!(
            heap::stats().mapped > before.mapped,
            "self-test heap: did not grow"
        );
    }
    let after = heap::stats();
    assert_eq!(after.used, before.used, "self-test heap: memory leaked");
    // What the test freed merged back: no more free blocks than before it (the VFS and the
    // rest of the boot allocated before, so there may be holes it did not make).
    assert!(
        after.free_blocks <= before.free_blocks,
        "self-test heap: free blocks not merged ({} before, {} after)",
        before.free_blocks,
        after.free_blocks
    );

    log::info!(
        "self-test heap: ok ({} KiB mapped after growing)",
        after.mapped / 1024
    );
}

/// Interrupts: an interrupt sent through the local APIC reaches the handler registered for
/// its vector (and the end-of-interrupt lets the next one through: the timer keeps ticking).
#[cfg(feature = "selftest")]
fn check_interrupts() {
    use core::sync::atomic::{AtomicU32, Ordering};
    use ochra_frame::arch::{apic, interrupts};

    const VECTOR: u8 = 0x40;
    static RECEIVED: AtomicU32 = AtomicU32::new(0);
    fn handler(vector: u8) {
        assert_eq!(vector, VECTOR, "self-test interrupts: wrong vector");
        RECEIVED.fetch_add(1, Ordering::Relaxed);
    }

    interrupts::set_handler(VECTOR, handler);
    for round in 1..=3 {
        apic::send_self_ipi(VECTOR);
        // Each `wait` returns after at least one interrupt (the IPI or a timer tick), so this
        // gives the IPI plenty of chances without depending on how fast the machine is.
        for _ in 0..100 {
            if RECEIVED.load(Ordering::Relaxed) == round {
                break;
            }
            interrupts::wait();
        }
        assert_eq!(
            RECEIVED.load(Ordering::Relaxed),
            round,
            "self-test interrupts: IPI {round} not received"
        );
    }
    log::info!("self-test interrupts: ok (3 self-IPIs on vector {VECTOR:#x})");
}

/// Timer: ticks keep arriving. Only the order of events is checked, never how long they take
/// (emulation in CI is not stable in time).
#[cfg(feature = "selftest")]
fn check_timer() {
    use crate::time;
    use ochra_frame::arch::interrupts;

    let cpus = ochra_frame::smp::cpu_count();
    let start: alloc::vec::Vec<u64> = (0..cpus).map(time::ticks_on).collect();
    let all_ticked = || (0..cpus).all(|cpu| time::ticks_on(cpu) >= start[cpu as usize] + 5);
    for _ in 0..1000 {
        if all_ticked() {
            break;
        }
        interrupts::wait();
    }
    for cpu in 0..cpus {
        let seen = time::ticks_on(cpu) - start[cpu as usize];
        assert!(seen >= 5, "self-test timer: only {seen} ticks on cpu {cpu}");
    }
    log::info!("self-test timer: ok (every one of {cpus} CPUs got at least 5 ticks)");
}

/// Steps each scheduler test task takes.
#[cfg(feature = "selftest")]
const TASK_STEPS: u32 = 5;
#[cfg(feature = "selftest")]
static TASKS_DONE: core::sync::atomic::AtomicU32 = core::sync::atomic::AtomicU32::new(0);

/// Scheduler: two tasks, A and B, each log a few steps and wait for the timer in between. If
/// the timer switches tasks, their lines interleave; `cargo xtask test` checks that order
/// (never how long anything takes).
#[cfg(feature = "selftest")]
pub fn spawn_tasks() {
    fn steps(name: &str) {
        for step in 1..=TASK_STEPS {
            log::info!("task {name}: step {step} of {TASK_STEPS}");
            ochra_frame::arch::interrupts::wait();
        }
        TASKS_DONE.fetch_add(1, core::sync::atomic::Ordering::Release);
    }
    fn task_a() -> ! {
        steps("A");
        loop {
            ochra_frame::arch::interrupts::wait();
        }
    }
    fn task_b() -> ! {
        steps("B");
        loop {
            ochra_frame::arch::interrupts::wait();
        }
    }
    crate::sched::spawn("A", task_a);
    crate::sched::spawn("B", task_b);
    spawn_user_program("hello");
}

/// Spawns the ring 3 test program `name` (see `ochra_frame::selftest::user_program`) in a new
/// address space: its code at 4 MiB (read and execute), one stack page at the top of the
/// user half.
#[cfg(feature = "selftest")]
fn spawn_user_program(name: &'static str) {
    use alloc::sync::Arc;
    use ochra_frame::mm::frames;
    use ochra_frame::mm::user::{USER_END, UserAccess, UserSpace};

    const CODE: u64 = 0x40_0000;
    let space = UserSpace::new().expect("out of memory");
    let mut code = frames::allocate().expect("out of memory");
    code.zero();
    code.write_bytes(0, ochra_frame::selftest::user_program(name));
    let executable = UserAccess::READ_EXECUTE;
    let writable = UserAccess::READ_WRITE;
    let mut stack = frames::allocate().expect("out of memory");
    stack.zero();
    let stack_page = USER_END - 4096;
    assert!(space.map(CODE, code, executable).is_ok());
    assert!(space.map(stack_page, stack, writable).is_ok());
    crate::sched::spawn_user(name, Arc::new(space), CODE, USER_END, None);
}

/// Without self-tests there are no test tasks.
#[cfg(not(feature = "selftest"))]
pub fn spawn_tasks() {}

/// Whether the tasks from [`spawn_tasks`] and the user tests are done.
pub fn tasks_finished() -> bool {
    #[cfg(feature = "selftest")]
    return TASKS_DONE.load(core::sync::atomic::Ordering::Acquire) == 2
        && !USER_TESTS_RUNNING.load(core::sync::atomic::Ordering::Acquire);
    #[cfg(not(feature = "selftest"))]
    true
}

/// One bit per CPU that reported in through [`cpu_arrived`] (at most 64 CPUs in this test).
#[cfg(feature = "selftest")]
static CPUS_ARRIVED: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);

/// Called by every AP when it reaches the kernel: records the CPU index read through its own
/// per-CPU data (GS), which must match the one it was given.
#[cfg(feature = "selftest")]
pub fn cpu_arrived(index: u32) {
    use core::sync::atomic::Ordering;
    let own = ochra_frame::cpu_local::cpu_index();
    assert_eq!(
        own, index,
        "self-test cpus: per-CPU data of cpu {index} says {own}"
    );
    let bit = 1 << own;
    let before = CPUS_ARRIVED.fetch_or(bit, Ordering::AcqRel);
    assert!(
        before & bit == 0,
        "self-test cpus: cpu {own} reported twice"
    );
}

#[cfg(not(feature = "selftest"))]
pub fn cpu_arrived(_index: u32) {}

/// Per-CPU data: every CPU reports its own index exactly once, the boot CPU included.
#[cfg(feature = "selftest")]
fn check_cpus() {
    use core::sync::atomic::Ordering;
    use ochra_frame::smp;

    cpu_arrived(0);
    let count = smp::cpu_count();
    assert!(count <= 64, "self-test cpus: more than 64 CPUs");
    let all = u64::MAX >> (64 - count);
    // The APs are online, but may not have reached `cpu_arrived` yet. Spinning is enough:
    // the timer is not running yet, and each AP needs only a few instructions more.
    let mut spins: u64 = 0;
    while CPUS_ARRIVED.load(Ordering::Acquire) != all {
        spins += 1;
        assert!(spins < 1 << 32, "self-test cpus: not every CPU reported in");
        core::hint::spin_loop();
    }
    log::info!("self-test cpus: ok ({count} CPUs, each found its own per-CPU data)");
}

/// Set by `test=apdoublefault` before the APs start: cpu 1 will raise a double fault, which
/// checks that every AP has its own TSS and double fault stack.
#[cfg(feature = "selftest")]
static AP_DOUBLE_FAULT: core::sync::atomic::AtomicBool = core::sync::atomic::AtomicBool::new(false);
/// Set by `run("apdoublefault")`, after the boot checks: the moment for cpu 1 to fault (the
/// crash report keeps the serial port, so nothing is logged after it).
#[cfg(feature = "selftest")]
static AP_DOUBLE_FAULT_GO: core::sync::atomic::AtomicBool =
    core::sync::atomic::AtomicBool::new(false);

/// Looks at the command line for tests that must be armed before the APs start.
#[cfg(feature = "selftest")]
pub fn prepare(command_line: &str) {
    if command_line
        .split_whitespace()
        .any(|argument| argument == "test=apdoublefault")
    {
        AP_DOUBLE_FAULT.store(true, core::sync::atomic::Ordering::Relaxed);
    }
}

#[cfg(not(feature = "selftest"))]
pub fn prepare(_command_line: &str) {}

/// Called by every AP when it reaches the kernel, with interrupts on.
pub fn on_ap(index: u32) {
    cpu_arrived(index);
    #[cfg(feature = "selftest")]
    if index == 1 && AP_DOUBLE_FAULT.load(core::sync::atomic::Ordering::Relaxed) {
        // Waiting with interrupts on, so this CPU's timer keeps ticking for `check_timer`.
        while !AP_DOUBLE_FAULT_GO.load(core::sync::atomic::Ordering::Acquire) {
            ochra_frame::arch::interrupts::wait();
        }
        ochra_frame::selftest::double_fault();
    }
}

/// User address spaces: mapping, permissions, unmapping and freeing; every frame (pages and
/// page tables) comes back. Then a TLB shootdown that every other CPU must answer.
#[cfg(feature = "selftest")]
fn check_user_space() {
    use ochra_frame::mm::frames;
    use ochra_frame::mm::user::{MapError, USER_END, UserAccess, UserSpace};

    const BASE: u64 = 0x40_0000;
    let read_write = UserAccess::READ_WRITE;
    let before = frames::stats();
    {
        let space = UserSpace::new().expect("self-test user space: out of memory");
        for page in 0..3u64 {
            let mut frame = frames::allocate().expect("out of memory");
            frame.write_u64(0, 0x0C4A_0000 + page);
            let address = frame.address();
            let virt = BASE + page * 4096;
            assert!(
                space.map(virt, frame, read_write).is_ok(),
                "self-test user space: map"
            );
            assert_eq!(
                space.translate(virt),
                Some((address, read_write)),
                "self-test user space: translate"
            );
        }
        let spare = frames::allocate().expect("out of memory");
        let (error, spare) = space.map(BASE, spare, read_write).unwrap_err();
        assert_eq!(
            error,
            MapError::AlreadyMapped,
            "self-test user space: double map"
        );
        let (error, _spare) = space.map(USER_END, spare, read_write).unwrap_err();
        assert_eq!(
            error,
            MapError::BadAddress,
            "self-test user space: kernel half"
        );

        let read_only = UserAccess::READ;
        assert_eq!(
            space.protect(BASE, 4, read_only),
            3,
            "self-test user space: protect"
        );
        assert_eq!(
            space.translate(BASE).map(|(_, access)| access),
            Some(read_only)
        );
        // No access (PROT_NONE): the frame stays, only the permissions go, and come back.
        assert_eq!(space.protect(BASE, 1, UserAccess::NONE), 1);
        assert_eq!(
            space.translate(BASE).map(|(_, access)| access),
            Some(UserAccess::NONE),
            "self-test user space: no access"
        );
        assert_eq!(space.protect(BASE, 1, read_only), 1);
        assert!(space.write_bytes(BASE + 4094, &[1, 2, 3, 4]).is_ok());
        assert_eq!(
            space.write_bytes(BASE + 3 * 4096 - 2, &[1, 2, 3, 4]),
            Err(MapError::BadAddress),
            "self-test user space: write past the mapping"
        );

        let unmapped = space.unmap(BASE, 4);
        assert_eq!(unmapped.len(), 3, "self-test user space: unmap");
        for (page, frame) in unmapped.iter().enumerate() {
            // `write_bytes` above crossed from page 0 into page 1, changing its first bytes.
            let expected = match page {
                1 => 0x0C4A_0403,
                _ => 0x0C4A_0000 + page as u64,
            };
            assert_eq!(frame.read_u64(0), expected, "self-test user space");
        }
        assert_eq!(unmapped[0].read_u64(511) >> 48, 0x0201);
        assert_eq!(
            space.translate(BASE),
            None,
            "self-test user space: still mapped"
        );

        // Mapped again and left mapped: dropping the space must free it with its tables.
        let frame = frames::allocate().expect("out of memory");
        assert!(space.map(0x7FFF_FFFF_F000, frame, read_write).is_ok());
    }
    assert_eq!(
        frames::stats(),
        before,
        "self-test user space: frames leaked"
    );

    let others = ochra_frame::smp::cpu_count() - 1;
    let answered = ochra_frame::mm::tlb::broadcast_test();
    assert_eq!(
        answered, others,
        "self-test user space: TLB shootdown answers"
    );
    log::info!(
        "self-test user space: ok (map, protect, unmap, free; shootdown answered by {answered} CPUs)"
    );
}

/// Random numbers: the ChaCha20 block function against the test vector of RFC 8439
/// (section 2.3.2), then a sanity check of the generator's output.
#[cfg(feature = "selftest")]
fn check_random() {
    use ochra_frame::random;

    let key: [u32; 8] = core::array::from_fn(|index| {
        let byte = |offset: usize| (index * 4 + offset) as u32;
        byte(0) | byte(1) << 8 | byte(2) << 16 | byte(3) << 24
    });
    let block = random::chacha20_block(&key, 1, &[0x0900_0000, 0x4a00_0000, 0]);
    let expected_start = [0x10, 0xf1, 0xe7, 0xe4, 0xd1, 0x3b, 0x59, 0x15];
    let expected_end = [0xcb, 0xd0, 0x83, 0xe8, 0xa2, 0x50, 0x3c, 0x4e];
    assert_eq!(
        block[..8],
        expected_start,
        "self-test random: ChaCha20 test vector"
    );
    assert_eq!(
        block[56..],
        expected_end,
        "self-test random: ChaCha20 test vector"
    );

    let mut first = [0u8; 64];
    let mut second = [0u8; 64];
    random::fill(&mut first);
    random::fill(&mut second);
    assert!(
        first != [0; 64] && first != second,
        "self-test random: output"
    );
    log::info!("self-test random: ok (ChaCha20 test vector, output differs)");
}
