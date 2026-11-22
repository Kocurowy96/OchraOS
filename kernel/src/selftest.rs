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
        _ => log::error!("unknown self-test {name:?}"),
    }
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
    check_frames();
    check_heap();
    check_interrupts();
    check_timer();
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
    assert_eq!(
        after.free_blocks, 1,
        "self-test heap: free blocks not merged"
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
    use ochra_frame::arch::interrupts;

    let start = crate::time::ticks();
    for _ in 0..1000 {
        if crate::time::ticks() >= start + 5 {
            break;
        }
        interrupts::wait();
    }
    let seen = crate::time::ticks() - start;
    assert!(seen >= 5, "self-test timer: only {seen} ticks");
    log::info!("self-test timer: ok ({seen} ticks)");
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
}

/// Without self-tests there are no test tasks.
#[cfg(not(feature = "selftest"))]
pub fn spawn_tasks() {}

/// Whether the tasks from [`spawn_tasks`] are done.
pub fn tasks_finished() -> bool {
    #[cfg(feature = "selftest")]
    return TASKS_DONE.load(core::sync::atomic::Ordering::Acquire) == 2;
    #[cfg(not(feature = "selftest"))]
    true
}
