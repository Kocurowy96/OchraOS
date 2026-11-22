// SPDX-License-Identifier: GPL-2.0-only

//! The scheduler: which task runs next. Round-robin — every task gets one time slice (one
//! timer tick) in turn. All CPUs share one run queue, so a task may continue on a different
//! CPU after every tick. The mechanism (task stacks, the context switch, making sure two CPUs
//! never run the same task) is in `ochra_frame::task`.

use alloc::collections::VecDeque;
use alloc::sync::Arc;
use core::sync::atomic::{AtomicBool, Ordering};

use ochra_frame::arch::interrupts;
use ochra_frame::mm::user::UserSpace;
use ochra_frame::sync::IrqMutex;
use ochra_frame::task::{self, Task};

/// Tasks waiting for the CPU, in the order they will get it. The running task is not here.
static RUN_QUEUE: IrqMutex<VecDeque<Arc<Task>>> = IrqMutex::new(VecDeque::new());

/// Creates a user task (see `task::spawn_user`) and puts it at the end of the queue.
#[cfg_attr(
    not(feature = "selftest"),
    expect(
        dead_code,
        reason = "only the self-tests start user tasks until init (stage 2, P2)"
    )
)]
pub fn spawn_user(name: &'static str, space: Arc<UserSpace>, entry: u64, stack: u64) {
    RUN_QUEUE
        .lock()
        .push_back(task::spawn_user(name, space, entry, stack));
}

/// Set by the boot CPU once the first tasks are queued; the other CPUs wait for it.
static READY: AtomicBool = AtomicBool::new(false);

/// Creates a task and puts it at the end of the queue.
pub fn spawn(name: &'static str, entry: fn() -> !) {
    RUN_QUEUE.lock().push_back(task::spawn(name, entry));
}

/// Starts scheduling on this CPU: runs the first queued task, and the caller's code never
/// continues. The boot CPU calls it once the first tasks are queued; the other CPUs call it
/// right away and wait (taking interrupts) until the boot CPU gets here.
pub fn start(boot_cpu: bool) -> ! {
    if boot_cpu {
        READY.store(true, Ordering::Release);
    }
    while !READY.load(Ordering::Acquire) {
        interrupts::wait();
    }
    // Interrupts off until the task runs: a tick in between would find no current task and
    // do nothing, but the popped task must not wait outside the queue for long.
    interrupts::disable();
    let first = RUN_QUEUE
        .lock()
        .pop_front()
        .expect("fewer tasks than CPUs (every CPU needs an idle task)");
    task::start(first)
}

/// Gives the CPU to the next task in the queue, if there is one. Called on every timer tick.
pub fn schedule() {
    // Interrupts stay off from looking at the current task until the switch: a timer tick in
    // between would queue the current task twice.
    let were_enabled = interrupts::disable();
    if let Some(current) = task::current() {
        let next = {
            let mut queue = RUN_QUEUE.lock();
            queue.pop_front().inspect(|_| queue.push_back(current))
        };
        // The queue lock is released before switching: the next task will need it.
        if let Some(next) = next {
            task::switch_to(next);
        }
    }
    if were_enabled {
        interrupts::enable();
    }
}
